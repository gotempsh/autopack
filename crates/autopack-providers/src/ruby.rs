//! Ruby provider.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::{foreign_manifest, procfile_web_command, read_version_file};

/// Ruby version used when the project does not pin one.
const DEFAULT_RUBY_VERSION: &str = "3.3";

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

        let step = ctx.step(steps::INSTALL);
        step.add_input(Layer::local().including(manifests));
        step.add_variable("BUNDLE_PATH", BUNDLE_PATH);
        step.add_variable("BUNDLE_WITHOUT", "development:test");
        step.add_variable("BUNDLE_JOBS", "4");
        step.add_variable("BUNDLE_CACHE_PATH", "/cache/bundler");
        if deployment {
            step.add_variable("BUNDLE_DEPLOYMENT", "1");
        }
        step.add_cache(cache);
        step.add_command(Command::shell("bundle install"));
        Ok(())
    }

    fn plan_build(&self, ctx: &mut BuildContext<'_>, is_rails: bool) -> Result<()> {
        let precompile =
            is_rails && (ctx.app.has_dir("app/assets") || ctx.app.has_dir("app/javascript"));

        let base = if ctx.has_step(steps::INSTALL) {
            Layer::step(steps::INSTALL)
        } else {
            Layer::step(steps::PACKAGES)
        };

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

/// The Ruby version, narrowed to `major.minor` so the image tag exists.
///
/// Official images publish `ruby:3.3-slim` and `ruby:3.3.6-slim`, but a
/// `.ruby-version` of `3.3.6` on a machine where only `3.3.7` was ever
/// published would fail to pull. `major.minor` always resolves.
fn ruby_version(app: &App) -> Result<(String, &'static str)> {
    if let Some(version) = read_version_file(app, ".ruby-version")? {
        if let Some(version) = major_minor(&version) {
            return Ok((version, ".ruby-version"));
        }
    }

    if let Some(gemfile) = app.read_file_opt("Gemfile")? {
        for line in gemfile.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("ruby ") {
                let value = rest.trim().trim_matches(['"', '\'', ' ']);
                if let Some(version) = major_minor(value) {
                    return Ok((version, "Gemfile"));
                }
            }
        }
    }

    Ok((DEFAULT_RUBY_VERSION.to_string(), "autopack default"))
}

fn major_minor(version: &str) -> Option<String> {
    let cleaned = version.trim().trim_start_matches(['~', '>', '=', '^', ' ']);
    let mut parts = cleaned.split('.');
    let major = parts.next()?.trim();
    let minor = parts.next().unwrap_or("0").trim();
    if major.is_empty() || !major.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let minor: String = minor.chars().take_while(char::is_ascii_digit).collect();
    if minor.is_empty() {
        return None;
    }
    Some(format!("{major}.{minor}"))
}

fn start_command(app: &App, is_rails: bool) -> Result<Option<String>> {
    if let Some(command) = procfile_web_command(app)? {
        return Ok(Some(command));
    }

    if is_rails {
        return Ok(Some(
            "bundle exec rails server -b 0.0.0.0 -p ${PORT:-3000}".to_string(),
        ));
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
    use crate::test_support::{plan_for, write_app};

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
            .starts_with("bundle exec rails server"));
    }

    #[test]
    fn ruby_version_file_selects_the_image() {
        let (_dir, app) = write_app(&[
            ("Gemfile", "gem 'sinatra'"),
            (".ruby-version", "3.2.4\n"),
            ("config.ru", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["image"], "ruby:3.2-slim");
    }

    #[test]
    fn deployment_mode_only_with_a_lockfile() {
        let (_dir, app) = write_app(&[("Gemfile", "gem 'sinatra'"), ("config.ru", "")]);
        let analysis = plan_for(&app);
        let install = analysis.plan.step("install").unwrap();
        assert!(!install.variables.contains_key("BUNDLE_DEPLOYMENT"));
    }
}
