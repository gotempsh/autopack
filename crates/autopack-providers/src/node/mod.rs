//! Node.js provider: npm, pnpm, yarn and bun apps, including SPA builds.

mod package_json;
mod package_manager;

pub use package_json::PackageJson;
pub use package_manager::PackageManager;

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::{
    caddy_layer, caddy_start_command, caddyfile, install_recorded_runtime_libraries,
    normalize_version_range, procfile_web_command, read_version_file,
    record_runtime_libraries_from_command, CADDYFILE_PATH, ELF_INSPECTION_PACKAGE,
    RUNTIME_DEPS_FILE,
};

/// Where a Next.js standalone bundle is staged for the runtime image.
const STANDALONE_DIR: &str = "/app/standalone";

/// Directory Playwright keeps browsers in when `PLAYWRIGHT_BROWSERS_PATH=0`
/// asks for an install beside the package.
const PLAYWRIGHT_BROWSERS: &str = ".local-browsers";

/// Fonts a browser needs but never links.
///
/// Everything else Chromium requires is a `DT_NEEDED` entry, so `readelf` finds it
/// — measured against Chrome for Testing: with the discovered set installed
/// there are zero unresolved libraries and it renders, screenshots and prints.
/// Fonts are the exception. They are opened through fontconfig at run time, so
/// no amount of inspecting the binary reveals them, and a browser without them
/// draws text as empty boxes rather than failing in a way anyone would notice.
const CHROMIUM_FONTS: &[&str] = &["fonts-liberation"];

/// Node version used when the app does not pin one.
const DEFAULT_NODE_VERSION: &str = "24";

/// Entry points tried, in order, when nothing else identifies a start command.
const ENTRY_POINTS: &[&str] = &[
    "server.js",
    "server.mjs",
    "index.js",
    "index.mjs",
    "app.js",
    "src/index.js",
    "src/index.mjs",
    "dist/index.js",
    "build/index.js",
];

/// Builds JavaScript and TypeScript applications.
pub struct NodeProvider;

/// A recognised JavaScript framework, where recognising it changes the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framework {
    Next,
    Nuxt,
    Remix,
    SvelteKit,
    Astro,
    Nest,
    Vite,
    CreateReactApp,
    Angular,
    Webpack,
    None,
}

impl Framework {
    fn id(self) -> &'static str {
        match self {
            Self::Next => "next",
            Self::Nuxt => "nuxt",
            Self::Remix => "remix",
            Self::SvelteKit => "sveltekit",
            Self::Astro => "astro",
            Self::Nest => "nest",
            Self::Vite => "vite",
            Self::CreateReactApp => "create-react-app",
            Self::Angular => "angular",
            Self::Webpack => "webpack",
            Self::None => "none",
        }
    }
}

/// A build that produces static files served by Caddy rather than a server process.
struct StaticSite {
    /// Output directory relative to the app root.
    directory: String,
    /// Whether unknown paths should fall back to `index.html`.
    spa: bool,
}

impl Provider for NodeProvider {
    fn id(&self) -> &'static str {
        "node"
    }

    fn display_name(&self) -> &'static str {
        "Node.js"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("package.json"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let package: PackageJson = ctx.app.read_json_opt("package.json")?.unwrap_or_default();
        let manager = PackageManager::detect(ctx.app, &package);
        let framework = detect_framework(&package);

        let (node_version, version_source) = node_version(ctx.app, &package)?;
        ctx.packages.add("node", &node_version, version_source);
        if let Some((tool, version)) = manager.mise_tool(&package) {
            ctx.packages.add(tool, version, "packageManager / lockfile");
        }

        ctx.add_metadata("framework", framework.id());
        ctx.add_metadata("packageManager", manager.id());
        ctx.add_metadata("nodeVersion", &node_version);

        // Native modules compile against system headers and load shared
        // libraries at run time; neither is present in a slim base image.
        let manifest = ctx.app.read_file_opt("package.json")?.unwrap_or_default();
        let (build_packages, runtime_packages) =
            crate::native::required_packages(&manifest, crate::native::NODE);
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

        // pnpm 11+ is distributed as `@pnpm/exe`, a standalone binary that
        // dynamically links libatomic.so.1. debian-slim does not ship it, so
        // without this package `pnpm ci` fails with exit 127 before install.
        // The runtime stage also copies `/mise` (and puts shims on PATH), so
        // non-simple start scripts that fall back to `pnpm run start` need the
        // same library at boot.
        if manager.needs_libatomic(&package, ctx.lock()) {
            ctx.build_apt_packages.push("libatomic1".to_string());
        }

        // The browser download has to land inside /app to survive into the
        // runtime image, and the app has to look for it there at boot.
        let browsers = browser_tooling(&package);
        ctx.deploy_apt_packages
            .extend(browsers.runtime_packages.iter().cloned());
        if !browsers.browser_searches.is_empty() {
            ctx.build_apt_packages
                .push(ELF_INSPECTION_PACKAGE.to_string());
            ctx.add_runtime_input(Layer::step(steps::INSTALL).including([RUNTIME_DEPS_FILE]));
            ctx.add_runtime_command(Command::shell(install_recorded_runtime_libraries(
                RUNTIME_DEPS_FILE,
            )));
        }
        if !browsers.is_empty() {
            ctx.add_metadata("browser", "chromium");
        }
        for (key, value) in &browsers.variables {
            ctx.add_deploy_variable(*key, value);
        }

        self.plan_install(ctx, &package, manager, &browsers)?;
        self.plan_build(ctx, &package, manager, framework, &browsers)?;

        let static_site = static_site(ctx, &package, framework)?;
        // Deferred until the deploy path is known: a static site serves with
        // Caddy and drops /mise, so it has no pnpm binary to load the library
        // for. Deciding this up front shipped it into every Vite bundle.
        if manager.needs_libatomic(&package, ctx.lock()) && static_site.is_none() {
            ctx.deploy_apt_packages.push("libatomic1".to_string());
        }
        match &static_site {
            Some(site) => self.plan_static_deploy(ctx, site)?,
            // A Next.js app configured for standalone output has already told
            // us it wants a pruned server; honouring that is the difference
            // between a 226MB image and a 3.1GB one.
            None if framework == Framework::Next && uses_standalone_output(ctx.app)? => {
                self.plan_standalone_deploy(ctx)?
            }
            None => self.plan_server_deploy(ctx, &package, manager)?,
        }

        Ok(())
    }
}

impl NodeProvider {
    /// Dependency install, isolated from application source so that editing a
    /// source file does not reinstall `node_modules`.
    fn plan_install(
        &self,
        ctx: &mut BuildContext<'_>,
        package: &PackageJson,
        manager: PackageManager,
        browsers: &BrowserTooling,
    ) -> Result<()> {
        let (cache_dir, cache_env) = manager.cache();
        let cache = ctx.shared_cache(format!("{}-store", manager.id()), cache_dir);

        // Workspaces reference package.json files scattered through the repo.
        // Copying only the root manifests would break the install, so trade
        // cache granularity for correctness and say so in the metadata.
        let manifest_layer = if package.has_workspaces() || ctx.app.has_file("pnpm-workspace.yaml")
        {
            ctx.add_metadata(
                "installContext",
                "full source (workspaces need every package.json)",
            );
            Layer::local()
        } else {
            let manifests: Vec<&str> = manager
                .manifest_files()
                .iter()
                .copied()
                .filter(|file| ctx.app.has_file(file))
                .collect();
            ctx.add_metadata("installContext", manifests.join(", "));
            Layer::local().including(manifests)
        };

        let install_command = manager.install_command(ctx.app, package, ctx.lock());
        let step = ctx.step(steps::INSTALL);
        step.add_input(manifest_layer);
        step.add_cache(cache);
        for (key, value) in cache_env {
            step.add_variable(key, value);
        }
        // Docker RUN has no TTY; pnpm prompts to purge node_modules unless CI
        // is set (ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY).
        if manager == PackageManager::Pnpm {
            step.add_variable("CI", "true");
        }
        for (key, value) in &browsers.variables {
            step.add_variable(*key, value);
        }
        step.add_command(Command::shell(install_command));
        for download in &browsers.downloads {
            step.add_command(Command::shell(download.clone()));
        }
        // Ask the browser what it links rather than carrying a list. The
        // hardcoded closure was a hand-maintained union across two Chrome
        // binaries, wrong in both directions — it missed seven libraries apt
        // happened to pull in transitively, and its names are bookworm's, so
        // it breaks on a base image bump.
        if !browsers.browser_searches.is_empty() {
            let search = browsers.browser_searches.join("; ");
            // Nothing downstream notices an empty result: no binaries means no
            // libraries recorded, the runtime stage skips its install, and the
            // image looks fine until the first time it opens a browser. Stop
            // here, where the message can say what was searched.
            step.add_command(Command::shell(format!(
                "set -eu; \
                 if [ -z \"$({search})\" ]; then \
                   echo 'autopack: the install step downloaded no browser' >&2; \
                   echo 'Searched: {search}' >&2; \
                   echo 'If the download was skipped (PUPPETEER_SKIP_DOWNLOAD, \
PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD), either unset it or drop the dependency.' >&2; \
                   exit 1; \
                 fi"
            )));
            step.add_command(Command::shell(record_runtime_libraries_from_command(
                &search,
                RUNTIME_DEPS_FILE,
            )));
        }
        Ok(())
    }

    /// Application build. The step always exists — it is what carries source
    /// into the runtime image — but only runs a command when there is one.
    fn plan_build(
        &self,
        ctx: &mut BuildContext<'_>,
        package: &PackageJson,
        manager: PackageManager,
        framework: Framework,
        browsers: &BrowserTooling,
    ) -> Result<()> {
        let build_script = package
            .script("build")
            .map(|_| manager.run_command("build"));
        let (node_version, _) = node_version(ctx.app, package)?;
        let legacy_openssl = needs_legacy_openssl(package, &node_version);

        // Cache each Next package at its own build directory, including root
        // scripts that delegate to workspace apps. Scope locks to the source
        // checkout so unrelated applications never serialize on one mount.
        let mut next_directories = Vec::new();
        if build_script.is_some() {
            if framework == Framework::Next {
                next_directories.push(APP_DIR.to_string());
            }
            for manifest in ctx.app.find_files("**/package.json")? {
                if manifest == "package.json" {
                    continue;
                }
                let child: PackageJson = match ctx.app.read_json(&manifest) {
                    Ok(package) => package,
                    Err(_) => continue,
                };
                if child.has_dependency("next") {
                    let directory = manifest.trim_end_matches("/package.json");
                    next_directories.push(format!("{APP_DIR}/{directory}"));
                }
            }
        }
        let identity = ctx.app.source().to_string_lossy();
        // Stable FNV-1a digest: cache names contain no host path or shell input.
        let scope = identity.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
        let caches: Vec<_> = next_directories
            .into_iter()
            .map(|directory| {
                let package_scope = directory.bytes().fold(scope, |hash, byte| {
                    (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
                });
                ctx.locked_cache(
                    format!("next-cache-{package_scope:016x}"),
                    format!("{directory}/.next/cache"),
                )
            })
            .collect();
        let step = ctx.step(steps::BUILD);
        for cache in caches {
            step.add_cache(cache);
        }
        step.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
        if legacy_openssl {
            step.add_variable("NODE_OPTIONS", LEGACY_OPENSSL_OPTION);
        }
        // `pnpm run` may re-invoke install for a deps check; same no-TTY rule.
        if manager == PackageManager::Pnpm {
            step.add_variable("CI", "true");
        }
        // `pnpm run build` can re-invoke install, which would re-download the
        // browser to the default cache without this.
        for (key, value) in &browsers.variables {
            step.add_variable(*key, value);
        }
        if let Some(command) = build_script {
            step.add_command(Command::shell(command));
        }
        Ok(())
    }

    /// Serve the build output with Caddy and drop Node from the runtime image.
    fn plan_static_deploy(&self, ctx: &mut BuildContext<'_>, site: &StaticSite) -> Result<()> {
        let root = format!("{APP_DIR}/{}", site.directory.trim_matches('/'));
        let config = caddyfile(&root, site.spa);

        let step = ctx.step(steps::BUILD);
        let asset = step.add_asset("Caddyfile", config);
        step.add_command(Command::file(CADDYFILE_PATH, asset));

        ctx.set_runtime_includes_runtimes(false);
        ctx.add_deploy_input(Layer::step(steps::BUILD).including([root.as_str(), CADDYFILE_PATH]));
        let caddy = caddy_layer(ctx);
        ctx.add_deploy_input(caddy);
        ctx.set_start_command(caddy_start_command());
        ctx.add_metadata("serve", format!("caddy static ({})", site.directory));
        Ok(())
    }

    /// Ship the app directory and run a Node process.
    /// Ship only the Next.js standalone bundle.
    ///
    /// `output: "standalone"` makes Next emit a server with just the modules it
    /// actually imports. Shipping `/app` instead means carrying every
    /// dependency, dev dependencies included, and the full build output — for
    /// the Temps landing site that is 3.1GB against 226MB.
    ///
    /// The pieces have to be assembled: Next writes the server to
    /// `.next/standalone`, but leaves `.next/static` and `public/` out of it,
    /// expecting them to be placed alongside.
    fn plan_standalone_deploy(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let has_public = ctx.app.has_dir("public");
        let stage = format!(
            "mkdir -p {STANDALONE_DIR} && \
             cp -r .next/standalone/. {STANDALONE_DIR}/ && \
             mkdir -p {STANDALONE_DIR}/.next && \
             cp -r .next/static {STANDALONE_DIR}/.next/static{public}",
            public = if has_public {
                format!(" && cp -r public {STANDALONE_DIR}/public")
            } else {
                String::new()
            }
        );

        ctx.step(steps::BUILD).add_command(Command::shell(stage));

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([STANDALONE_DIR]));
        ctx.add_deploy_variable("NODE_ENV", "production");
        // The standalone server binds to HOSTNAME, which defaults to
        // localhost — unreachable from outside the container.
        ctx.add_deploy_variable("HOSTNAME", "0.0.0.0");
        ctx.add_metadata("nextOutput", "standalone");

        ctx.set_start_command(format!("node {STANDALONE_DIR}/server.js"));
        Ok(())
    }

    fn plan_server_deploy(
        &self,
        ctx: &mut BuildContext<'_>,
        package: &PackageJson,
        manager: PackageManager,
    ) -> Result<()> {
        ctx.add_deploy_input(Layer::step(steps::BUILD).including([APP_DIR]));
        ctx.add_deploy_variable("NODE_ENV", "production");
        // Framework CLIs (`next start`, `nest start`, `remix-serve`) live in
        // node_modules/.bin, which only a package manager puts on PATH. Since
        // the start script is exec'd directly — to keep the app as PID 1 for
        // SIGTERM — that directory has to be on PATH explicitly.
        ctx.add_deploy_path(format!("{APP_DIR}/node_modules/.bin"));

        if let Some(command) = self.start_command(ctx.app, package, manager)? {
            ctx.set_start_command(command);
        }
        Ok(())
    }

    fn start_command(
        &self,
        app: &App,
        package: &PackageJson,
        manager: PackageManager,
    ) -> Result<Option<String>> {
        if let Some(command) = procfile_web_command(app)? {
            return Ok(Some(command));
        }

        if let Some(script) = package.script("start") {
            // Running the script body directly makes the app PID 1, so it
            // receives SIGTERM. Going through `npm run` inserts a shell that
            // swallows it and turns every deploy into a 10s kill timeout.
            return Ok(Some(if is_simple_command(script) {
                script.to_string()
            } else {
                manager.run_command("start")
            }));
        }

        if let Some(main) = package.main.as_deref().filter(|main| app.has_file(main)) {
            return Ok(Some(format!("node {main}")));
        }

        if let Some(entry) = ENTRY_POINTS.iter().find(|entry| app.has_file(entry)) {
            return Ok(Some(format!("node {entry}")));
        }

        Ok(None)
    }
}

/// Whether `next.config.*` sets `output: "standalone"`.
///
/// Read as text rather than evaluated: the config is JavaScript or TypeScript
/// and may compute values, but this setting is written literally in practice.
fn uses_standalone_output(app: &App) -> Result<bool> {
    for name in [
        "next.config.ts",
        "next.config.js",
        "next.config.mjs",
        "next.config.cjs",
    ] {
        let Some(contents) = app.read_file_opt(name)? else {
            continue;
        };
        let normalised: String = contents.chars().filter(|c| !c.is_whitespace()).collect();
        if normalised.contains("output:\"standalone\"")
            || normalised.contains("output:'standalone'")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Which framework the app uses, if any autopack treats specially.
fn detect_framework(package: &PackageJson) -> Framework {
    // Ordered most specific first: a Next.js app also depends on react, and a
    // SvelteKit app also depends on vite.
    if package.has_dependency("next") {
        Framework::Next
    } else if package.has_any_dependency(&["nuxt", "nuxt3"]) {
        Framework::Nuxt
    } else if package.has_any_dependency(&["@remix-run/node", "@remix-run/serve"]) {
        Framework::Remix
    } else if package.has_dependency("@sveltejs/kit") {
        Framework::SvelteKit
    } else if package.has_dependency("astro") {
        Framework::Astro
    } else if package.has_dependency("@nestjs/core") {
        Framework::Nest
    } else if package.has_dependency("@angular/core") {
        Framework::Angular
    } else if package.has_dependency("react-scripts") {
        Framework::CreateReactApp
    } else if package.has_dependency("vite") {
        Framework::Vite
    } else if package.has_dependency("webpack") {
        Framework::Webpack
    } else {
        Framework::None
    }
}

/// Whether this build produces static files, and where they land.
///
/// Only frameworks with an unambiguous default output are handled. Anything
/// else runs a server process, which is recoverable with `AUTOPACK_STATIC_DIR`
/// — whereas serving the wrong directory would produce a silent 404 wall.
fn static_site(
    ctx: &BuildContext<'_>,
    package: &PackageJson,
    framework: Framework,
) -> Result<Option<StaticSite>> {
    if let Some(directory) = ctx.env.config("STATIC_DIR") {
        return Ok(Some(StaticSite {
            directory: directory.to_string(),
            spa: !ctx.env.is_enabled("AUTOPACK_STATIC_MPA"),
        }));
    }

    // A start script means the author intends to run a server.
    let has_server_script = package.script("start").is_some();

    let site = match framework {
        Framework::CreateReactApp if !has_server_script => Some(StaticSite {
            directory: "build".into(),
            spa: true,
        }),
        Framework::Vite if !has_server_script => Some(StaticSite {
            directory: "dist".into(),
            spa: true,
        }),
        // Astro only emits static output without a server adapter.
        Framework::Astro
            if !has_server_script
                && !package.has_any_dependency(&[
                    "@astrojs/node",
                    "@astrojs/vercel",
                    "@astrojs/netlify",
                    "@astrojs/cloudflare",
                ]) =>
        {
            Some(StaticSite {
                directory: "dist".into(),
                spa: false,
            })
        }
        // A webpack page bundle with no sign of a server. An unreadable output
        // directory stops planning here: falling through to the server path
        // would run its browser entry (`src/index.js`) under Node.
        Framework::Webpack
            if !has_server_script
                && is_webpack_spa(package)
                && !has_server_entry(ctx, package)? =>
        {
            match webpack_output_directory(ctx.app, package)? {
                Some(directory) => Some(StaticSite {
                    directory,
                    spa: true,
                }),
                None => return Err(autopack_core::Error::MissingStartCommand),
            }
        }
        _ => None,
    };
    Ok(site)
}

/// Whether a webpack app bundles a browser page rather than a server.
///
/// webpack builds servers and libraries as readily as SPAs, so its presence
/// says nothing about the output. `html-webpack-plugin` does: it is what
/// emits the `index.html` a static server needs. The build script has to run
/// webpack too, or the configured output is never produced.
fn is_webpack_spa(package: &PackageJson) -> bool {
    package.has_dependency("html-webpack-plugin")
        && package
            .script("build")
            .is_some_and(|script| !webpack_invocations(script).is_empty())
}

/// Root-level files that start a Node server. `src/index.js` is left out on
/// purpose: it is webpack's default *browser* entry.
const SERVER_ENTRY_POINTS: &[&str] =
    &["server.js", "server.mjs", "index.js", "index.mjs", "app.js"];

/// Whether something besides the bundle says the app runs a server: a start
/// command the user supplied, a Procfile `web:` line, a `main` outside `src/`,
/// or a root-level server entry point. An Express app that also builds its
/// client with webpack is a server, as it was before webpack was recognised.
fn has_server_entry(ctx: &BuildContext<'_>, package: &PackageJson) -> Result<bool> {
    let user_start_command = ctx.env.config("START_CMD").is_some()
        || ctx
            .config
            .deploy
            .as_ref()
            .is_some_and(|deploy| deploy.start_command.is_some());
    let server_main = package.main.as_deref().is_some_and(|main| {
        !main.trim_start_matches("./").starts_with("src/") && ctx.app.has_file(main)
    });
    Ok(user_start_command
        || server_main
        || procfile_web_command(ctx.app)?.is_some()
        || SERVER_ENTRY_POINTS
            .iter()
            .any(|entry| ctx.app.has_file(entry)))
}

/// Config files webpack-cli loads when no `--config` is given, in its order.
const WEBPACK_CONFIGS: &[&str] = &[
    "webpack.config.js",
    "webpack.config.mjs",
    "webpack.config.cjs",
    "webpack.config.ts",
    "webpack.config.mts",
    "webpack.config.cts",
];

/// Where a webpack build writes its output, relative to the app root.
///
/// `None` when it cannot be told without running the config: guessing would
/// serve an empty directory, which is a worse failure than asking for
/// `AUTOPACK_STATIC_DIR`. A config file is required: without one
/// `html-webpack-plugin` never runs, so there would be no page to serve.
fn webpack_output_directory(app: &App, package: &PackageJson) -> Result<Option<String>> {
    let script = package.script("build").unwrap_or_default();
    let flags = match webpack_invocations(script).as_slice() {
        [invocation] => match WebpackFlags::parse(invocation) {
            Some(flags) => flags,
            None => return Ok(None),
        },
        // Several webpack runs may write several directories.
        _ => return Ok(None),
    };

    let config_path = match flags.config {
        Some(config) => config,
        None => match WEBPACK_CONFIGS.iter().find(|name| app.has_file(name)) {
            Some(name) => (*name).to_string(),
            // webpack-cli also loads `webpack.config.babel.js`,
            // `.webpack/webpackfile.js` and others; unknown here.
            None => return Ok(None),
        },
    };
    let Some(config) = app.read_file_opt(&config_path)? else {
        return Ok(None);
    };
    if let Some(directory) = flags.output_path {
        let webpack_4 = dependency_major(package, "webpack").is_some_and(|major| major <= 4);
        // webpack-cli 3 reads `-o` as the output *file*.
        if webpack_4 && flags.short_output {
            return Ok(None);
        }
        return Ok(join_inside_app("", &directory));
    }
    let config_dir = config_path
        .trim_start_matches("./")
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    Ok(configured_output_path(&config, config_dir))
}

/// webpack's `output.path` when the config does not set one.
const WEBPACK_DEFAULT_OUTPUT: &str = "dist";

/// The words of each command in `script` that runs webpack, from the word
/// after `webpack` to the end of that command.
fn webpack_invocations(script: &str) -> Vec<Vec<&str>> {
    script
        .split(['&', '|', ';'])
        .filter_map(|command| {
            let mut words = command.split_whitespace();
            words.find(|word| matches!(*word, "webpack" | "webpack-cli"))?;
            Some(words.collect())
        })
        .collect()
}

/// The webpack-cli flags that change which config is read or where it writes.
#[derive(Debug, Default, PartialEq, Eq)]
struct WebpackFlags {
    config: Option<String>,
    output_path: Option<String>,
    /// The output path came from `-o`, which webpack-cli 3 reads as a file.
    short_output: bool,
}

impl WebpackFlags {
    /// `None` when a relevant flag has a value that cannot be read: one
    /// split by the shell's quoting, or the webpack-cli 3 `--output` file.
    fn parse(words: &[&str]) -> Option<Self> {
        let mut flags = Self::default();
        let mut words = words.iter().copied();
        while let Some(word) = words.next() {
            let (name, inline) = match word.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (word, None),
            };
            let is_output = match name {
                "--config" | "-c" => false,
                "--output-path" | "-o" => true,
                "--output" => return None,
                _ => continue,
            };
            let value = unquote(inline.or_else(|| words.next())?)?;
            if is_output {
                flags.output_path = Some(value.to_string());
                flags.short_output = name == "-o";
            } else {
                flags.config = Some(value.to_string());
            }
        }
        Some(flags)
    }
}

/// A shell word without its surrounding quotes, if it is complete.
fn unquote(word: &str) -> Option<&str> {
    for quote in ['\'', '"'] {
        if let Some(rest) = word.strip_prefix(quote) {
            return rest
                .strip_suffix(quote)
                .filter(|inner| !inner.contains(quote));
        }
    }
    (!word.is_empty() && !word.contains(['\'', '"'])).then_some(word)
}

/// The major version of a declared dependency.
fn dependency_major(package: &PackageJson, name: &str) -> Option<u64> {
    package
        .dependencies
        .get(name)
        .or_else(|| package.dev_dependencies.get(name))
        .and_then(|range| {
            crate::version::Version::parse(range.trim_start_matches(|c: char| !c.is_ascii_digit()))
        })
        .map(|version| version.major)
}

/// The directory a webpack config's `output.path` names, relative to the app.
///
/// Read as text rather than evaluated, like `next.config.*`. Recognises the
/// forms configs are written in practice — `path.resolve(__dirname, 'build')`,
/// `path.join(...)`, a bare `resolve(...)` — and a config with no
/// `output.path`, which writes to `dist`. `__dirname` is the config's own
/// directory (`config_dir`); `process.cwd()` is the app root. Several configs
/// (an exported array) must agree.
///
/// Anything the parser does not fully account for is `None`: an `output` it
/// could not read (shorthand, quoted key, `config.output`, a spread), or a
/// config that pulls in another local file (`webpack-merge` over a common
/// config) without naming `output.path` itself. `merge()` combines `output`
/// deeply, so the common config's `path` survives an `output` block here
/// that only sets `filename`.
fn configured_output_path(config: &str, config_dir: &str) -> Option<String> {
    let normalised = normalise_config(config);
    let values = object_values(&normalised, "output");
    if values.len() != mentions(&normalised, "output") {
        return None;
    }
    let inherits_config = imports_local_config(&normalised);
    if values.is_empty() && inherits_config {
        return None;
    }
    let mut directories = Vec::new();
    for value in values {
        let ObjectValue::Literal(body) = value else {
            // `output: sharedOutput` — defined elsewhere.
            return None;
        };
        let directory = match top_level_value(body, "path") {
            Some(path) => resolved_directory(path, config_dir)?,
            None if inherits_config => return None,
            None => WEBPACK_DEFAULT_OUTPUT.to_string(),
        };
        directories.push(directory);
    }
    match directories.split_first() {
        None => Some(WEBPACK_DEFAULT_OUTPUT.to_string()),
        Some((first, rest)) if rest.iter().all(|other| other == first) => Some(first.clone()),
        Some(_) => None,
    }
}

/// `config` with comments and whitespace removed outside string literals.
///
/// String contents are kept as written, so `'http://host'` keeps its `//`
/// and a glob such as `'src/**/*.js'` its `/*`.
fn normalise_config(config: &str) -> String {
    let mut normalised = String::with_capacity(config.len());
    let mut chars = config.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                normalised.push(c);
                while let Some(inner) = chars.next() {
                    normalised.push(inner);
                    if inner == '\\' {
                        if let Some(escaped) = chars.next() {
                            normalised.push(escaped);
                        }
                    } else if inner == c {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = ' ';
                for skipped in chars.by_ref() {
                    if previous == '*' && skipped == '/' {
                        break;
                    }
                    previous = skipped;
                }
            }
            c if c.is_whitespace() => {}
            c => normalised.push(c),
        }
    }
    normalised
}

/// The characters of `source` that are outside string literals, with their
/// byte offsets.
fn structural_chars(source: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    source.char_indices().filter(move |&(_, c)| {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == open {
                quote = None;
            }
            return false;
        }
        if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
            return false;
        }
        true
    })
}

/// Whether `c` can continue a JavaScript identifier.
fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// How many times `word` appears as a whole word, strings included, so that
/// a quoted `"output":` key or `config.output` is counted.
fn mentions(source: &str, word: &str) -> usize {
    source
        .match_indices(word)
        .filter(|(start, _)| {
            let before = source[..*start].chars().next_back();
            let after = source[start + word.len()..].chars().next();
            !before.is_some_and(is_identifier_char) && !after.is_some_and(is_identifier_char)
        })
        .count()
}

/// Whether a config pulls in another local file or merges configs.
fn imports_local_config(normalised: &str) -> bool {
    normalised.contains("merge(")
        || ["require(", "from", "import("].iter().any(|keyword| {
            ["'.", "\"."]
                .iter()
                .any(|quote| normalised.contains(&format!("{keyword}{quote}")))
        })
}

/// The value of a `key:` property, wherever it appears in `source`.
enum ObjectValue<'a> {
    /// An object literal; the text between its braces.
    Literal(&'a str),
    /// Anything that is not an object literal.
    Expression,
}

/// Every value assigned to property `key` in normalised `source`.
fn object_values<'a>(source: &'a str, key: &str) -> Vec<ObjectValue<'a>> {
    let pattern = format!("{key}:");
    let mut values = Vec::new();
    for (start, _) in source.match_indices(&pattern) {
        // A property name, not the tail of a longer one (`libraryOutput:`).
        let preceded_by_boundary = source[..start]
            .chars()
            .next_back()
            .is_none_or(|c| matches!(c, '{' | ','));
        if !preceded_by_boundary {
            continue;
        }
        let rest = &source[start + pattern.len()..];
        values.push(match rest.strip_prefix('{').and_then(balanced_body) {
            Some(body) => ObjectValue::Literal(body),
            None => ObjectValue::Expression,
        });
    }
    values
}

/// The text up to the brace that closes one already opened.
fn balanced_body(source: &str) -> Option<&str> {
    let mut depth = 0usize;
    for (index, c) in structural_chars(source) {
        match c {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' if depth == 0 => return (c == '}').then(|| &source[..index]),
            '}' | ')' | ']' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// The value of `key:` among the top-level properties of an object body.
fn top_level_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let mut depth = 0usize;
    let mut property_start = 0;
    let mut properties = Vec::new();
    for (index, c) in structural_chars(body) {
        match c {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                properties.push(&body[property_start..index]);
                property_start = index + 1;
            }
            _ => {}
        }
    }
    properties.push(&body[property_start..]);
    properties
        .into_iter()
        .find_map(|property| property.strip_prefix(key)?.strip_prefix(':'))
}

/// How a config reaches Node's `path` module before calling `resolve`/`join`.
const PATH_MODULE_PREFIXES: &[&str] = &[
    "path.",
    "require('path').",
    "require(\"path\").",
    "require('node:path').",
    "require(\"node:path\").",
];

/// The app-relative directory a `path.resolve(__dirname, ...)` call names.
fn resolved_directory(value: &str, config_dir: &str) -> Option<String> {
    let call = PATH_MODULE_PREFIXES
        .iter()
        .find_map(|prefix| value.strip_prefix(prefix))
        .unwrap_or(value);
    let arguments = call
        .strip_prefix("resolve(")
        .or_else(|| call.strip_prefix("join("))?
        .strip_suffix(')')?;
    let mut arguments = arguments.split(',');
    let base = match arguments.next()? {
        "__dirname" | "import.meta.dirname" => config_dir,
        "process.cwd()" => "",
        _ => return None,
    };
    let segments: Option<Vec<&str>> = arguments.map(string_literal).collect();
    join_inside_app(base, &segments?.join("/"))
}

/// The contents of a quoted string with nothing interpolated into it.
fn string_literal(value: &str) -> Option<&str> {
    let quote = value.chars().next()?;
    if !matches!(quote, '\'' | '"' | '`') {
        return None;
    }
    let inner = value.strip_prefix(quote)?.strip_suffix(quote)?;
    (!inner.contains(quote) && !inner.contains("${") && !inner.contains('\\')).then_some(inner)
}

/// `relative` resolved against the app-relative `base`, or `None` if it is
/// absolute, leaves the app, is the app root itself, or names a directory a
/// Caddyfile cannot hold unquoted.
fn join_inside_app(base: &str, relative: &str) -> Option<String> {
    if relative.starts_with('/') {
        return None;
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in base.split('/').chain(relative.split('/')) {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }
    let safe = segments.iter().all(|segment| {
        segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'))
    });
    (!segments.is_empty() && safe).then(|| segments.join("/"))
}

/// What a non-Node provider learns from [`plan_front_end`].
pub(crate) struct FrontEnd {
    /// The package manager the app uses.
    pub(crate) manager: PackageManager,
    /// Whether `package.json` defines a `build` script.
    pub(crate) has_build_script: bool,
}

/// Install Node, the app's package manager and its JavaScript dependencies in
/// `step_name`, for an app another provider builds.
///
/// Server frameworks compile their front end with Node during their own build
/// — Laravel through Vite or Mix, Rails through jsbundling, cssbundling or
/// Webpacker — and all of it fails without `node` and `node_modules`. The
/// commands are added to the step before the provider's own, so the
/// dependencies are in place when its asset task runs. Returns `None` when the
/// app has no `package.json`.
pub(crate) fn plan_front_end(
    ctx: &mut BuildContext<'_>,
    step_name: &str,
) -> Result<Option<FrontEnd>> {
    let Some(package) = ctx.app.read_json_opt::<PackageJson>("package.json")? else {
        return Ok(None);
    };
    let manager = PackageManager::detect(ctx.app, &package);
    let (node_version, version_source) = node_version(ctx.app, &package)?;
    ctx.packages.add("node", &node_version, version_source);
    if let Some((tool, version)) = manager.mise_tool(&package) {
        ctx.packages.add(tool, version, "packageManager / lockfile");
    }
    if manager.needs_libatomic(&package, ctx.lock()) {
        ctx.build_apt_packages.push("libatomic1".to_string());
    }
    ctx.add_metadata("frontEnd", manager.id());
    ctx.add_metadata("nodeVersion", &node_version);

    let install = manager.install_command(ctx.app, &package, ctx.lock());
    let legacy_openssl = needs_legacy_openssl(&package, &node_version);
    let (cache_dir, cache_env) = manager.cache();
    let cache = ctx.shared_cache(format!("{}-store", manager.id()), cache_dir);

    let step = ctx.step(step_name);
    step.add_cache(cache);
    for (key, value) in cache_env {
        step.add_variable(key, value);
    }
    if manager == PackageManager::Pnpm {
        // Docker RUN has no TTY; pnpm prompts to purge node_modules otherwise.
        step.add_variable("CI", "true");
    }
    if legacy_openssl {
        step.add_variable("NODE_OPTIONS", LEGACY_OPENSSL_OPTION);
    }
    step.add_command(Command::shell(install));

    Ok(Some(FrontEnd {
        manager,
        has_build_script: package.script("build").is_some(),
    }))
}

/// Lets webpack 4 hash with MD4 on Node 17+, whose OpenSSL 3 dropped it.
const LEGACY_OPENSSL_OPTION: &str = "--openssl-legacy-provider";

/// Whether the build runs webpack 4 on a Node new enough to need
/// [`LEGACY_OPENSSL_OPTION`].
///
/// Webpack 4 hashes modules with MD4, which OpenSSL 3 (Node 17+) no longer
/// provides; the build dies with `ERR_OSSL_EVP_UNSUPPORTED`. It arrives
/// directly or through the toolchains built on it: Webpacker 5 and older,
/// Create React App 4, Vue CLI 4 and Laravel Mix 5. Node 16 and older reject
/// the flag outright, so it is only set when the Node being installed has it.
fn needs_legacy_openssl(package: &PackageJson, node_version: &str) -> bool {
    let major = |name: &str| {
        package
            .dependencies
            .get(name)
            .or_else(|| package.dev_dependencies.get(name))
            .and_then(|range| {
                crate::version::Version::parse(
                    range.trim_start_matches(|c: char| !c.is_ascii_digit()),
                )
            })
            .map(|version| version.major)
    };
    let webpack4 = major("webpack").is_some_and(|m| m <= 4)
        || major("@rails/webpacker").is_some_and(|m| m <= 5)
        || major("react-scripts").is_some_and(|m| m <= 4)
        || major("@vue/cli-service").is_some_and(|m| m <= 4)
        || major("laravel-mix").is_some_and(|m| m <= 5);
    // An alias (`lts`, `latest`) is always a current release.
    let modern_node =
        crate::version::Version::parse(node_version).is_none_or(|version| version.major >= 17);
    webpack4 && modern_node
}

/// The Node version to install, and where it was found.
fn node_version(app: &App, package: &PackageJson) -> Result<(String, String)> {
    for file in [".nvmrc", ".node-version"] {
        if let Some(version) = read_version_file(app, file)? {
            if let Some(version) = normalize_version_range(&version) {
                return Ok((version, file.to_string()));
            }
        }
    }

    if let Some(engine) = package.engines.get("node") {
        if let Some(version) = normalize_version_range(engine) {
            return Ok((version, "package.json engines.node".to_string()));
        }
    }

    Ok((DEFAULT_NODE_VERSION.to_string(), "autopack default".into()))
}

/// Browser tooling an app needs wired up: the system libraries the browser
/// links against, where it is cached, and how it is fetched.
struct BrowserTooling {
    /// Debian packages the runtime image needs that ELF inspection cannot discover.
    ///
    /// Fonts only. A browser does not link them — it opens them through
    /// fontconfig at run time — so nothing about the binary reveals that they
    /// are needed, and without them Chromium renders text as empty boxes
    /// instead of failing in a way anyone would notice.
    runtime_packages: Vec<String>,
    /// Shell expressions that list the browser binaries to inspect.
    browser_searches: Vec<String>,
    /// Environment for the install, build and runtime stages.
    variables: Vec<(&'static str, String)>,
    /// Commands the install step runs after dependencies are in place.
    downloads: Vec<String>,
}

impl BrowserTooling {
    fn is_empty(&self) -> bool {
        self.runtime_packages.is_empty() && self.variables.is_empty()
    }
}

/// Work out how to make a browser available to the app at run time.
///
/// Puppeteer and Playwright both fetch a browser out of band, by default into
/// a cache under `$HOME`. Neither location survives into the runtime image:
/// the deploy layer carries only `/app`, and the two stages do not even agree
/// on `$HOME` — the build runs as root while the runtime user is `autopack`.
/// The result is a build that looks clean and an app that fails on first use
/// with "Could not find Chrome ... cache path is ...
/// /home/autopack/.cache/puppeteer". Pointing both tools at `/app` makes the
/// browser travel with the app.
///
/// They differ in how the browser arrives: Puppeteer downloads it from its own
/// postinstall hook, while Playwright leaves it to an explicit
/// `playwright install` and otherwise fails at launch telling the user to run
/// it. Only Chromium is fetched, which is the browser the image carries system
/// libraries for.
///
/// Gated on a declared *runtime* dependency rather than on the text of the
/// manifest, which is what the apt table elsewhere uses. Two reasons the looser
/// test does not survive here. Fetching a browser costs a 300MB image and a
/// command run as root, so a blog whose description mentions Playwright should
/// not pay for it — and `@playwright/test` is a devDependency of a great many
/// frontend repos that ship a bundle of static files and never launch
/// anything. An app that drives a browser in production declares it in
/// `dependencies`; that is the contract.
///
/// The `-core` packages are absent by the same logic: they never fetch a
/// browser, and their usual job is driving a remote one over CDP, so the local
/// library closure would be bloat.
fn browser_tooling(package: &PackageJson) -> BrowserTooling {
    let mut tooling = BrowserTooling {
        runtime_packages: Vec::new(),
        browser_searches: Vec::new(),
        variables: Vec::new(),
        downloads: Vec::new(),
    };

    let playwright = ["playwright", "@playwright/test"]
        .iter()
        .any(|name| package.dependencies.contains_key(*name));
    let puppeteer = package.dependencies.contains_key("puppeteer");

    if playwright {
        // `0` is Playwright's own opt-in for "install beside the package",
        // which puts the browser under node_modules and therefore under /app.
        tooling
            .variables
            .push(("PLAYWRIGHT_BROWSERS_PATH", "0".to_string()));
        // `--no-install` rather than `--yes`: the dependency is installed by
        // the command just above, so a resolution failure means the detection
        // was wrong, and failing is better than fetching an unpinned package
        // from the registry and running it as root. `npx` rather than the
        // detected package manager's runner because npm ships with Node, so
        // this works the same for a pnpm, yarn or bun project.
        tooling
            .downloads
            .push("npx --no-install playwright install chromium".to_string());
    }
    if puppeteer {
        tooling
            .variables
            .push(("PUPPETEER_CACHE_DIR", format!("{APP_DIR}/.cache/puppeteer")));
    }
    // `find` rather than a positional glob. pnpm does not hoist
    // `playwright-core` to the top of node_modules — the browsers land under
    // `.pnpm/playwright-core@1.62.1/node_modules/...` — so a glob written for
    // npm's layout matches nothing there, and matching nothing is silent: the
    // recorded list comes out empty and the image ships with no browser
    // libraries at all.
    //
    // Both binaries are collected. Playwright launches the headless shell by
    // default and Puppeteer the full browser, they sit in differently named
    // directories, and their link sets are not identical.
    if playwright {
        tooling.browser_searches.push(format!(
            "find {APP_DIR}/node_modules -path '*/{PLAYWRIGHT_BROWSERS}/*' -type f \\
             \\( -name chrome -o -name chrome-headless-shell -o -name headless_shell \\) \\
             2>/dev/null"
        ));
    }
    if puppeteer {
        tooling.browser_searches.push(format!(
            "find {APP_DIR}/.cache/puppeteer -type f \\
             \\( -name chrome -o -name chrome-headless-shell \\) 2>/dev/null"
        ));
    }
    if playwright || puppeteer {
        tooling.runtime_packages = CHROMIUM_FONTS
            .iter()
            .map(|package| (*package).to_string())
            .collect();
    }

    tooling
}

/// True when a script body is a single command that can be exec'd directly.
fn is_simple_command(script: &str) -> bool {
    !script.contains("&&")
        && !script.contains("||")
        && !script.contains(';')
        && !script.contains('|')
        && !script.contains('&')
        && !script.contains('>')
        && !script.contains('<')
        && !script.contains('$')
        && !script.contains('`')
}

#[cfg(test)]
mod tests {
    use super::{
        configured_output_path, needs_legacy_openssl, PackageJson, WebpackFlags,
        PLAYWRIGHT_BROWSERS, RUNTIME_DEPS_FILE,
    };
    use crate::test_support::{plan_for, plan_with_env, try_plan_for, write_app};
    use autopack_core::APP_DIR;

    #[test]
    fn webpack_4_toolchains_get_the_legacy_openssl_provider_on_modern_node() {
        let package = |deps: &str| -> PackageJson {
            serde_json::from_str(&format!(r#"{{"dependencies":{{{deps}}}}}"#)).unwrap()
        };
        assert!(needs_legacy_openssl(
            &package(r#""react-scripts":"4.0.3""#),
            "24"
        ));
        assert!(needs_legacy_openssl(
            &package(r#""@rails/webpacker":"^5.4.0""#),
            "lts"
        ));
        assert!(needs_legacy_openssl(
            &package(r#""webpack":"^4.46.0""#),
            "20"
        ));
        assert!(needs_legacy_openssl(
            &package(r#""laravel-mix":"^5.0.1""#),
            "18"
        ));
        // Node 16 rejects the flag, and webpack 5 does not need it.
        assert!(!needs_legacy_openssl(
            &package(r#""react-scripts":"4.0.3""#),
            "16"
        ));
        assert!(!needs_legacy_openssl(
            &package(r#""webpack":"^5.90.0""#),
            "24"
        ));
        assert!(!needs_legacy_openssl(
            &package(r#""react-scripts":"5.0.1""#),
            "24"
        ));
    }

    #[test]
    fn a_create_react_app_4_build_sets_the_legacy_openssl_provider() {
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"scripts":{"build":"react-scripts build"},"dependencies":{"react-scripts":"4.0.3"}}"#,
            ),
            ("package-lock.json", "{}"),
        ]);
        let plan = plan_for(&app).plan;
        assert_eq!(
            plan.step("build").unwrap().variables["NODE_OPTIONS"],
            "--openssl-legacy-provider"
        );
    }

    #[test]
    fn detects_npm_and_plans_install_and_build() {
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"name":"api","scripts":{"build":"tsc","start":"node dist/index.js"}}"#,
            ),
            ("package-lock.json", "{}"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "node");
        assert_eq!(analysis.metadata["packageManager"], "npm");
        assert_eq!(
            analysis.plan.step("install").unwrap().commands[0].display_name(),
            "npm ci"
        );
        assert_eq!(
            analysis.plan.step("build").unwrap().commands[0].display_name(),
            "npm run build"
        );
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("node dist/index.js")
        );
    }

    #[test]
    fn locally_installed_cli_tools_are_on_the_runtime_path() {
        // `next start` resolves to node_modules/.bin/next. Exec'ing it without
        // that directory on PATH fails with "exec: next: not found".
        let (_dir, app) = write_app(&[(
            "package.json",
            r#"{"dependencies":{"next":"15"},"scripts":{"build":"next build","start":"next start"}}"#,
        )]);
        let analysis = plan_for(&app);
        assert!(
            analysis
                .plan
                .deploy
                .paths
                .contains(&"/app/node_modules/.bin".to_string()),
            "{:?}",
            analysis.plan.deploy.paths
        );
    }

    #[test]
    fn compound_start_scripts_go_through_the_package_manager() {
        let (_dir, app) = write_app(&[(
            "package.json",
            r#"{"scripts":{"start":"npm run migrate && node index.js"}}"#,
        )]);
        let analysis = plan_for(&app);
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("npm run start")
        );
    }

    /// A React app bundled by webpack 5 with `html-webpack-plugin`, the shape
    /// of a Module Federation host or remote.
    const WEBPACK_SPA_PACKAGE: &str = r#"{
        "scripts": {"dev": "webpack serve --mode development", "build": "webpack --mode production"},
        "dependencies": {"react": "^18.3.1", "react-dom": "^18.3.1"},
        "devDependencies": {"html-webpack-plugin": "^5.6.0", "webpack": "^5.94.0", "webpack-cli": "^5.1.4"}
    }"#;

    /// A webpack 5 SPA whose build script is `build`.
    fn webpack_package(build: &str) -> String {
        format!(
            r#"{{"scripts":{{"build":"{build}"}},
                "devDependencies":{{"html-webpack-plugin":"5","webpack":"^5.94.0"}}}}"#
        )
    }

    /// What a planned app serves, or `None` when planning fails.
    fn serves(files: &[(&str, &str)]) -> Option<String> {
        let (_dir, app) = write_app(files);
        let analysis = try_plan_for(&app).ok()?;
        analysis.metadata.get("serve").cloned()
    }

    /// A Module Federation-style config with no `output.path` writes `dist`.
    #[test]
    fn webpack_spas_are_served_as_static_sites_from_dist() {
        let (_dir, app) = write_app(&[
            ("package.json", WEBPACK_SPA_PACKAGE),
            ("package-lock.json", "{}"),
            (
                "webpack.config.js",
                r#"const HtmlWebpackPlugin = require('html-webpack-plugin');
const { ModuleFederationPlugin } = require('webpack').container;
module.exports = {
  entry: './src/index', // bootstraps the remotes
  output: { publicPath: 'auto', clean: true },
  devServer: { port: 3000, historyApiFallback: true },
  plugins: [
    new ModuleFederationPlugin({ name: 'host', remotes: { shop: 'shop@http://localhost:3001/remoteEntry.js' } }),
    new HtmlWebpackPlugin({ template: './src/index.html' }),
  ],
};"#,
            ),
            ("src/index.html", r#"<div id="root"></div>"#),
            // webpack's default browser entry is not a server entry point.
            ("src/index.js", "import('./bootstrap');"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.metadata["framework"], "webpack");
        assert_eq!(analysis.metadata["serve"], "caddy static (dist)");
        assert!(analysis
            .plan
            .deploy
            .start_command
            .as_deref()
            .unwrap()
            .starts_with("caddy run"));
        assert_eq!(
            analysis.plan.step("build").unwrap().commands[0].display_name(),
            "npm run build"
        );
    }

    /// `output.path` set with `path.resolve(__dirname, ...)` is served.
    #[test]
    fn webpack_spas_serve_the_configured_output_path() {
        assert_eq!(
            serves(&[
                ("package.json", WEBPACK_SPA_PACKAGE),
                (
                    "webpack.config.js",
                    "const path = require('path');\nmodule.exports = {\n  output: {\n    filename: '[name].[contenthash].js',\n    path: path.resolve(__dirname, 'build'),\n  },\n};",
                ),
            ])
            .as_deref(),
            Some("caddy static (build)")
        );
    }

    /// Planning stops rather than starting the browser entry under Node.
    #[test]
    fn a_webpack_spa_whose_output_cannot_be_read_fails_planning() {
        for config in [
            // No config: html-webpack-plugin never runs without one.
            None,
            Some("module.exports = { output: { path: outputDirectory() } };"),
        ] {
            let mut files = vec![
                ("package.json", WEBPACK_SPA_PACKAGE),
                // Must not be started under Node as a fallback entry point.
                ("src/index.js", "document.body.textContent = 'hi';"),
            ];
            if let Some(config) = config {
                files.push(("webpack.config.js", config));
            }
            let (_dir, app) = write_app(&files);
            let error = try_plan_for(&app).unwrap_err();
            assert!(
                matches!(error, autopack_core::Error::MissingStartCommand),
                "{config:?}: {error}"
            );
            assert!(error.to_string().contains("AUTOPACK_STATIC_DIR"), "{error}");

            let analysis = plan_with_env(&app, &[("AUTOPACK_STATIC_DIR", "out")]).unwrap();
            assert_eq!(analysis.metadata["serve"], "caddy static (out)");
            // A start command the user supplies makes it a server instead.
            let analysis = plan_with_env(&app, &[("AUTOPACK_START_CMD", "node api.js")]).unwrap();
            assert_eq!(
                analysis.plan.deploy.start_command.as_deref(),
                Some("node api.js")
            );
        }
    }

    /// A server entry point wins over a webpack client bundle.
    #[test]
    fn a_server_that_builds_its_client_with_webpack_stays_a_server() {
        let config = (
            "webpack.config.js",
            "module.exports = { output: { path: require('path').resolve(__dirname, 'public') } };",
        );
        let start = |files: &[(&str, &str)]| {
            let (_dir, app) = write_app(files);
            plan_for(&app).plan.deploy.start_command
        };
        let package = webpack_package("webpack");
        assert_eq!(
            start(&[("package.json", &package), config, ("server.js", "")]).as_deref(),
            Some("node server.js")
        );
        assert_eq!(
            start(&[
                ("package.json", &package),
                config,
                ("Procfile", "web: node api.js"),
            ])
            .as_deref(),
            Some("node api.js")
        );
        let with_main = r#"{"main":"lib/api.js","scripts":{"build":"webpack"},
            "devDependencies":{"html-webpack-plugin":"5","webpack":"5"}}"#;
        assert_eq!(
            start(&[("package.json", with_main), config, ("lib/api.js", "")]).as_deref(),
            Some("node lib/api.js")
        );
    }

    /// Without `html-webpack-plugin` there is no page to serve.
    #[test]
    fn webpack_without_an_html_page_runs_a_server() {
        // webpack bundling a Node server: no html-webpack-plugin, a start script.
        let (_dir, app) = write_app(&[(
            "package.json",
            r#"{"scripts":{"build":"webpack","start":"node dist/main.js"},
                "devDependencies":{"webpack":"5","webpack-cli":"5"}}"#,
        )]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["framework"], "webpack");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("node dist/main.js")
        );
    }

    /// A start script says the author runs a server.
    #[test]
    fn webpack_spas_with_a_start_script_run_it() {
        let (_dir, app) = write_app(&[(
            "package.json",
            r#"{"scripts":{"build":"webpack","start":"node server.js"},
                "devDependencies":{"html-webpack-plugin":"5","webpack":"5"}}"#,
        )]);
        assert_eq!(
            plan_for(&app).plan.deploy.start_command.as_deref(),
            Some("node server.js")
        );
    }

    /// `--output-path` and webpack 5's `-o` decide the directory.
    #[test]
    fn webpack_cli_output_flags_override_the_config() {
        let config = ("webpack.config.js", "module.exports = {};");
        assert_eq!(
            serves(&[
                (
                    "package.json",
                    &webpack_package("webpack --mode production --output-path public/bundle")
                ),
                config,
            ])
            .as_deref(),
            Some("caddy static (public/bundle)")
        );
        assert_eq!(
            serves(&[("package.json", &webpack_package("webpack -o out")), config]).as_deref(),
            Some("caddy static (out)")
        );
        // webpack-cli 3 reads `-o` as the output file.
        let webpack_4 = r#"{"scripts":{"build":"webpack -o dist/bundle.js"},
            "devDependencies":{"html-webpack-plugin":"4","webpack":"^4.46.0"}}"#;
        assert_eq!(serves(&[("package.json", webpack_4), config]), None);
    }

    /// Flags of other tools in the build script are not webpack's.
    #[test]
    fn only_the_webpack_command_s_own_flags_are_read() {
        let files = |build: String| {
            vec![
                ("package.json".to_string(), build),
                (
                    "webpack.config.js".to_string(),
                    "module.exports = { output: { path: require('path').resolve(__dirname, 'build') } };"
                        .to_string(),
                ),
                (".eslintrc.json".to_string(), "{}".to_string()),
            ]
        };
        for build in [
            "eslint -c .eslintrc.json src && webpack",
            "node gen.js -o src/generated && webpack",
            "webpack && purgecss -c p.js -o build/css",
        ] {
            let files = files(webpack_package(build));
            let files: Vec<(&str, &str)> = files
                .iter()
                .map(|(name, contents)| (name.as_str(), contents.as_str()))
                .collect();
            assert_eq!(
                serves(&files).as_deref(),
                Some("caddy static (build)"),
                "{build}"
            );
        }
        // Two webpack runs may write two directories.
        let files = files(webpack_package(
            "webpack --config a.js && webpack --config b.js",
        ));
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, contents)| (name.as_str(), contents.as_str()))
            .collect();
        assert_eq!(serves(&files), None);
    }

    /// `__dirname` is the config file's directory, not the app root.
    #[test]
    fn a_named_config_resolves_dirname_against_its_own_directory() {
        let package = webpack_package("webpack --config config/webpack.prod.js");
        let serve = |path: &str| {
            let config = format!(
                "const path = require('path');\nmodule.exports = {{ output: {{ path: {path} }} }};"
            );
            serves(&[
                ("package.json", &package),
                ("webpack.config.js", "module.exports = { output: { path: require('path').resolve(__dirname, 'dev') } };"),
                ("config/webpack.prod.js", &config),
            ])
        };
        assert_eq!(
            serve("path.resolve(__dirname, '../release')").as_deref(),
            Some("caddy static (release)")
        );
        assert_eq!(
            serve("path.resolve(__dirname, 'dist')").as_deref(),
            Some("caddy static (config/dist)")
        );
        assert_eq!(
            serve("path.resolve(process.cwd(), 'dist')").as_deref(),
            Some("caddy static (dist)")
        );
    }

    /// The config forms the parser reads, with the directory each names.
    #[test]
    fn webpack_output_paths_are_read_from_common_config_forms() {
        let read = |config: &str| configured_output_path(config, "");

        assert_eq!(read("module.exports = {}").as_deref(), Some("dist"));
        assert_eq!(
            read("export default { output: { path: path.join(__dirname, 'www') } }").as_deref(),
            Some("www")
        );
        assert_eq!(
            read("module.exports = { output: { path: resolve(process.cwd(), \"out\", \"web\") } }")
                .as_deref(),
            Some("out/web")
        );
        assert_eq!(
            read(
                "export default { output: { path: path.resolve(import.meta.dirname, './dist/') } }"
            )
            .as_deref(),
            Some("dist")
        );
        // Nested `path` keys belong to other options, not to output.path.
        assert_eq!(
            read("module.exports = { output: { library: { path: 'x' }, clean: true } }").as_deref(),
            Some("dist")
        );
        // `libraryOutput:` is not `output:`.
        assert_eq!(
            read("module.exports = { experiments: { libraryOutput: { path: 'x' } } }").as_deref(),
            Some("dist")
        );
        // Comments are not config, wherever they sit.
        assert_eq!(
            read("module.exports = {\n  // output: { path: path.resolve(__dirname, 'old') },\n  entry: './src',\n}")
                .as_deref(),
            Some("dist")
        );
        assert_eq!(
            read("module.exports = {\n  mode: 'production', // prod\n  output: { path: path.resolve(__dirname, 'build') },\n}")
                .as_deref(),
            Some("build")
        );
        assert_eq!(
            read("module.exports = { /* x */ output: { path: path.resolve(__dirname, 'build') } }")
                .as_deref(),
            Some("build")
        );
        // Braces and `//` inside strings are not structure or comments.
        assert_eq!(
            read("module.exports = { output: { publicPath: 'http://cdn/}{', path: path.resolve(__dirname, 'build') } }")
                .as_deref(),
            Some("build")
        );
        // Configs exported as an array must agree.
        assert_eq!(
            read("module.exports = [{ output: { path: path.resolve(__dirname, 'a') } }, { output: {} }]"),
            None
        );
        assert_eq!(
            read("module.exports = [{ output: { clean: true } }, { output: {} }]").as_deref(),
            Some("dist")
        );
        // A local helper does not matter when output is spelled out here.
        assert_eq!(
            read("const paths = require('./paths');\nmodule.exports = { output: { path: path.resolve(__dirname, 'build') } }")
                .as_deref(),
            Some("build")
        );
    }

    /// Forms whose directory cannot be told without running the config.
    #[test]
    fn webpack_output_paths_that_cannot_be_read_safely_are_rejected() {
        for config in [
            "module.exports = { output: sharedOutput }",
            "module.exports = { entry, output }",
            "module.exports = { \"output\": { \"path\": \"/srv\" } }",
            "config.output.path = path.resolve(__dirname, 'build'); module.exports = config;",
            "module.exports = { output: { ...base.output } }",
            "const common = require('./webpack.common');\nmodule.exports = merge(common, { mode: 'production' });",
            "import common from './webpack.common.js';\nexport default { ...common, mode: 'production' };",
            "const common = require('./webpack.common');\nmodule.exports = merge(common, { mode: 'production', output: { filename: '[name].[contenthash].js' } });",
            "module.exports = { output: { path: '/var/www' } }",
            "module.exports = { output: { path: path.resolve(__dirname, '../shared') } }",
            "module.exports = { output: { path: path.resolve('/srv', 'dist') } }",
            "module.exports = { output: { path: path.resolve(__dirname, `${target}`) } }",
            "module.exports = { output: { path: path.resolve(__dirname, outDir) } }",
            "module.exports = { output: { path: path.resolve(__dirname, 'my site') } }",
        ] {
            assert_eq!(configured_output_path(config, ""), None, "{config}");
        }
    }

    /// `--flag value` and `--flag=value`, and the unreadable cases.
    #[test]
    fn webpack_cli_flags_are_parsed_in_both_spellings() {
        let parse = |words: &str| {
            let words: Vec<&str> = words.split_whitespace().collect();
            WebpackFlags::parse(&words)
        };
        assert_eq!(
            parse("--config=config/prod.js -o build"),
            Some(WebpackFlags {
                config: Some("config/prod.js".into()),
                output_path: Some("build".into()),
                short_output: true,
            })
        );
        assert_eq!(
            parse("-c 'webpack.prod.js' --output-path=\"out\""),
            Some(WebpackFlags {
                config: Some("webpack.prod.js".into()),
                output_path: Some("out".into()),
                short_output: false,
            })
        );
        assert_eq!(parse("--mode production"), Some(WebpackFlags::default()));
        // A quoted value the shell split, an empty one, a dangling flag, and
        // webpack-cli 3's output *file* are all unreadable.
        assert_eq!(parse("--output-path='my dir'"), None);
        assert_eq!(parse("-c="), None);
        assert_eq!(parse("--config"), None);
        assert_eq!(parse("--output dist/bundle.js"), None);
    }

    /// An ESM service whose only script is `dev` still gets a start command.
    #[test]
    fn an_esm_entry_point_under_src_is_started_directly() {
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"type":"module","scripts":{"dev":"node --watch src/index.mjs"},"dependencies":{"express":"^4.19.2"}}"#,
            ),
            ("src/index.mjs", "import express from 'express';"),
        ]);
        assert_eq!(
            plan_for(&app).plan.deploy.start_command.as_deref(),
            Some("node src/index.mjs")
        );
    }

    #[test]
    fn vite_apps_are_served_as_static_sites() {
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"dependencies":{"vite":"^5"},"scripts":{"build":"vite build"}}"#,
            ),
            ("index.html", "<html></html>"),
        ]);
        let analysis = plan_for(&app);

        assert!(analysis
            .plan
            .deploy
            .start_command
            .as_deref()
            .unwrap()
            .starts_with("caddy run"));
        assert!(analysis
            .plan
            .deploy
            .inputs
            .iter()
            .any(|input| input.step.as_deref() == Some(crate::support::CADDY_STEP)));
        // Node is a build-time dependency only.
        assert!(!analysis
            .plan
            .deploy
            .paths
            .iter()
            .any(|p| p.contains("mise")));
    }

    #[test]
    fn next_apps_run_a_server() {
        let (_dir, app) = write_app(&[(
            "package.json",
            r#"{"dependencies":{"next":"14","vite":"5"},"scripts":{"build":"next build","start":"next start"}}"#,
        )]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.metadata["framework"], "next");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("next start")
        );
        assert_eq!(analysis.plan.deploy.variables["NODE_ENV"], "production");
    }

    #[test]
    fn next_build_reuses_a_locked_next_cache() {
        let (_dir, app) = write_app(&[(
            "package.json",
            r#"{"dependencies":{"next":"15"},"scripts":{"build":"next build","start":"next start"}}"#,
        )]);
        let analysis = plan_for(&app);

        let build = analysis.plan.step("build").unwrap();
        assert!(
            build
                .caches
                .iter()
                .any(|name| name.starts_with("next-cache-")),
            "build step caches: {:?}",
            build.caches
        );
        let cache = &analysis.plan.caches[&build.caches[0]];
        assert_eq!(cache.directory, format!("{APP_DIR}/.next/cache"));
        assert_eq!(cache.cache_type, autopack_core::plan::CacheType::Locked);
    }

    #[test]
    fn next_caches_are_stable_and_isolated_between_checkouts() {
        let manifest = r#"{"dependencies":{"next":"15"},"scripts":{"build":"next build","start":"next start"}}"#;
        let (_first, app) = write_app(&[("package.json", manifest)]);
        let (_second, other) = write_app(&[("package.json", manifest)]);
        let names =
            |app: &autopack_core::App| plan_for(app).plan.step("build").unwrap().caches.clone();
        assert_eq!(names(&app), names(&app));
        assert_ne!(names(&app), names(&other));
    }

    #[test]
    fn delegated_workspace_build_caches_each_next_package() {
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"workspaces":["apps/*"],"scripts":{"build":"cd apps/web && npm run build","start":"cd apps/web && npm start"}}"#,
            ),
            (
                "apps/web/package.json",
                r#"{"dependencies":{"next":"15"},"scripts":{"build":"next build"}}"#,
            ),
            (
                "apps/admin/package.json",
                r#"{"dependencies":{"next":"15"}}"#,
            ),
            (
                "apps/other/package.json",
                r#"{"dependencies":{"vite":"5"}}"#,
            ),
        ]);
        let analysis = plan_for(&app);
        let build = analysis.plan.step("build").unwrap();
        let directories: Vec<_> = build
            .caches
            .iter()
            .map(|name| analysis.plan.caches[name].directory.as_str())
            .collect();
        assert_eq!(
            directories,
            ["/app/apps/admin/.next/cache", "/app/apps/web/.next/cache"]
        );
        let web_cache = build
            .caches
            .iter()
            .find(|name| analysis.plan.caches[*name].directory == "/app/apps/web/.next/cache")
            .unwrap();
        std::fs::create_dir_all(app.path("apps/aaa")).unwrap();
        std::fs::write(
            app.path("apps/aaa/package.json"),
            r#"{"dependencies":{"next":"15"}}"#,
        )
        .unwrap();
        let refreshed = autopack_core::App::new(app.source()).unwrap();
        let updated = plan_for(&refreshed);
        assert_eq!(
            updated.plan.caches[web_cache].directory,
            "/app/apps/web/.next/cache"
        );
    }

    #[test]
    fn non_next_builds_get_no_next_cache() {
        let (_dir, app) = write_app(&[(
            "package.json",
            r#"{"dependencies":{"vite":"5"},"scripts":{"build":"vite build"}}"#,
        )]);
        let analysis = plan_for(&app);
        assert!(!analysis.plan.caches.contains_key("next-cache"));
    }

    #[test]
    fn next_standalone_ships_only_the_pruned_server() {
        // Shipping /app instead carries every dependency and the whole build
        // output: 3.1GB against 363MB for the Temps landing site.
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"dependencies":{"next":"15"},"scripts":{"build":"next build","start":"next start"}}"#,
            ),
            ("next.config.ts", "export default { output: 'standalone' };"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.metadata["nextOutput"], "standalone");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("node /app/standalone/server.js")
        );
        assert_eq!(
            analysis.plan.deploy.inputs[0].filter.include,
            vec!["/app/standalone".to_string()]
        );
        // The standalone server binds HOSTNAME, which defaults to localhost.
        assert_eq!(analysis.plan.deploy.variables["HOSTNAME"], "0.0.0.0");
    }

    #[test]
    fn a_next_app_without_standalone_ships_the_app_directory() {
        let (_dir, app) = write_app(&[(
            "package.json",
            r#"{"dependencies":{"next":"15"},"scripts":{"build":"next build","start":"next start"}}"#,
        )]);
        let analysis = plan_for(&app);

        assert!(!analysis.metadata.contains_key("nextOutput"));
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("next start")
        );
    }

    #[test]
    fn nvmrc_pins_the_node_version() {
        let (_dir, app) = write_app(&[
            ("package.json", r#"{"engines":{"node":">=18"}}"#),
            (".nvmrc", "v22.3.0"),
            ("index.js", ""),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["nodeVersion"], "22.3.0");
        let (_, request) = analysis
            .packages
            .iter()
            .find(|(name, _)| name == "node")
            .unwrap();
        assert_eq!(request.source, ".nvmrc");
    }

    #[test]
    fn engines_are_used_when_no_version_file_exists() {
        let (_dir, app) = write_app(&[
            ("package.json", r#"{"engines":{"node":">=20"}}"#),
            ("index.js", ""),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["nodeVersion"], "20");
    }

    #[test]
    fn workspaces_install_from_the_full_context() {
        let (_dir, app) = write_app(&[
            ("package.json", r#"{"workspaces":["packages/*"]}"#),
            ("pnpm-lock.yaml", ""),
            ("packages/api/package.json", "{}"),
            ("index.js", ""),
        ]);
        let analysis = plan_for(&app);
        let install = analysis.plan.step("install").unwrap();
        let local = install.inputs.iter().find(|i| i.local).unwrap();
        assert!(local.filter.is_unfiltered());
    }

    #[test]
    fn pnpm_installs_libatomic_for_the_standalone_binary() {
        // pnpm 11+ is `@pnpm/exe`, which links against libatomic.so.1.
        // debian-slim does not ship it, so `pnpm ci` exits 127 without this.
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"packageManager":"pnpm@11.19.0","scripts":{"start":"node index.js"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
            ("index.js", ""),
        ]);
        let analysis = plan_for(&app);
        let packages = analysis.plan.step("packages").unwrap();
        assert!(
            packages.commands[0].display_name().contains("libatomic1"),
            "{}",
            packages.commands[0].display_name()
        );
    }

    #[test]
    fn pnpm_11_ships_libatomic_in_the_runtime_image() {
        // Non-simple start scripts fall back to `pnpm run start`, and the
        // runtime stage copies `/mise` — so libatomic must be present at boot.
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"packageManager":"pnpm@11.19.0","scripts":{"start":"next start -p $PORT"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
            ("index.js", ""),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("pnpm run start")
        );
        let runtime = analysis.plan.step("runtime").unwrap();
        assert!(
            runtime.commands[0].display_name().contains("libatomic1"),
            "{}",
            runtime.commands[0].display_name()
        );
    }

    #[test]
    fn a_pin_naming_another_manager_does_not_select_pnpm_ci() {
        // `detect` lets the lockfile win, so this is a pnpm app. Honouring
        // npm's version number as pnpm's installs pnpm 7, where `ci` is not a
        // command — pnpm aliases the app's own `ci` script instead, and the
        // install step exits 0 having installed nothing from the lockfile.
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"packageManager":"npm@7.33.5","scripts":{"ci":"echo pwned","start":"node index.js"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
            ("index.js", ""),
        ]);
        let analysis = plan_for(&app);
        let install = analysis.plan.step("install").unwrap();
        assert_eq!(
            install.commands[0].display_name(),
            "pnpm install --frozen-lockfile"
        );
        let packages = analysis.plan.step("packages").unwrap();
        assert!(
            packages
                .assets
                .values()
                .any(|a| a.contains("pnpm = \"latest\"")),
            "{:?}",
            packages.assets
        );
    }

    #[test]
    fn a_static_site_does_not_ship_libatomic() {
        // A Vite SPA serves with Caddy and drops /mise, so there is no pnpm
        // binary in the runtime image to load libatomic for. The build stage
        // still needs it, because that is where pnpm actually runs.
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"packageManager":"pnpm@11.19.0","devDependencies":{"vite":"^5"},"scripts":{"build":"vite build"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
            ("index.html", ""),
        ]);
        let analysis = plan_for(&app);
        assert!(analysis.plan.step("packages").unwrap().commands[0]
            .display_name()
            .contains("libatomic1"));
        assert!(
            !analysis.plan.step("runtime").unwrap().commands[0]
                .display_name()
                .contains("libatomic1"),
            "{}",
            analysis.plan.step("runtime").unwrap().commands[0].display_name()
        );
    }

    #[test]
    fn older_pnpm_does_not_install_libatomic() {
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"packageManager":"pnpm@9.15.0","scripts":{"start":"node index.js"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
            ("index.js", ""),
        ]);
        let analysis = plan_for(&app);
        let packages = analysis.plan.step("packages").unwrap();
        assert!(
            !packages.commands[0].display_name().contains("libatomic1"),
            "{}",
            packages.commands[0].display_name()
        );
        let runtime = analysis.plan.step("runtime").unwrap();
        assert!(
            !runtime.commands[0].display_name().contains("libatomic1"),
            "{}",
            runtime.commands[0].display_name()
        );
    }

    #[test]
    fn pnpm_sets_ci_on_install_and_build() {
        // `pnpm run build` may re-invoke install for a deps check. Without a TTY
        // that aborts unless CI=true (ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY).
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"packageManager":"pnpm@11.19.0","scripts":{"build":"tsc","start":"node index.js"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
            ("index.js", ""),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(
            analysis.plan.step("install").unwrap().variables.get("CI"),
            Some(&"true".to_string())
        );
        assert_eq!(
            analysis.plan.step("build").unwrap().variables.get("CI"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn playwright_discovers_its_libraries_and_gets_an_in_app_browser_cache() {
        // The browser is downloaded during install. Left at its default
        // location it lands under $HOME, which the deploy layer never carries
        // — and the build runs as root while the runtime user is `autopack`,
        // so the two do not even agree on where $HOME is.
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"dependencies":{"playwright":"^1.62.0"},"scripts":{"start":"node server.js"}}"#,
            ),
            ("package-lock.json", "{}"),
            ("server.js", ""),
        ]);
        let analysis = plan_for(&app);

        // No hardcoded library closure: the runtime image installs whatever
        // static ELF inspection found the browser to link, plus fonts, which are opened
        // through fontconfig and so are invisible to the linker.
        let runtime = analysis.plan.step("runtime").unwrap();
        let apt = runtime.commands[0].display_name();
        assert!(apt.contains("fonts-liberation"), "{apt}");
        assert!(!apt.contains("libnss3"), "{apt}");
        assert!(
            runtime
                .commands
                .iter()
                .any(|c| c.display_name().contains(RUNTIME_DEPS_FILE)),
            "runtime does not install the recorded libraries"
        );
        assert!(
            analysis
                .plan
                .step("install")
                .unwrap()
                .commands
                .iter()
                .any(|c| {
                    c.display_name().contains("readelf ")
                        && c.display_name().contains(PLAYWRIGHT_BROWSERS)
                }),
            "install step does not inspect the browser"
        );

        for step in ["install", "build"] {
            assert_eq!(
                analysis
                    .plan
                    .step(step)
                    .unwrap()
                    .variables
                    .get("PLAYWRIGHT_BROWSERS_PATH"),
                Some(&"0".to_string()),
                "{step}"
            );
        }

        // Playwright does not fetch a browser from a postinstall hook the way
        // Puppeteer does; without this the app starts and then tells the user
        // to run `npx playwright install`.
        let install = analysis.plan.step("install").unwrap();
        assert!(
            install
                .commands
                .iter()
                .any(|c| c.display_name().contains("playwright install chromium")),
            "{:?}",
            install
                .commands
                .iter()
                .map(|c| c.display_name())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            analysis
                .plan
                .deploy
                .variables
                .get("PLAYWRIGHT_BROWSERS_PATH"),
            Some(&"0".to_string())
        );
    }

    #[test]
    fn puppeteer_cache_is_redirected_under_the_app_directory() {
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"dependencies":{"puppeteer":"^24.0.0"},"scripts":{"start":"node server.js"}}"#,
            ),
            ("package-lock.json", "{}"),
            ("server.js", ""),
        ]);
        let analysis = plan_for(&app);
        let expected = format!("{APP_DIR}/.cache/puppeteer");
        for step in ["install", "build"] {
            assert_eq!(
                analysis
                    .plan
                    .step(step)
                    .unwrap()
                    .variables
                    .get("PUPPETEER_CACHE_DIR"),
                Some(&expected),
                "{step}"
            );
        }
        assert_eq!(
            analysis.plan.deploy.variables.get("PUPPETEER_CACHE_DIR"),
            Some(&expected)
        );
        // Puppeteer's own postinstall fetches the browser, so adding a
        // download command would download it a second time.
        assert!(!analysis
            .plan
            .step("install")
            .unwrap()
            .commands
            .iter()
            .any(|c| c.display_name().contains("playwright install")));
    }

    #[test]
    fn a_text_mention_does_not_fetch_a_browser() {
        // Detection used to read the whole manifest, so a blog whose
        // description mentioned Playwright had `playwright install` run as
        // root in its build — fetching an unpinned package it never declared.
        for manifest in [
            r#"{"description":"A blog. We test it with playwright.","dependencies":{"express":"^4"},"scripts":{"start":"node s.js"}}"#,
            r#"{"keywords":["scraping","playwright","puppeteer"],"dependencies":{"express":"^4"},"scripts":{"start":"node s.js"}}"#,
            r#"{"dependencies":{"express":"^4"},"scripts":{"start":"node s.js","test":"playwright test"}}"#,
        ] {
            let (_dir, app) = write_app(&[
                ("package.json", manifest),
                ("package-lock.json", "{}"),
                ("s.js", ""),
            ]);
            let analysis = plan_for(&app);
            let install = analysis.plan.step("install").unwrap();
            assert!(
                !install
                    .commands
                    .iter()
                    .any(|c| c.display_name().contains("playwright install")),
                "{manifest}"
            );
            assert!(
                !analysis.plan.step("runtime").unwrap().commands[0]
                    .display_name()
                    .contains("fonts-liberation"),
                "{manifest}"
            );
        }
    }

    #[test]
    fn a_dev_dependency_browser_is_not_shipped() {
        // `@playwright/test` is a devDependency of a great many frontend
        // repos. This one is a Vite SPA: the runtime image is Caddy plus a
        // dist directory, with no Node and nowhere to launch a browser.
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"devDependencies":{"vite":"^5","@playwright/test":"^1.62"},"scripts":{"build":"vite build"}}"#,
            ),
            ("package-lock.json", "{}"),
            ("index.html", ""),
        ]);
        let analysis = plan_for(&app);
        let apt = analysis.plan.step("runtime").unwrap().commands[0].display_name();
        assert!(!apt.contains("fonts-liberation"), "{apt}");
        assert!(!analysis
            .plan
            .step("install")
            .unwrap()
            .commands
            .iter()
            .any(|c| c.display_name().contains("playwright install")));
    }

    #[test]
    fn a_missing_browser_fails_the_build_rather_than_the_container() {
        // A search that finds nothing is silent: no libraries are recorded,
        // the runtime stage skips its install, and the image dies the first
        // time it opens a browser. pnpm hits exactly this — it does not hoist
        // playwright-core, so a glob written for npm's layout finds nothing.
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"dependencies":{"puppeteer":"^24.0.0"},"scripts":{"start":"node s.js"}}"#,
            ),
            ("package-lock.json", "{}"),
            ("s.js", ""),
        ]);
        let analysis = plan_for(&app);
        let install = analysis.plan.step("install").unwrap();
        let names: Vec<_> = install.commands.iter().map(|c| c.display_name()).collect();

        let guard = names
            .iter()
            .position(|n| n.contains("downloaded no browser"))
            .expect("no guard against a missing browser");
        assert!(names[guard].contains("exit 1"));
        assert!(names[guard].contains("SKIP_DOWNLOAD"));

        // The guard has to run before the inspection it protects.
        let record = names.iter().position(|n| n.contains("readelf ")).unwrap();
        assert!(guard < record, "{names:?}");
    }

    #[test]
    fn browser_discovery_does_not_assume_a_hoisted_layout() {
        // pnpm puts the browsers under
        // node_modules/.pnpm/playwright-core@1.62.1/node_modules/... — a
        // positional glob written for npm finds nothing there, and finding
        // nothing produced an image with no browser libraries and a build
        // that exited 0.
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"dependencies":{"playwright":"^1.62.1"},"scripts":{"start":"node s.js"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
            ("s.js", ""),
        ]);
        let analysis = plan_for(&app);
        let record = analysis
            .plan
            .step("install")
            .unwrap()
            .commands
            .iter()
            .map(|c| c.display_name())
            .find(|n| n.contains("readelf "))
            .expect("no discovery command");
        assert!(record.contains("find "), "{record}");
        // Both binaries: Playwright launches the headless shell by default,
        // Puppeteer the full browser, and they do not link the same set.
        assert!(record.contains("-name chrome-headless-shell"), "{record}");
        assert!(record.contains("-name chrome "), "{record}");
    }

    #[test]
    fn an_app_without_a_browser_gets_no_browser_environment() {
        let (_dir, app) = write_app(&[
            (
                "package.json",
                r#"{"dependencies":{"express":"^4"},"scripts":{"start":"node server.js"}}"#,
            ),
            ("package-lock.json", "{}"),
            ("server.js", ""),
        ]);
        let analysis = plan_for(&app);
        let install = analysis.plan.step("install").unwrap();
        assert!(install.variables.get("PLAYWRIGHT_BROWSERS_PATH").is_none());
        assert!(install.variables.get("PUPPETEER_CACHE_DIR").is_none());
        let apt = analysis.plan.step("runtime").unwrap().commands[0].display_name();
        assert!(!apt.contains("fonts-liberation"), "{apt}");
    }

    #[test]
    fn static_dir_config_forces_static_serving() {
        let (_dir, app) = write_app(&[(
            "package.json",
            r#"{"scripts":{"build":"remix build","start":"remix-serve build"}}"#,
        )]);
        let analysis = plan_with_env(&app, &[("AUTOPACK_STATIC_DIR", "public")]).unwrap();
        assert_eq!(analysis.metadata["serve"], "caddy static (public)");
    }

    #[test]
    fn procfile_wins_over_the_start_script() {
        let (_dir, app) = write_app(&[
            ("package.json", r#"{"scripts":{"start":"node index.js"}}"#),
            ("Procfile", "web: node worker.js"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("node worker.js")
        );
    }
}
