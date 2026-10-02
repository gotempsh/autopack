//! Ruby provider.

use autopack_core::plan::{Command, GeneratedValue, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::{foreign_manifest, procfile_web_command, read_version_file};
use crate::version::{Requirement, Version};

/// Ruby version used when the project does not pin one.
const DEFAULT_RUBY_VERSION: &str = "3.3";

/// Creates the database when missing and runs pending migrations.
const RAILS_DB_PREPARE: &str = "bundle exec rails db:prepare";

/// Where bundler installs gems, inside the app so the runtime image gets them.
const BUNDLE_PATH: &str = "/app/vendor/bundle";

/// Builds Ruby applications, including Rails.
pub struct RubyProvider;

impl Provider for RubyProvider {
    fn id(&self) -> &'static str {
        "ruby"
    }

    fn display_name(&self) -> &'static str {
        "Ruby"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        if app.has_any_file(["Gemfile", "config.ru", ".ruby-version"]) {
            return Ok(true);
        }
        Ok(foreign_manifest(app, &["Gemfile"]).is_none() && app.has_match("**/*.rb"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let (version, source) = ruby_version(ctx.app)?;
        ctx.add_metadata("rubyVersion", &version);
        ctx.add_metadata("rubyVersionSource", source);

        // mise builds Ruby from source, which adds five to ten minutes to a
        // cold build. The official image ships a prebuilt interpreter.
        let image = format!("ruby:{version}-slim");
        ctx.set_base_image(&image);
        ctx.set_runtime_base_image(&image);
        ctx.add_metadata("image", &image);
        ctx.set_base_image_runtimes(["ruby"]);

        // Native gem extensions need a toolchain; psych needs libyaml.
        ctx.build_apt_packages.extend(
            ["build-essential", "libyaml-dev", "pkg-config", "git"]
                .into_iter()
                .map(String::from),
        );

        let mut gemfile = ctx.app.read_file_opt("Gemfile")?.unwrap_or_default();
        // The lockfile names transitive gems too, and a native extension is
        // just as likely to arrive through a dependency as directly.
        if let Some(lock) = ctx.app.read_file_opt("Gemfile.lock")? {
            gemfile.push('\n');
            gemfile.push_str(&lock);
        }

        let (build_packages, runtime_packages) =
            crate::native::required_packages(&gemfile, crate::native::RUBY);
        if !build_packages.is_empty() || !runtime_packages.is_empty() {
            ctx.add_metadata(
                "systemPackages",
                format!(
                    "build: [{}], runtime: [{}]",
                    build_packages.join(" "),
                    runtime_packages.join(" ")
                ),
            );
        }
        ctx.build_apt_packages.extend(build_packages);
        ctx.deploy_apt_packages.extend(runtime_packages);
        let is_rails = gemfile.contains("rails") || ctx.app.has_file("bin/rails");
        if is_rails {
            ctx.add_metadata("framework", "rails");
        }

        self.plan_install(ctx)?;
        self.plan_build(ctx, is_rails)?;

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([APP_DIR]));
        ctx.add_deploy_variable("BUNDLE_PATH", BUNDLE_PATH);
        ctx.add_deploy_variable("BUNDLE_WITHOUT", "development:test");
        ctx.add_deploy_variable("RAILS_ENV", "production");
        ctx.add_deploy_variable("RACK_ENV", "production");
        // Rails buffers logs to a file by default, which is invisible in a
        // container.
        ctx.add_deploy_variable("RAILS_LOG_TO_STDOUT", "1");
        if is_rails {
            // Rails before 7.1 only serves `public/` (precompiled assets)
            // when told to; there is no nginx in front of the container.
            ctx.add_deploy_variable("RAILS_SERVE_STATIC_FILES", "1");
            // Rails refuses to boot in production without a secret key base,
            // and it must stay the same across deploys or every session and
            // signed cookie is invalidated. With a master key the app reads
            // it from its encrypted credentials, and a generated
            // SECRET_KEY_BASE would silently take precedence over that one.
            ctx.require_generated_variable_unless(
                "SECRET_KEY_BASE",
                GeneratedValue::HexSecret { bytes: 64 },
                &["RAILS_MASTER_KEY"],
            );
            if let Some(database_yml) = ctx.app.read_file_opt("config/database.yml")? {
                match production_sqlite(&database_yml) {
                    Some(ProductionSqlite::NoFile) => ctx.require_persistent_path(
                        RAILS_STORAGE,
                        "Rails is configured for SQLite in production, but \
                         config/database.yml gives the production database no file, so \
                         the app cannot open it. Point production at a file under \
                         storage/ and keep that directory on persistent storage, or use \
                         a database server (link Postgres and add the pg gem).",
                        &[],
                    ),
                    Some(ProductionSqlite::File) => ctx.require_persistent_path(
                        RAILS_STORAGE,
                        "The production database is a SQLite file inside the container; \
                         without persistent storage its data is lost on every redeploy.",
                        &["DATABASE_URL"],
                    ),
                    None => {}
                }
            }
            let declares_release = autopack_core::Procfile::load(ctx.app)?
                .is_some_and(|procfile| procfile.release().is_some());
            if ctx.app.has_file("config/database.yml") && !declares_release {
                ctx.add_task("release", RAILS_DB_PREPARE);
            }
        }

        if let Some(command) = start_command(ctx.app, is_rails)? {
            ctx.set_start_command(command);
        }
        Ok(())
    }
}

impl RubyProvider {
    fn plan_install(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        if !ctx.app.has_file("Gemfile") {
            return Ok(());
        }

        let cache = ctx.shared_cache("bundler", "/cache/bundler");
        let manifests: Vec<&str> = ["Gemfile", "Gemfile.lock", ".ruby-version"]
            .into_iter()
            .filter(|file| ctx.app.has_file(file))
            .collect();
        // `--deployment` refuses to run when the lockfile is stale, which is
        // the correct failure for a build but wrong without a lockfile at all.
        let deployment = ctx.app.has_file("Gemfile.lock");

        let needs_platform = deployment
            && !lock_covers_linux(&ctx.app.read_file_opt("Gemfile.lock")?.unwrap_or_default());

        let step = ctx.step(steps::INSTALL);
        step.add_input(Layer::local().including(manifests));
        step.add_variable("BUNDLE_PATH", BUNDLE_PATH);
        step.add_variable("BUNDLE_WITHOUT", "development:test");
        step.add_variable("BUNDLE_JOBS", "4");
        // Only Bundler's own index cache is shared between builds. Never
        // `BUNDLE_CACHE_PATH`: that is the app's packaged gem directory
        // (`vendor/cache`), and in deployment mode Bundler treats a non-empty
        // one as the complete source of gems, so another app's cached gems
        // fail the build with "Could not find ... in any of the sources".
        // Nor `BUNDLE_GLOBAL_GEM_CACHE`: it restores compiled extensions from
        // the cache but only their extension directory, so a gem that builds
        // a library into its own `lib/` (sassc's `libsass.so`) loses it.
        step.add_variable("BUNDLE_USER_CACHE", "/cache/bundler");
        if deployment {
            step.add_variable("BUNDLE_DEPLOYMENT", "1");
        }
        step.add_cache(cache);
        if needs_platform {
            // A lockfile resolved on a Mac lists only `arm64-darwin-*`, and
            // Bundler refuses to install it anywhere else: "Your bundle only
            // supports platforms [...] but your local platform is
            // aarch64-linux". Adding the build platform is the fix Bundler
            // itself suggests; it re-resolves platform-specific gems without
            // changing any version. Deployment mode freezes the lockfile, so
            // it is lifted for this one command.
            // The build step copies the source over this one, lockfile
            // included, so the corrected lockfile is kept aside for it.
            step.add_command(Command::shell(format!(
                "BUNDLE_DEPLOYMENT=false BUNDLE_FROZEN=false \
                 bundle lock --add-platform \"$(ruby -e 'print Gem::Platform.local')\" \
                 && cp Gemfile.lock {PLATFORM_LOCK}"
            )));
        }
        step.add_command(Command::shell("bundle install"));
        Ok(())
    }

    fn plan_build(&self, ctx: &mut BuildContext<'_>, is_rails: bool) -> Result<()> {
        let precompile =
            is_rails && (ctx.app.has_dir("app/assets") || ctx.app.has_dir("app/javascript"));
        let restores_lock = ctx.has_step(steps::INSTALL)
            && ctx.app.has_file("Gemfile.lock")
            && !lock_covers_linux(&ctx.app.read_file_opt("Gemfile.lock")?.unwrap_or_default());

        let base = if ctx.has_step(steps::INSTALL) {
            Layer::step(steps::INSTALL)
        } else {
            Layer::step(steps::PACKAGES)
        };

        if restores_lock {
            ctx.step(steps::BUILD)
                .add_command(Command::shell(format!("cp {PLATFORM_LOCK} Gemfile.lock")));
        }
        // jsbundling, cssbundling and Webpacker all run Node from inside
        // `assets:precompile`.
        if precompile {
            crate::node::plan_front_end(ctx, steps::BUILD)?;
        }

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![base, Layer::local()];
        step.add_variable("BUNDLE_PATH", BUNDLE_PATH);
        step.add_variable("BUNDLE_WITHOUT", "development:test");
        step.add_variable("RAILS_ENV", "production");

        if precompile {
            // Rails refuses to boot without a secret; asset compilation does
            // not use it, so a placeholder keeps the build from needing the
            // production credential.
            step.add_command(Command::shell(
                "SECRET_KEY_BASE=${SECRET_KEY_BASE:-autopack-precompile} \
                 bundle exec rails assets:precompile",
            ));
            ctx.add_metadata("assets", "rails assets:precompile");
        }
        Ok(())
    }
}

/// Where Rails keeps SQLite databases and Active Storage's local files.
const RAILS_STORAGE: &str = "/app/storage";

/// How `config/database.yml` sets up a SQLite production database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProductionSqlite {
    /// SQLite, with a database file.
    File,
    /// SQLite with no file at all: the Rails 8.1 template comments the
    /// production paths out for the operator to choose.
    NoFile,
}

/// Whether production uses SQLite, read from the text of `database.yml`.
///
/// The file is ERB and YAML with merge keys, so it is read line by line: the
/// `production:` block decides, falling back to the adapter anywhere in the
/// file for blocks that only merge `*default`. A `url:` or a server adapter in
/// production means a database server.
fn production_sqlite(database_yml: &str) -> Option<ProductionSqlite> {
    let mut production = Vec::new();
    let mut in_production = false;
    for line in database_yml.lines() {
        let top_level = !line.starts_with([' ', '\t']) && !line.trim().is_empty();
        if top_level && !line.trim_start().starts_with('#') {
            in_production = line.trim_end() == "production:";
            continue;
        }
        if in_production {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                production.push(trimmed);
            }
        }
    }
    if production.is_empty() {
        return None;
    }
    let value = |key: &str| {
        production
            .iter()
            .filter_map(|line| line.strip_prefix(key))
            .map(str::trim)
            .collect::<Vec<_>>()
    };
    if !value("url:").is_empty() {
        return None;
    }
    let adapters = value("adapter:");
    let sqlite = if adapters.is_empty() {
        database_yml
            .lines()
            .map(str::trim)
            .any(|line| line == "adapter: sqlite3")
    } else {
        adapters.iter().all(|adapter| *adapter == "sqlite3")
    };
    if !sqlite {
        return None;
    }
    Some(if value("database:").is_empty() {
        ProductionSqlite::NoFile
    } else {
        ProductionSqlite::File
    })
}

/// Where the install step keeps a lockfile it added the build platform to.
const PLATFORM_LOCK: &str = "/tmp/autopack-Gemfile.lock";

/// `major.minor` lines the official `ruby:<version>-slim` image publishes.
const RUBY_MINORS: &[&str] = &["2.7", "3.0", "3.1", "3.2", "3.3", "3.4", "4.0"];

/// Newest line an open-ended range (`>= 3.2`) resolves to while it admits
/// one. Jumping a range to a brand-new major is more likely to break an app
/// than to help it.
const RANGE_CEILING: &str = "3.4";

/// The Ruby version to build with: an exact `x.y.z` when the project pins
/// one, otherwise a `major.minor` line.
///
/// Bundler refuses to run when the Gemfile's `ruby` directive names a patch
/// the interpreter does not match ("Your Ruby version is 3.1.7, but your
/// Gemfile specified 3.1.2"), so an exact pin must become the exact image tag.
/// Official images publish every patch tag and keep old ones. Ranges resolve
/// to the newest admitted line, not the lower bound: the lockfile was most
/// likely resolved on a recent interpreter.
fn ruby_version(app: &App) -> Result<(String, &'static str)> {
    if let Some(version) = read_version_file(app, ".ruby-version")? {
        let version = version.trim_start_matches("ruby-");
        if let Some(requirement) = Requirement::parse(version) {
            if let Some(exact) = requirement.exact() {
                return Ok((exact.to_string(), ".ruby-version"));
            }
            if let Some(minor) = newest_ruby(&requirement) {
                return Ok((minor.to_string(), ".ruby-version"));
            }
        }
    }

    let locked = match app.read_file_opt("Gemfile.lock")? {
        Some(lock) => locked_ruby_version(&lock),
        None => None,
    };

    if let Some(gemfile) = app.read_file_opt("Gemfile")? {
        if let Some(requirement) = gemfile_ruby_requirement(&gemfile) {
            if let Some(exact) = requirement.exact() {
                return Ok((exact.to_string(), "Gemfile"));
            }
            // The lockfile records the interpreter the app was last resolved
            // on; prefer it whenever the Gemfile's range allows it.
            if let Some(locked) = &locked {
                if requirement.admits(locked) {
                    return Ok((locked.clone(), "Gemfile.lock"));
                }
            }
            if let Some(minor) = newest_ruby(&requirement) {
                return Ok((minor.to_string(), "Gemfile"));
            }
        }
    }

    if let Some(locked) = locked {
        return Ok((locked, "Gemfile.lock"));
    }

    Ok((DEFAULT_RUBY_VERSION.to_string(), "autopack default"))
}

/// The newest published line `requirement` admits, preferring lines up to
/// [`RANGE_CEILING`].
fn newest_ruby(requirement: &Requirement) -> Option<&'static str> {
    let ceiling = Version::parse(RANGE_CEILING).map(|v| (v.major, v.minor));
    let settled: Vec<&'static str> = RUBY_MINORS
        .iter()
        .copied()
        .filter(|minor| Version::parse(minor).map(|v| (v.major, v.minor)) <= ceiling)
        .collect();
    requirement
        .newest(&settled)
        .or_else(|| requirement.newest(RUBY_MINORS))
}

/// The requirement in the Gemfile's `ruby` directive.
///
/// Handles `ruby '3.1.2'`, `ruby "~> 3.2"` and the multi-argument form
/// `ruby '>= 3.2', '< 4.0'`. Keyword arguments (`engine:`, `file:`) are
/// ignored; `file:` points at `.ruby-version`, which is read first anyway.
fn gemfile_ruby_requirement(gemfile: &str) -> Option<Requirement> {
    gemfile.lines().find_map(|line| {
        let line = line.trim();
        let rest = line
            .strip_prefix("ruby ")
            .or_else(|| line.strip_prefix("ruby("))?;
        if rest.trim_start().starts_with("file:") {
            return None;
        }
        let arguments: Vec<&str> = quoted_strings(rest);
        if arguments.is_empty() {
            return None;
        }
        Requirement::parse(&arguments.join(", "))
    })
}

/// The contents of every '…' or "…" literal in `text`, in order, stopping at
/// the first keyword argument.
fn quoted_strings(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(['\'', '"']) {
        // A `key:` before the next literal ends the positional arguments.
        if rest[..start].contains(':') {
            break;
        }
        let quote = &rest[start..start + 1];
        let after = &rest[start + 1..];
        let Some(end) = after.find(quote) else {
            break;
        };
        found.push(&after[..end]);
        rest = &after[end + 1..];
    }
    found
}

/// Whether a lockfile's `PLATFORMS` section already lets any Linux build
/// install it: the generic `ruby` platform does, and so do both Linux CPU
/// architectures together. A single Linux architecture is not enough — the
/// image may be built for the other one.
fn lock_covers_linux(lock: &str) -> bool {
    let mut lines = lock.lines();
    if lines.find(|line| line.trim() == "PLATFORMS").is_none() {
        // No section at all: Bundler treats it as `ruby`.
        return true;
    }
    let platforms: Vec<&str> = lines
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .collect();
    platforms.contains(&"ruby")
        || (platforms.iter().any(|p| p.starts_with("x86_64-linux"))
            && platforms.iter().any(|p| p.starts_with("aarch64-linux")))
}

/// The interpreter recorded in Gemfile.lock's `RUBY VERSION` section.
fn locked_ruby_version(lock: &str) -> Option<String> {
    let mut lines = lock.lines();
    lines.find(|line| line.trim() == "RUBY VERSION")?;
    let line = lines.find(|line| !line.trim().is_empty())?;
    let version = line.trim().strip_prefix("ruby ")?;
    let version = Version::parse(version)?;
    let patch = version.patch?;
    Some(format!("{}.{}.{patch}", version.major, version.minor))
}

fn start_command(app: &App, is_rails: bool) -> Result<Option<String>> {
    if let Some(command) = procfile_web_command(app)? {
        return Ok(Some(command));
    }

    if is_rails {
        let server = "bundle exec rails server -b 0.0.0.0 -p ${PORT:-3000}";
        if !app.has_file("config/database.yml") {
            return Ok(Some(server.to_string()));
        }
        // What Rails' own generated Dockerfile entrypoint does: create the
        // database if it does not exist and run pending migrations. A new
        // app's production database is SQLite inside the container, which
        // only exists once this has run. A failure is reported but does not
        // stop the server, so the app's own error page explains it.
        return Ok(Some(format!(
            "if [ -z \"${{AUTOPACK_NO_MIGRATE:-}}\" ]; then \
               {RAILS_DB_PREPARE} || echo 'autopack: rails db:prepare failed; set AUTOPACK_NO_MIGRATE=1 to skip it' >&2; \
             fi; exec {server}"
        )));
    }

    if app.has_file("config.ru") {
        return Ok(Some(
            "bundle exec rackup -o 0.0.0.0 -p ${PORT:-3000}".to_string(),
        ));
    }

    Ok(["main.rb", "app.rb", "server.rb"]
        .into_iter()
        .find(|entry| app.has_file(entry))
        .map(|entry| format!("ruby {entry}")))
}

#[cfg(test)]
mod tests {
    use super::PLATFORM_LOCK;
    use crate::test_support::{plan_for, write_app};

    #[test]
    fn rails_assets_get_node_and_the_apps_javascript_dependencies() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "source 'https://rubygems.org'\ngem 'rails'\n"),
            (
                "Gemfile.lock",
                "GEM\n  specs:\n    rails (6.1.7)\n\nPLATFORMS\n  x86_64-linux\n",
            ),
            ("config/application.rb", ""),
            ("app/javascript/packs/application.js", ""),
            (
                "package.json",
                r#"{"dependencies":{"@rails/webpacker":"^5.4.0"}}"#,
            ),
            ("yarn.lock", ""),
        ]);
        let analysis = plan_for(&app);
        let build = analysis.plan.step("build").unwrap();
        let commands: Vec<String> = build
            .commands
            .iter()
            .map(|c| c.display_name().to_string())
            .collect();

        let install = commands
            .iter()
            .position(|c| c.starts_with("yarn install"))
            .expect("yarn install in the build step");
        let precompile = commands
            .iter()
            .position(|c| c.contains("assets:precompile"))
            .unwrap();
        assert!(install < precompile, "{commands:?}");
        assert!(analysis.packages.iter().any(|(tool, _)| tool == "node"));
        // Webpacker 5 is webpack 4, which needs MD4 on a current Node.
        assert_eq!(build.variables["NODE_OPTIONS"], "--openssl-legacy-provider");
    }

    #[test]
    fn a_lockfile_fixed_for_linux_survives_the_source_copy() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "source 'https://rubygems.org'\ngem 'rack'\n"),
            (
                "Gemfile.lock",
                "GEM\n  specs:\n    rack (3.0.0)\n\nPLATFORMS\n  arm64-darwin-21\n",
            ),
            ("config.ru", "run ->(env) { [200, {}, ['ok']] }\n"),
        ]);
        let plan = plan_for(&app).plan;
        let names = |step: &str| -> Vec<String> {
            plan.step(step)
                .unwrap()
                .commands
                .iter()
                .map(|c| c.display_name().to_string())
                .collect()
        };
        let install = names("install");
        let build = names("build");

        assert!(install
            .iter()
            .any(|c| c.contains("--add-platform") && c.ends_with(PLATFORM_LOCK)));
        assert_eq!(
            build.first(),
            Some(&format!("cp {PLATFORM_LOCK} Gemfile.lock"))
        );
    }

    #[test]
    fn a_master_key_makes_the_generated_secret_unnecessary() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "source 'https://rubygems.org'\ngem 'rails'\n"),
            ("config/application.rb", ""),
        ]);
        let deploy = plan_for(&app).plan.deploy;
        assert_eq!(
            deploy.generated_variables["SECRET_KEY_BASE"].unless_set,
            vec!["RAILS_MASTER_KEY".to_string()]
        );
    }

    #[test]
    fn sqlite_production_databases_are_recognised() {
        use super::{production_sqlite, ProductionSqlite};

        // The Rails 8.1 template: production paths commented out.
        let rails_81 = "default: &default\n  adapter: sqlite3\n  timeout: 5000\n\n\
            production:\n  primary:\n    <<: *default\n    \
            # database: path/to/persistent/storage/production.sqlite3\n  cache:\n    \
            <<: *default\n";
        assert_eq!(production_sqlite(rails_81), Some(ProductionSqlite::NoFile));

        let rails_80 = "default: &default\n  adapter: sqlite3\n\nproduction:\n  \
            primary:\n    <<: *default\n    database: storage/production.sqlite3\n";
        assert_eq!(production_sqlite(rails_80), Some(ProductionSqlite::File));

        let postgres = "default: &default\n  adapter: sqlite3\n\nproduction:\n  \
            <<: *default\n  adapter: postgresql\n  database: app\n";
        assert_eq!(production_sqlite(postgres), None);

        let url = "production:\n  url: <%= ENV['DATABASE_URL'] %>\n";
        assert_eq!(production_sqlite(url), None);

        assert_eq!(
            production_sqlite("development:\n  adapter: sqlite3\n"),
            None
        );
    }

    #[test]
    fn a_sqlite_production_database_needs_persistent_storage() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "source 'https://rubygems.org'\ngem 'rails'\n"),
            ("config/application.rb", ""),
            (
                "config/database.yml",
                "default: &default\n  adapter: sqlite3\n\nproduction:\n  <<: *default\n  \
                 database: storage/production.sqlite3\n",
            ),
        ]);
        let deploy = plan_for(&app).plan.deploy;
        assert_eq!(deploy.persistent_paths.len(), 1);
        assert_eq!(deploy.persistent_paths[0].path, "/app/storage");
        assert_eq!(deploy.persistent_paths[0].unless_set, vec!["DATABASE_URL"]);
    }

    #[test]
    fn a_postgres_rails_app_needs_no_persistent_storage() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "source 'https://rubygems.org'\ngem 'rails'\n"),
            ("config/application.rb", ""),
            (
                "config/database.yml",
                "production:\n  adapter: postgresql\n  url: <%= ENV['DATABASE_URL'] %>\n",
            ),
        ]);
        assert!(plan_for(&app).plan.deploy.persistent_paths.is_empty());
    }

    #[test]
    fn the_shared_gem_cache_is_never_the_apps_vendor_cache() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "source 'https://rubygems.org'\ngem 'rack'\n"),
            (
                "Gemfile.lock",
                "GEM\n  specs:\n    rack (3.0.0)\n\nPLATFORMS\n  x86_64-linux\n",
            ),
            ("config.ru", "run ->(env) { [200, {}, ['ok']] }\n"),
        ]);
        let plan = plan_for(&app).plan;
        let install = plan.step("install").unwrap();

        // In deployment mode Bundler reads BUNDLE_CACHE_PATH as the app's own
        // complete gem package; sharing it across apps breaks the next build.
        assert!(!install.variables.contains_key("BUNDLE_CACHE_PATH"));
        assert_eq!(install.variables["BUNDLE_USER_CACHE"], "/cache/bundler");
        // Restoring cached native extensions drops libraries gems build into
        // their own lib/ directory.
        assert!(!install.variables.contains_key("BUNDLE_GLOBAL_GEM_CACHE"));
    }

    #[test]
    fn rack_apps_use_rackup() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "source 'https://rubygems.org'\ngem 'sinatra'\n"),
            ("Gemfile.lock", ""),
            ("config.ru", "run Sinatra::Application"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "ruby");
        assert_eq!(analysis.metadata["image"], "ruby:3.3-slim");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("bundle exec rackup -o 0.0.0.0 -p ${PORT:-3000}")
        );
        // No mise runtime is needed: the base image already has Ruby.
        assert!(analysis.packages.is_empty());
    }

    #[test]
    fn rails_apps_precompile_assets_and_boot_the_server() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "gem 'rails', '~> 7.1'\n"),
            ("Gemfile.lock", ""),
            ("app/assets/config/manifest.js", ""),
            ("config.ru", ""),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.metadata["framework"], "rails");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("assets:precompile"));
        assert!(analysis
            .plan
            .deploy
            .start_command
            .as_deref()
            .unwrap()
            .ends_with("bundle exec rails server -b 0.0.0.0 -p ${PORT:-3000}"));
    }

    #[test]
    fn ruby_version_file_selects_the_exact_image() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "gem 'sinatra'"),
            (".ruby-version", "3.2.4\n"),
            ("config.ru", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["image"], "ruby:3.2.4-slim");

        let (_dir, app) = write_app(&[
            ("Gemfile", "gem 'sinatra'"),
            (".ruby-version", "ruby-3.3\n"),
            ("config.ru", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["image"], "ruby:3.3-slim");
    }

    #[test]
    fn an_exact_gemfile_pin_selects_the_exact_patch_image() {
        // Bundler refuses to install when the interpreter's patch differs from
        // the Gemfile's `ruby` directive, so the minor line is not enough.
        let (_dir, app) = write_app(&[
            (
                "Gemfile",
                "source 'https://rubygems.org'\nruby '3.1.2'\ngem 'sinatra'\n",
            ),
            ("Gemfile.lock", "RUBY VERSION\n   ruby 3.1.2p20\n"),
            ("config.ru", ""),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["image"], "ruby:3.1.2-slim");
        assert_eq!(analysis.metadata["rubyVersionSource"], "Gemfile");
    }

    #[test]
    fn a_gemfile_range_prefers_the_locked_interpreter() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "ruby '>= 3.2', '< 4.0'\ngem 'rails'\n"),
            (
                "Gemfile.lock",
                "GEM\n  specs:\n\nRUBY VERSION\n   ruby 3.4.9p0\n\nBUNDLED WITH\n   2.6.2\n",
            ),
            ("config.ru", ""),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["image"], "ruby:3.4.9-slim");
        assert_eq!(analysis.metadata["rubyVersionSource"], "Gemfile.lock");
    }

    #[test]
    fn a_gemfile_range_without_a_lock_takes_the_newest_admitted_line() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "ruby '>= 3.1', '< 3.4'\ngem 'sinatra'\n"),
            ("config.ru", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["image"], "ruby:3.3-slim");

        // An open-ended range stays on a settled line rather than jumping to a
        // new major.
        let (_dir, app) = write_app(&[
            ("Gemfile", "ruby '>= 3.2'\ngem 'sinatra'\n"),
            ("config.ru", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["image"], "ruby:3.4-slim");

        // A range that only admits the new major still gets it.
        let (_dir, app) = write_app(&[
            ("Gemfile", "ruby '~> 4.0'\ngem 'sinatra'\n"),
            ("config.ru", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["image"], "ruby:4.0-slim");
    }

    #[test]
    fn a_locked_interpreter_is_used_when_the_gemfile_names_none() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "gem 'sinatra'\n"),
            ("Gemfile.lock", "RUBY VERSION\n   ruby 3.3.6p108\n"),
            ("config.ru", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["image"], "ruby:3.3.6-slim");
    }

    #[test]
    fn gemfile_ruby_directive_parsing() {
        use super::{gemfile_ruby_requirement, locked_ruby_version};
        assert_eq!(
            gemfile_ruby_requirement("ruby \"3.2.2\"\n")
                .unwrap()
                .exact(),
            Some("3.2.2")
        );
        assert!(gemfile_ruby_requirement("ruby file: '.ruby-version'\n").is_none());
        assert_eq!(
            gemfile_ruby_requirement("ruby '3.3.0', engine: 'jruby', engine_version: '9.4.0.0'")
                .unwrap()
                .exact(),
            Some("3.3.0")
        );
        assert!(gemfile_ruby_requirement("gem 'ruby-progressbar'\n").is_none());
        assert_eq!(
            locked_ruby_version("RUBY VERSION\n  ruby 3.0.0p0\n").as_deref(),
            Some("3.0.0")
        );
        assert_eq!(locked_ruby_version("BUNDLED WITH\n  2.4.1\n"), None);
    }

    #[test]
    fn deployment_mode_only_with_a_lockfile() {
        let (_dir, app) = write_app(&[("Gemfile", "gem 'sinatra'"), ("config.ru", "")]);
        let analysis = plan_for(&app);
        let install = analysis.plan.step("install").unwrap();
        assert!(!install.variables.contains_key("BUNDLE_DEPLOYMENT"));
    }

    #[test]
    fn a_lockfile_without_a_linux_platform_gets_the_build_platform_added() {
        let install = |lock: &str| {
            let (_dir, app) = write_app(&[
                ("Gemfile", "gem 'sinatra'\n"),
                ("Gemfile.lock", lock),
                ("config.ru", ""),
            ]);
            plan_for(&app)
                .plan
                .step("install")
                .unwrap()
                .commands
                .iter()
                .map(|command| command.display_name())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mac_only = "GEM\n  specs:\n\nPLATFORMS\n  arm64-darwin-21\n\nDEPENDENCIES\n  sinatra\n";
        assert!(install(mac_only).contains("bundle lock --add-platform"));
        let one_linux = "PLATFORMS\n  x86_64-darwin-20\n  x86_64-linux\n\n";
        assert!(install(one_linux).contains("bundle lock --add-platform"));
        let generic = "PLATFORMS\n  ruby\n  arm64-darwin-23\n\n";
        assert!(!install(generic).contains("bundle lock --add-platform"));
        let both = "PLATFORMS\n  aarch64-linux\n  x86_64-linux-gnu\n\n";
        assert!(!install(both).contains("bundle lock --add-platform"));
    }

    #[test]
    fn rails_apps_get_a_generated_secret_and_prepare_their_database() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "gem 'rails', '~> 8.0'\n"),
            ("Gemfile.lock", ""),
            ("config.ru", ""),
            ("config/database.yml", "production:\n  adapter: sqlite3\n"),
        ]);
        let analysis = plan_for(&app);
        let deploy = &analysis.plan.deploy;
        assert_eq!(
            deploy
                .generated_variables
                .get("SECRET_KEY_BASE")
                .map(|v| &v.value),
            Some(&autopack_core::plan::GeneratedValue::HexSecret { bytes: 64 })
        );
        assert_eq!(deploy.variables["RAILS_SERVE_STATIC_FILES"], "1");
        assert_eq!(deploy.tasks["release"], "bundle exec rails db:prepare");
        let start = deploy.start_command.as_deref().unwrap();
        assert!(start.contains("bundle exec rails db:prepare ||"), "{start}");
        assert!(start.contains("AUTOPACK_NO_MIGRATE"), "{start}");
    }

    #[test]
    fn rack_apps_declare_no_rails_secret() {
        let (_dir, app) = write_app(&[("Gemfile", "gem 'sinatra'\n"), ("config.ru", "")]);
        assert!(plan_for(&app).plan.deploy.generated_variables.is_empty());
    }
}
