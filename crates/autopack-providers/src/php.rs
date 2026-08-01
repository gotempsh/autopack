//! PHP provider, built on FrankenPHP.

use serde::Deserialize;

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::procfile_web_command;

/// PHP version used when `composer.json` does not constrain one.
const DEFAULT_PHP_VERSION: &str = "8.3";

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

        let (version, source) = php_version(&composer);
        let image = format!("dunglas/frankenphp:1-php{version}");
        ctx.set_base_image(&image);
        ctx.set_runtime_base_image(&image);
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
        ctx.add_deploy_variable("APP_ENV", "production");

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
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
            if let Some(known) = known {
                ctx.build_apt_packages
                    .extend(known.build.iter().map(|p| p.to_string()));
                ctx.deploy_apt_packages
                    .extend(known.runtime.iter().map(|p| p.to_string()));
                if let Some(configure) = known.configure {
                    extension_commands.push(configure.to_string());
                }
                if known.pecl {
                    extension_commands.push(format!(
                        "pecl install {extension} && docker-php-ext-enable {extension}"
                    ));
                    continue;
                }
            }
            extension_commands.push(format!("docker-php-ext-install -j\"$(nproc)\" {extension}"));
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
    format!(
        "set -eu; \
         if [ -s {PHP_RUNTIME_DEPS} ]; then \
           apt-get update; \
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

/// The PHP version from `composer.json`'s `require.php` constraint.
fn php_version(composer: &ComposerJson) -> (String, &'static str) {
    if let Some(constraint) = composer.require.get("php") {
        if let Some(version) = first_version(constraint) {
            return (version, "composer.json require.php");
        }
    }
    (DEFAULT_PHP_VERSION.to_string(), "autopack default")
}

/// `^8.2 || ^8.3` -> `8.2`. FrankenPHP publishes `major.minor` tags only.
fn first_version(constraint: &str) -> Option<String> {
    let first = constraint.split("||").next()?.split_whitespace().next()?;
    let cleaned = first.trim_start_matches(['^', '~', '>', '=', '<', 'v']);
    let mut parts = cleaned.split('.');
    let major = parts.next()?;
    let minor = parts.next().unwrap_or("0");
    if !major.chars().all(|c| c.is_ascii_digit()) || major.is_empty() {
        return None;
    }
    let minor: String = minor.chars().take_while(char::is_ascii_digit).collect();
    if minor.is_empty() {
        return None;
    }
    Some(format!("{major}.{minor}"))
}

/// Where the front controller lives.
fn document_root(ctx: &BuildContext<'_>) -> String {
    if let Some(configured) = ctx.env.config("PHP_ROOT") {
        return format!("{APP_DIR}/{}", configured.trim_matches('/'));
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
        assert_eq!(analysis.metadata["image"], "dunglas/frankenphp:1-php8.2");
        assert_eq!(analysis.metadata["documentRoot"], "/app/public");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("frankenphp run --config /app/Caddyfile")
        );
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
}
