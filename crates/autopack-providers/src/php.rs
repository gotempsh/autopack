//! PHP provider, built on FrankenPHP.

use serde::Deserialize;

use autopack_core::plan::{Command, GeneratedValue, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::procfile_web_command;
use crate::version::Requirement;

/// PHP version used when `composer.json` does not constrain one.
const DEFAULT_PHP_VERSION: &str = "8.3";

/// `major.minor` lines FrankenPHP publishes images for (`1-php8.4`). Older
/// PHP has no FrankenPHP image at all, so asking for one fails the pull.
const FRANKENPHP_PHP_VERSIONS: &[&str] = &["8.2", "8.3", "8.4", "8.5"];

/// Newest line a range resolves to while it admits one: a brand-new PHP
/// release is more likely to surface deprecations than to help.
const PHP_RANGE_CEILING: &str = "8.4";

/// Where the FrankenPHP image installs its binary.
const FRANKENPHP_BINARY: &str = "/usr/local/bin/frankenphp";

/// Image the Composer binary is copied from.
const COMPOSER_IMAGE: &str = "composer:2";

/// Path the generated FrankenPHP config is written to.
const CADDYFILE_PATH: &str = "/app/Caddyfile";

/// Where the official PHP images keep compiled extensions and their ini files.
///
/// Extensions are compiled into the PHP installation, not into `/app`, so the
/// runtime image has to be handed these two directories explicitly — otherwise
/// the build succeeds and every request fails with "undefined function".
const PHP_EXTENSION_DIRS: &[&str] = &["/usr/local/lib/php/extensions", "/usr/local/etc/php/conf.d"];

/// File the build stage writes its resolved runtime package list to.
const PHP_RUNTIME_DEPS: &str = "/usr/local/share/autopack-php-runtime-deps";

/// A PHP extension that needs more than `docker-php-ext-install <name>`.
struct PhpExtension {
    /// Name as written in composer.json, without the `ext-` prefix.
    name: &'static str,
    /// Headers needed to compile it.
    build: &'static [&'static str],
    /// Shared libraries it loads at run time.
    ///
    /// Left empty on purpose: the soname-versioned runtime package changes
    /// between Debian releases (`libicu72` on bookworm, `libicu76` on trixie,
    /// and the `t64` transition renamed others), and FrankenPHP does not track
    /// the same release as the default base image. They are resolved from the
    /// compiled extensions instead — see `runtime_library_resolution`.
    runtime: &'static [&'static str],
    /// A `docker-php-ext-configure` invocation, when the defaults are wrong.
    configure: Option<&'static str>,
    /// True when the extension comes from PECL rather than the PHP source tree.
    pecl: bool,
}

/// Extensions autopack knows how to build.
///
/// Anything absent is still attempted with a plain `docker-php-ext-install`,
/// which covers the many extensions that need no external library
/// (`bcmath`, `pdo_mysql`, `opcache`, `pcntl`, `sockets`, `exif`, …).
const PHP_EXTENSIONS: &[PhpExtension] = &[
    PhpExtension {
        name: "gd",
        build: &["libpng-dev", "libjpeg-dev", "libfreetype6-dev"],
        runtime: &[],
        // Without this, gd builds with neither JPEG nor FreeType support and
        // fails at run time rather than at build time.
        configure: Some("docker-php-ext-configure gd --with-freetype --with-jpeg"),
        pecl: false,
    },
    PhpExtension {
        name: "intl",
        build: &["libicu-dev"],
        runtime: &[],
        configure: None,
        pecl: false,
    },
    PhpExtension {
        name: "zip",
        build: &["libzip-dev"],
        runtime: &[],
        configure: None,
        pecl: false,
    },
    PhpExtension {
        name: "pdo_pgsql",
        build: &["libpq-dev"],
        runtime: &[],
        configure: None,
        pecl: false,
    },
    PhpExtension {
        name: "pgsql",
        build: &["libpq-dev"],
        runtime: &[],
        configure: None,
        pecl: false,
    },
    PhpExtension {
        name: "soap",
        build: &["libxml2-dev"],
        runtime: &[],
        configure: None,
        pecl: false,
    },
    PhpExtension {
        name: "xsl",
        build: &["libxslt1-dev"],
        runtime: &[],
        configure: None,
        pecl: false,
    },
    PhpExtension {
        name: "redis",
        build: &[],
        runtime: &[],
        configure: None,
        pecl: true,
    },
    PhpExtension {
        name: "imagick",
        build: &["libmagickwand-dev"],
        runtime: &[],
        configure: None,
        pecl: true,
    },
];

/// Builds PHP applications.
///
/// The runtime is [FrankenPHP](https://frankenphp.dev): one process that is
/// both the web server and the PHP runtime. The usual alternative — Caddy or
/// nginx supervising php-fpm — needs a process manager in the container and
/// gets signal handling subtly wrong.
pub struct PhpProvider;

#[derive(Debug, Default, Deserialize)]
struct ComposerJson {
    #[serde(default)]
    require: indexmap::IndexMap<String, String>,
}

/// The parts of composer.lock that constrain the PHP version.
#[derive(Debug, Default, Deserialize)]
struct ComposerLock {
    #[serde(default)]
    packages: Vec<LockedPackage>,
}

#[derive(Debug, Default, Deserialize)]
struct LockedPackage {
    #[serde(default)]
    name: String,
    #[serde(default)]
    require: indexmap::IndexMap<String, String>,
}

impl Provider for PhpProvider {
    fn id(&self) -> &'static str {
        "php"
    }

    fn display_name(&self) -> &'static str {
        "PHP"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_any_file(["composer.json", "index.php", "public/index.php"]))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let composer: ComposerJson = ctx.app.read_json_opt("composer.json")?.unwrap_or_default();
        // A malformed lockfile is Composer's problem to report, not a reason
        // to refuse to plan.
        let lock: ComposerLock = ctx
            .app
            .read_json_opt("composer.lock")
            .ok()
            .flatten()
            .unwrap_or_default();

        let choice = php_version(&composer, &lock);
        if let Some(note) = &choice.note {
            ctx.add_note(note.clone());
        }
        let (version, source) = (choice.version, choice.source);
        let image = format!("dunglas/frankenphp:1-php{version}");
        ctx.set_base_image(&image);
        ctx.set_runtime_base_image(&image);
        ctx.set_base_image_runtimes(["php"]);
        ctx.add_metadata("phpVersion", &version);
        ctx.add_metadata("phpVersionSource", source);
        ctx.add_metadata("image", &image);

        let framework = detect_framework(ctx.app, &composer);
        if let Some(framework) = framework {
            ctx.add_metadata("framework", framework);
        }

        let extensions = requested_extensions(&composer);
        if !extensions.is_empty() {
            ctx.add_metadata("extensions", extensions.join(" "));
        }

        if ctx.app.has_file("composer.json") {
            // `--prefer-dist` downloads zip archives, and the FrankenPHP image
            // ships neither the PHP zip extension nor an `unzip` binary, so
            // Composer fails on the first package. git covers the source
            // fallback for packages with no dist archive.
            ctx.build_apt_packages
                .extend(["unzip", "git"].into_iter().map(String::from));
            self.plan_install(ctx, &extensions)?;
        }

        let document_root = document_root(ctx);
        ctx.add_metadata("documentRoot", &document_root);

        self.plan_build(ctx, &document_root)?;

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([APP_DIR]));
        if !extensions.is_empty() {
            // Extensions live in the PHP installation, not in /app, so they
            // need copying out explicitly or every request fails with an
            // undefined function. They land in the runtime *stage* rather than
            // the final image so the next command can inspect them.
            let mut copied: Vec<&str> = PHP_EXTENSION_DIRS.to_vec();
            copied.push(PHP_RUNTIME_DEPS);
            ctx.add_runtime_input(Layer::step(steps::BUILD).including(copied));
            ctx.add_runtime_command(Command::shell(install_recorded_runtime_libraries()));
        }
        // Symfony only loads `config/packages/prod` for `prod`; Laravel and
        // most others call it `production`.
        let app_env = if framework == Some("symfony") {
            "prod"
        } else {
            "production"
        };
        ctx.add_deploy_variable("APP_ENV", app_env);
        // Caddy keeps its state under XDG directories that default to `/data`
        // and `/config`, which only root can write; the container runs
        // unprivileged.
        ctx.add_deploy_variable("XDG_CONFIG_HOME", "/tmp/caddy/config");
        ctx.add_deploy_variable("XDG_DATA_HOME", "/tmp/caddy/data");
        match framework {
            Some("laravel") => {
                // Laravel logs to storage/logs by default, which nobody reads
                // in a container.
                ctx.add_deploy_variable("LOG_CHANNEL", "stderr");
                // Every encrypted cookie and session depends on it, so it is
                // generated once and kept, never baked into the image.
                ctx.require_generated_variable(
                    "APP_KEY",
                    GeneratedValue::PrefixedBase64Secret { bytes: 32 },
                );
                ctx.require_generated_variable("APP_URL", GeneratedValue::PublicUrl);
                if laravel_defaults_to_sqlite(ctx.app)? {
                    let database = ctx.env.get("DB_DATABASE").unwrap_or(LARAVEL_SQLITE);
                    let database = if database.starts_with('/') {
                        database.to_string()
                    } else {
                        format!("{APP_DIR}/{database}")
                    };
                    let directory = std::path::Path::new(&database)
                        .parent()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    ctx.add_note("SQLite persistence follows DB_DATABASE supplied during analysis. If it changes at runtime, update the storage mount to that database directory.");
                    ctx.require_persistent_path(
                        directory,
                        "Laravel's default database is a SQLite file inside the \
                         container; it is created at start so the app boots, but \
                         without persistent storage its data (users, sessions, jobs) is \
                         lost on every redeploy. Set DB_CONNECTION and DB_HOST (or \
                         DB_URL) to use a database server instead.",
                        &["DB_URL", "DB_HOST"],
                    );
                }
                let declares_release = autopack_core::Procfile::load(ctx.app)?
                    .is_some_and(|procfile| procfile.release().is_some());
                if !declares_release {
                    ctx.add_task("release", laravel_release_command());
                }
            }
            Some("symfony") => {
                ctx.require_generated_variable(
                    "APP_SECRET",
                    GeneratedValue::HexSecret { bytes: 16 },
                );
            }
            _ => {}
        }

        // The official image's binary carries a `cap_net_bind_service` file
        // capability so it can bind :80. A container started with every
        // capability dropped refuses to exec a binary whose file capabilities
        // exceed its bounding set ("exec: frankenphp: Operation not
        // permitted"), and the Caddyfile listens on $PORT, so the capability
        // is never needed. `cp` does not carry the extended attribute over.
        ctx.add_runtime_command(Command::shell(format!(
            "cp {FRANKENPHP_BINARY} {FRANKENPHP_BINARY}.autopack \
             && mv {FRANKENPHP_BINARY}.autopack {FRANKENPHP_BINARY}"
        )));

        let start = match procfile_web_command(ctx.app)? {
            Some(command) if heroku_php_server(&command).is_some() => {
                ctx.add_note(format!(
                    "Procfile web command `{command}` starts a Heroku-only server; \
                     serving the same document root with FrankenPHP instead"
                ));
                if framework == Some("laravel") {
                    laravel_start_command()
                } else {
                    format!("frankenphp run --config {CADDYFILE_PATH}")
                }
            }
            Some(command) => command,
            None if framework == Some("laravel") => laravel_start_command(),
            None => format!("frankenphp run --config {CADDYFILE_PATH}"),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

impl PhpProvider {
    fn plan_install(&self, ctx: &mut BuildContext<'_>, extensions: &[String]) -> Result<()> {
        let cache = ctx.shared_cache("composer", "/cache/composer");
        let manifests: Vec<&str> = ["composer.json", "composer.lock"]
            .into_iter()
            .filter(|file| ctx.app.has_file(file))
            .collect();

        // Extension headers must be present before the extensions build, and
        // the extensions before Composer runs — Composer verifies every
        // `ext-*` platform requirement and refuses to install without them.
        let mut extension_commands = Vec::new();
        for extension in extensions {
            let known = PHP_EXTENSIONS.iter().find(|e| e.name == extension);
            let mut install = Vec::new();
            if let Some(known) = known {
                ctx.build_apt_packages
                    .extend(known.build.iter().map(|p| p.to_string()));
                ctx.deploy_apt_packages
                    .extend(known.runtime.iter().map(|p| p.to_string()));
                if let Some(configure) = known.configure {
                    install.push(configure.to_string());
                }
            }
            if known.is_some_and(|known| known.pecl) {
                install.push(format!(
                    "pecl install {extension} && docker-php-ext-enable {extension}"
                ));
            } else {
                install.push(format!("docker-php-ext-install -j\"$(nproc)\" {extension}"));
            }
            // Many `ext-*` requirements name extensions compiled into PHP
            // itself (`json` since PHP 8, `ctype`, `mbstring`, `tokenizer`
            // in the official image); building them again fails with
            // "cannot stat 'modules/*'". Only build what is not loaded.
            extension_commands.push(format!(
                "if php -m | grep -qix '{extension}'; then \
                   echo 'autopack: PHP extension {extension} is already built in'; \
                 else {}; fi",
                install.join(" && ")
            ));
        }

        let step = ctx.step(steps::INSTALL);
        step.add_input(Layer::local().including(manifests));
        step.add_variable("COMPOSER_CACHE_DIR", "/cache/composer");
        step.add_variable("COMPOSER_ALLOW_SUPERUSER", "1");
        step.add_cache(cache);
        let records_libraries = !extension_commands.is_empty();
        for command in extension_commands {
            step.add_command(Command::shell(command));
        }
        if records_libraries {
            step.add_command(Command::shell(record_runtime_libraries()));
        }
        // The FrankenPHP image has PHP but no Composer, and the Composer image
        // has an older PHP. Taking just the phar keeps one PHP version in play.
        step.add_command(Command::copy_from(
            COMPOSER_IMAGE,
            "/usr/bin/composer",
            "/usr/local/bin/composer",
        ));
        // Autoloading and scripts are deferred until the source is present.
        step.add_command(Command::shell(
            "composer install --no-dev --no-scripts --no-autoloader --prefer-dist --no-interaction",
        ));
        Ok(())
    }

    fn plan_build(&self, ctx: &mut BuildContext<'_>, document_root: &str) -> Result<()> {
        let has_composer = ctx.has_step(steps::INSTALL);
        let base = if has_composer {
            Layer::step(steps::INSTALL)
        } else {
            Layer::step(steps::PACKAGES)
        };
        let config = caddyfile(document_root);

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![base, Layer::local()];
        if has_composer {
            step.add_variable("COMPOSER_ALLOW_SUPERUSER", "1");
            step.add_command(Command::shell(
                "composer dump-autoload --optimize --no-dev --no-interaction",
            ));
        }

        // Laravel's Vite (or Mix) build writes `public/build`; a page using
        // `@vite` is a 500 without its manifest. It runs after Composer: Vite
        // plugins such as Ziggy and Wayfinder call `php artisan`.
        if let Some(front_end) = crate::node::plan_front_end(ctx, steps::BUILD)? {
            if front_end.has_build_script {
                let build = front_end.manager.run_command("build");
                ctx.add_metadata("assets", &build);
                ctx.step(steps::BUILD).add_command(Command::shell(build));
            }
        }
        let step = ctx.step(steps::BUILD);
        let asset = step.add_asset("Caddyfile", config);
        step.add_command(Command::file(CADDYFILE_PATH, asset));
        Ok(())
    }
}

/// Record which packages own the shared libraries the extensions link against.
///
/// Runs in the *build* stage, which is the only place it can: `ldd` reports a
/// missing library as "not found" with no path, so resolution has to happen
/// where the `-dev` packages are still installed. The answer is written to a
/// file the runtime stage reads.
///
/// Paths go through `readlink -f` first. On a usr-merged Debian, `ldd` prints
/// `/lib/<triplet>/libicuio.so.76` while dpkg records the file under
/// `/usr/lib/...`, so querying the raw path silently finds nothing.
///
/// This replaces hardcoding names like `libicu72`, which is correct on
/// bookworm and wrong on trixie (`libicu76`) — and FrankenPHP tracks a
/// different Debian release than the default base image.
fn record_runtime_libraries() -> String {
    format!(
        "set -eu; \
         ldd \"$(php -r 'echo ini_get(\"extension_dir\");')\"/*.so 2>/dev/null \
           | awk '/=> \\// {{ print $3 }}' | sort -u \
           | xargs -r readlink -f 2>/dev/null | sort -u \
           | xargs -r dpkg-query -S 2>/dev/null \
           | cut -d: -f1 | sed 's/:.*//' | sort -u > {PHP_RUNTIME_DEPS}; \
         cat {PHP_RUNTIME_DEPS}"
    )
}

/// Install the packages recorded during the build.
fn install_recorded_runtime_libraries() -> String {
    let apt_update = autopack_core::apt::update_command();
    format!(
        "set -eu; \
         if [ -s {PHP_RUNTIME_DEPS} ]; then \
           {apt_update}; \
           apt-get install -y --no-install-recommends $(cat {PHP_RUNTIME_DEPS}); \
           rm -rf /var/lib/apt/lists/*; \
         fi"
    )
}

/// The `ext-*` requirements declared in composer.json.
fn requested_extensions(composer: &ComposerJson) -> Vec<String> {
    composer
        .require
        .keys()
        .filter_map(|requirement| requirement.strip_prefix("ext-"))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Where Laravel keeps its default SQLite database.
const LARAVEL_SQLITE: &str = "/app/database/database.sqlite";

/// Whether the app falls back to SQLite when `DB_CONNECTION` is unset, as
/// Laravel 11 and later do (`env('DB_CONNECTION', 'sqlite')`).
fn laravel_defaults_to_sqlite(app: &App) -> Result<bool> {
    let config = app
        .read_file_opt("config/database.php")?
        .unwrap_or_default()
        .replace('"', "'")
        .replace(' ', "");
    Ok(config.contains("env('DB_CONNECTION','sqlite')"))
}

/// Runs pending migrations without prompting.
const LARAVEL_MIGRATE: &str = "php artisan migrate --force";

/// Create a missing SQLite file before the one-off migration task. Database
/// migrations must fail the deployment, never race on every server restart.
fn laravel_release_command() -> String {
    format!(
        "if [ \"${{DB_CONNECTION:-sqlite}}\" = sqlite ] && [ -z \"${{DB_URL:-}}\" ]; then \
           db=\"${{DB_DATABASE:-/app/database/database.sqlite}}\"; \
           if [ \"$db\" != ':memory:' ]; then mkdir -p \"$(dirname \"$db\")\" && touch \"$db\" || exit 1; fi; \
         fi; {LARAVEL_MIGRATE}"
    )
}

fn laravel_start_command() -> String {
    format!("exec frankenphp run --config {CADDYFILE_PATH}")
}

/// The PHP version to build with, and why.
struct PhpVersionChoice {
    version: String,
    source: &'static str,
    note: Option<String>,
}

/// The newest FrankenPHP-published PHP line that `composer.json` *and* every
/// locked package accept.
///
/// Composer refuses to install a lockfile whose packages exclude the running
/// interpreter, so the lock's constraints matter as much as the app's own.
/// Ranges resolve to the newest admitted line rather than the lower bound:
/// `^7.4 || ^8.0` has no FrankenPHP image at 7.4.
fn php_version(composer: &ComposerJson, lock: &ComposerLock) -> PhpVersionChoice {
    let app = composer
        .require
        .get("php")
        .and_then(|constraint| Requirement::parse(constraint));
    let locked: Vec<Requirement> = lock
        .packages
        .iter()
        .filter_map(|package| package.require.get("php"))
        .filter_map(|constraint| Requirement::parse(constraint))
        .collect();

    let all_admit = |candidate: &str| {
        app.as_ref().is_none_or(|req| req.admits(candidate))
            && locked.iter().all(|req| req.admits(candidate))
    };
    let newest = |admits: &dyn Fn(&str) -> bool| {
        let settled = FRANKENPHP_PHP_VERSIONS
            .iter()
            .rev()
            .copied()
            .filter(|candidate| {
                crate::version::Version::parse(candidate).map(|v| (v.major, v.minor))
                    <= crate::version::Version::parse(PHP_RANGE_CEILING).map(|v| (v.major, v.minor))
            })
            .find(|candidate| admits(candidate));
        settled.or_else(|| {
            FRANKENPHP_PHP_VERSIONS
                .iter()
                .rev()
                .copied()
                .find(|candidate| admits(candidate))
        })
    };

    if app.is_none() && locked.is_empty() {
        return PhpVersionChoice {
            version: DEFAULT_PHP_VERSION.to_string(),
            source: "autopack default",
            note: None,
        };
    }
    if let Some(version) = newest(&all_admit) {
        return PhpVersionChoice {
            version: version.to_string(),
            source: if locked.is_empty() {
                "composer.json require.php"
            } else {
                "composer.json require.php and composer.lock"
            },
            note: None,
        };
    }
    if let Some(app) = &app {
        if let Some(version) = newest(&|candidate: &str| app.admits(candidate)) {
            let blockers: Vec<&str> = lock
                .packages
                .iter()
                .filter(|package| {
                    package
                        .require
                        .get("php")
                        .and_then(|constraint| Requirement::parse(constraint))
                        .is_some_and(|req| !req.admits(version))
                })
                .map(|package| package.name.as_str())
                .take(5)
                .collect();
            return PhpVersionChoice {
                version: version.to_string(),
                source: "composer.json require.php",
                note: Some(format!(
                    "no PHP version FrankenPHP publishes ({}) satisfies every locked package; \
                     using PHP {version}. Composer may reject: {}",
                    FRANKENPHP_PHP_VERSIONS.join(", "),
                    blockers.join(", ")
                )),
            };
        }
    }
    let oldest = FRANKENPHP_PHP_VERSIONS[0];
    PhpVersionChoice {
        version: oldest.to_string(),
        source: "oldest FrankenPHP-supported PHP",
        note: Some(format!(
            "composer.json requires PHP {}, which FrankenPHP does not publish (it supports {}); \
             building with PHP {oldest}, the closest available. Raise `require.php` if the app \
             runs on it",
            composer
                .require
                .get("php")
                .map(String::as_str)
                .unwrap_or("?"),
            FRANKENPHP_PHP_VERSIONS.join(", ")
        )),
    }
}

/// The document root a Heroku PHP server command (`heroku-php-apache2 web/`)
/// serves, when `command` is one. Those wrappers only exist on Heroku's stack.
///
/// Returns `Some(None)` for a wrapper with no document root argument.
fn heroku_php_server(command: &str) -> Option<Option<String>> {
    let mut words = command.split_whitespace();
    let program = words.next()?;
    let program = program.rsplit('/').next().unwrap_or(program);
    if !matches!(program, "heroku-php-apache2" | "heroku-php-nginx") {
        return None;
    }
    // Options (`-C nginx.conf`, `-F fpm.conf`, `-i php.ini`) take a value;
    // the document root is the first bare argument.
    let mut root = None;
    let mut skip_value = false;
    for word in words {
        if skip_value {
            skip_value = false;
            continue;
        }
        if word.starts_with('-') {
            skip_value = !word.contains('=');
            continue;
        }
        root = Some(word.trim_matches('/').to_string());
        break;
    }
    Some(root.filter(|root| !root.is_empty() && !root.contains("..")))
}

/// Where the front controller lives.
fn document_root(ctx: &BuildContext<'_>) -> String {
    if let Some(configured) = ctx.env.config("PHP_ROOT") {
        return format!("{APP_DIR}/{}", configured.trim_matches('/'));
    }
    // A Heroku Procfile names the document root its server was given.
    if let Ok(Some(command)) = procfile_web_command(ctx.app) {
        if let Some(Some(root)) = heroku_php_server(&command) {
            if ctx.app.has_dir(&root) {
                return format!("{APP_DIR}/{root}");
            }
        }
    }
    // Laravel, Symfony and most modern frameworks put the front controller in
    // `public/`; exposing the repository root instead would serve `.env`.
    for candidate in ["public", "web", "html"] {
        if ctx.app.has_file(format!("{candidate}/index.php")) {
            return format!("{APP_DIR}/{candidate}");
        }
    }
    APP_DIR.to_string()
}

fn detect_framework(app: &App, composer: &ComposerJson) -> Option<&'static str> {
    if composer.require.contains_key("laravel/framework") || app.has_file("artisan") {
        Some("laravel")
    } else if composer.require.contains_key("symfony/framework-bundle") {
        Some("symfony")
    } else if composer.require.contains_key("wordpress") || app.has_file("wp-config.php") {
        Some("wordpress")
    } else {
        None
    }
}

/// A FrankenPHP Caddyfile serving `document_root`.
fn caddyfile(document_root: &str) -> String {
    format!(
        "{{\n\
         \tfrankenphp\n\
         \torder php_server before file_server\n\
         \tauto_https off\n\
         \tadmin off\n\
         \tlog {{\n\t\tformat console\n\t}}\n\
         }}\n\
         \n\
         :{{$PORT:3000}} {{\n\
         \troot * {document_root}\n\
         \tencode zstd gzip\n\
         \tphp_server\n\
         }}\n"
    )
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, write_app};
    use autopack_core::plan::GeneratedValue;

    #[test]
    fn laravel_release_creates_sqlite_before_migrating_and_propagates_failure() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("data/custom.sqlite");
        let stub = directory.path().join("php");
        std::fs::write(
            &stub,
            "#!/bin/sh\ntest -f \"$DB_DATABASE\" || exit 99\nexit 7\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(super::laravel_release_command())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    directory.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("DB_CONNECTION", "sqlite")
            .env("DB_URL", "")
            .env("DB_DATABASE", &database)
            .status()
            .unwrap();
        assert!(database.is_file());
        assert_eq!(status.code(), Some(7));
        assert!(!super::laravel_start_command().contains("migrate"));
    }

    #[test]
    fn laravel_persistence_uses_the_effective_database_directory() {
        let (_dir, app) = write_app(&[
            (
                "composer.json",
                r#"{"require":{"laravel/framework":"^11.0"}}"#,
            ),
            ("artisan", ""),
            ("public/index.php", "<?php"),
            (
                "config/database.php",
                "<?php return ['default' => env('DB_CONNECTION', 'sqlite')];",
            ),
        ]);
        let analysis =
            crate::test_support::plan_with_env(&app, &[("DB_DATABASE", "/data/custom.sqlite")])
                .unwrap();
        assert_eq!(analysis.plan.deploy.persistent_paths[0].path, "/data");
    }

    #[test]
    fn laravel_apps_serve_the_public_directory() {
        let (_dir, app) = write_app(&[
            (
                "composer.json",
                r#"{"require":{"php":"^8.2","laravel/framework":"^11.0"}}"#,
            ),
            ("composer.lock", "{}"),
            ("artisan", ""),
            ("public/index.php", "<?php"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "php");
        assert_eq!(analysis.metadata["framework"], "laravel");
        // `^8.2` admits every published line; the newest settled one wins
        // over the lower bound.
        assert_eq!(analysis.metadata["image"], "dunglas/frankenphp:1-php8.4");
        assert_eq!(analysis.metadata["documentRoot"], "/app/public");
        let start = analysis.plan.deploy.start_command.as_deref().unwrap();
        assert!(start.ends_with("exec frankenphp run --config /app/Caddyfile"));
        assert!(!start.contains("migrate"));
        let release = &analysis.plan.deploy.tasks["release"];
        assert!(release.contains("touch"));
        assert!(release.contains("php artisan migrate --force"));
    }

    #[test]
    fn laravel_apps_get_a_generated_key_and_container_friendly_defaults() {
        let (_dir, app) = write_app(&[
            (
                "composer.json",
                r#"{"require":{"php":"^8.2","laravel/framework":"^11.0"}}"#,
            ),
            ("composer.lock", "{}"),
            ("artisan", ""),
            ("public/index.php", "<?php"),
        ]);
        let deploy = plan_for(&app).plan.deploy;

        assert_eq!(
            deploy.generated_variables.get("APP_KEY").map(|v| &v.value),
            Some(&GeneratedValue::PrefixedBase64Secret { bytes: 32 })
        );
        assert_eq!(
            deploy.generated_variables.get("APP_URL").map(|v| &v.value),
            Some(&GeneratedValue::PublicUrl)
        );
        assert_eq!(deploy.variables["APP_ENV"], "production");
        assert_eq!(deploy.variables["LOG_CHANNEL"], "stderr");
        assert_eq!(deploy.variables["XDG_DATA_HOME"], "/tmp/caddy/data");
        assert!(!deploy.variables.contains_key("APP_KEY"));
    }

    #[test]
    fn laravels_default_sqlite_database_needs_persistent_storage() {
        let (_dir, app) = write_app(&[
            (
                "composer.json",
                r#"{"require":{"php":"^8.2","laravel/framework":"^11.0"}}"#,
            ),
            ("composer.lock", "{}"),
            ("artisan", ""),
            ("public/index.php", "<?php"),
            (
                "config/database.php",
                "<?php return ['default' => env('DB_CONNECTION', 'sqlite')];",
            ),
        ]);
        let deploy = plan_for(&app).plan.deploy;
        assert_eq!(deploy.persistent_paths.len(), 1);
        assert_eq!(deploy.persistent_paths[0].path, "/app/database");
        assert_eq!(
            deploy.persistent_paths[0].unless_set,
            vec!["DB_URL", "DB_HOST"]
        );
    }

    #[test]
    fn a_laravel_app_defaulting_to_mysql_needs_no_persistent_storage() {
        let (_dir, app) = write_app(&[
            (
                "composer.json",
                r#"{"require":{"php":"^8.2","laravel/framework":"^10.0"}}"#,
            ),
            ("composer.lock", "{}"),
            ("artisan", ""),
            ("public/index.php", "<?php"),
            (
                "config/database.php",
                "<?php return ['default' => env(\"DB_CONNECTION\", \"mysql\")];",
            ),
        ]);
        assert!(plan_for(&app).plan.deploy.persistent_paths.is_empty());
    }

    #[test]
    fn symfony_apps_run_in_prod_with_a_generated_secret() {
        let (_dir, app) = write_app(&[
            (
                "composer.json",
                r#"{"require":{"php":"^8.2","symfony/framework-bundle":"^7.0"}}"#,
            ),
            ("composer.lock", "{}"),
            ("public/index.php", "<?php"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["framework"], "symfony");
        let deploy = analysis.plan.deploy;
        assert_eq!(deploy.variables["APP_ENV"], "prod");
        assert_eq!(
            deploy
                .generated_variables
                .get("APP_SECRET")
                .map(|v| &v.value),
            Some(&GeneratedValue::HexSecret { bytes: 16 })
        );
    }

    #[test]
    fn laravel_front_ends_are_built_after_composer() {
        let (_dir, app) = write_app(&[
            (
                "composer.json",
                r#"{"require":{"php":"^8.2","laravel/framework":"^11.0"}}"#,
            ),
            ("composer.lock", "{}"),
            ("artisan", ""),
            ("public/index.php", "<?php"),
            (
                "package.json",
                r#"{"scripts":{"build":"vite build"},"devDependencies":{"vite":"^6.0.0"}}"#,
            ),
            ("package-lock.json", "{}"),
        ]);
        let analysis = plan_for(&app);
        let commands: Vec<String> = analysis
            .plan
            .step("build")
            .unwrap()
            .commands
            .iter()
            .map(|command| command.display_name().to_string())
            .collect();

        let composer = commands
            .iter()
            .position(|c| c.starts_with("composer dump-autoload"));
        let install = commands.iter().position(|c| c == "npm ci");
        let build = commands.iter().position(|c| c == "npm run build");
        assert!(composer < install && install < build, "{commands:?}");
        assert!(analysis.packages.iter().any(|(tool, _)| tool == "node"));
    }

    #[test]
    fn php_apps_without_a_package_json_install_no_node() {
        let (_dir, app) = write_app(&[
            ("composer.json", r#"{"require":{"php":"^8.3"}}"#),
            ("composer.lock", "{}"),
            ("index.php", "<?php"),
        ]);
        let analysis = plan_for(&app);
        assert!(!analysis.packages.iter().any(|(tool, _)| tool == "node"));
    }

    #[test]
    fn built_in_extensions_are_not_rebuilt() {
        let (_dir, app) = write_app(&[
            (
                "composer.json",
                r#"{"require":{"php":"^8.2","ext-json":"*","ext-intl":"*"}}"#,
            ),
            ("composer.lock", "{}"),
            ("index.php", "<?php"),
        ]);
        let plan = format!("{:?}", plan_for(&app).plan);
        assert!(plan.contains("if php -m | grep -qix 'json'"), "{plan}");
        assert!(plan.contains("if php -m | grep -qix 'intl'"), "{plan}");
    }

    #[test]
    fn the_generated_caddyfile_never_exposes_the_repository_root_for_laravel() {
        let (_dir, app) = write_app(&[
            ("composer.json", r#"{"require":{"php":"^8.3"}}"#),
            ("public/index.php", "<?php"),
            (".env", "APP_KEY=secret"),
        ]);
        let analysis = plan_for(&app);
        let build = analysis.plan.step("build").unwrap();
        assert!(build.assets["Caddyfile"].contains("root * /app/public"));
    }

    #[test]
    fn a_bare_index_php_is_served_from_the_root() {
        let (_dir, app) = write_app(&[("index.php", "<?php echo 'hi';")]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["documentRoot"], "/app");
        // Without composer.json there is nothing to install.
        assert!(analysis.plan.step("install").is_none());
    }

    #[test]
    fn php_wins_over_node_for_a_laravel_repo_with_vite() {
        let (_dir, app) = write_app(&[
            (
                "composer.json",
                r#"{"require":{"laravel/framework":"^11"}}"#,
            ),
            ("public/index.php", ""),
            (
                "package.json",
                r#"{"devDependencies":{"vite":"^5"},"scripts":{"build":"vite build"}}"#,
            ),
        ]);
        assert_eq!(plan_for(&app).provider, "php");
    }

    #[test]
    fn php_ranges_resolve_to_the_newest_published_frankenphp_line() {
        // `^7.4 || ^8.0` has no FrankenPHP image at 7.4.
        let (_dir, app) = write_app(&[
            ("composer.json", r#"{"require":{"php":"^7.4 || ^8.0"}}"#),
            ("public/index.php", "<?php"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["image"], "dunglas/frankenphp:1-php8.4");
        assert!(!analysis.metadata.contains_key("configNote1"));

        // Only the newest line admits `^8.5`.
        let (_dir, app) = write_app(&[
            ("composer.json", r#"{"require":{"php":"^8.5"}}"#),
            ("public/index.php", "<?php"),
        ]);
        assert_eq!(
            plan_for(&app).metadata["image"],
            "dunglas/frankenphp:1-php8.5"
        );
    }

    #[test]
    fn locked_packages_cap_the_php_version() {
        let lock = r#"{"packages":[
            {"name":"vendor/old","require":{"php":">=7.2 <8.4"}},
            {"name":"vendor/any","require":{"php":"^8.1"}}
        ]}"#;
        let (_dir, app) = write_app(&[
            ("composer.json", r#"{"require":{"php":"^8.1"}}"#),
            ("composer.lock", lock),
            ("public/index.php", "<?php"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["image"], "dunglas/frankenphp:1-php8.3");
        assert_eq!(
            analysis.metadata["phpVersionSource"],
            "composer.json require.php and composer.lock"
        );
    }

    #[test]
    fn a_lock_no_published_version_satisfies_falls_back_to_the_app_range_with_a_note() {
        let lock = r#"{"packages":[{"name":"vendor/legacy","require":{"php":"<8.0"}}]}"#;
        let (_dir, app) = write_app(&[
            ("composer.json", r#"{"require":{"php":">=8.2"}}"#),
            ("composer.lock", lock),
            ("public/index.php", "<?php"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["image"], "dunglas/frankenphp:1-php8.4");
        assert!(analysis.metadata["configNote1"].contains("vendor/legacy"));
    }

    #[test]
    fn a_php_requirement_frankenphp_cannot_meet_uses_the_oldest_published_line() {
        let (_dir, app) = write_app(&[
            ("composer.json", r#"{"require":{"php":"^7.4"}}"#),
            ("index.php", "<?php"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["image"], "dunglas/frankenphp:1-php8.2");
        assert!(analysis.metadata["configNote1"].contains("^7.4"));
    }

    #[test]
    fn heroku_procfile_servers_are_replaced_by_frankenphp() {
        let (_dir, app) = write_app(&[
            ("composer.json", r#"{"require":{"php":"^8.2"}}"#),
            ("Procfile", "web: heroku-php-apache2 web/\n"),
            ("web/index.php", "<?php"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("frankenphp run --config /app/Caddyfile")
        );
        assert_eq!(analysis.metadata["documentRoot"], "/app/web");
        assert!(analysis.metadata["configNote1"].contains("heroku-php-apache2"));
    }

    #[test]
    fn heroku_php_server_parsing() {
        use super::heroku_php_server;
        assert_eq!(
            heroku_php_server("heroku-php-apache2 web/"),
            Some(Some("web".to_string()))
        );
        assert_eq!(
            heroku_php_server("vendor/bin/heroku-php-nginx -C nginx.conf public/"),
            Some(Some("public".to_string()))
        );
        assert_eq!(heroku_php_server("heroku-php-apache2"), Some(None));
        assert_eq!(heroku_php_server("heroku-php-apache2 ../etc"), Some(None));
        assert_eq!(heroku_php_server("php artisan serve"), None);
    }

    #[test]
    fn the_runtime_binary_is_copied_without_its_file_capability() {
        // A container with every capability dropped refuses to exec a binary
        // that carries a file capability, so the runtime must replace it with
        // a plain copy.
        let (_dir, app) = write_app(&[("index.php", "<?php")]);
        let analysis = plan_for(&app);
        let runtime = analysis.plan.step("runtime").expect("a runtime step");
        assert!(runtime.commands.iter().any(|command| command
            .display_name()
            .contains("cp /usr/local/bin/frankenphp /usr/local/bin/frankenphp.autopack")));
    }
}
