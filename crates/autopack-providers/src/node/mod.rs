//! Node.js provider: npm, pnpm, yarn and bun apps, including SPA builds.

mod package_json;
mod package_manager;

pub use package_json::PackageJson;
pub use package_manager::PackageManager;

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::{
    caddy_layer, caddy_start_command, caddyfile, install_recorded_runtime_libraries,
    normalize_version_range, procfile_web_command, read_version_file, record_runtime_libraries,
    CADDYFILE_PATH, RUNTIME_DEPS_FILE,
};

/// Where a Next.js standalone bundle is staged for the runtime image.
const STANDALONE_DIR: &str = "/app/standalone";

/// Where Playwright keeps browsers when `PLAYWRIGHT_BROWSERS_PATH=0` asks for
/// an install beside the package.
const PLAYWRIGHT_PACKAGE: &str = "playwright-core";
const PLAYWRIGHT_BROWSERS: &str = ".local-browsers";

/// Fonts a browser needs but never links.
///
/// Everything else Chromium requires is a `DT_NEEDED` entry, so `ldd` finds it
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
        if !browsers.browser_binaries.is_empty() {
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
        self.plan_build(ctx, &package, manager, &browsers)?;

        let static_site = static_site(ctx, &package, framework);
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
        if !browsers.browser_binaries.is_empty() {
            step.add_command(Command::shell(record_runtime_libraries(
                &browsers.browser_binaries.join(" "),
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
        browsers: &BrowserTooling,
    ) -> Result<()> {
        let build_script = package
            .script("build")
            .map(|_| manager.run_command("build"));

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
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
        ctx.add_deploy_input(caddy_layer());
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
) -> Option<StaticSite> {
    if let Some(directory) = ctx.env.config("STATIC_DIR") {
        return Some(StaticSite {
            directory: directory.to_string(),
            spa: !ctx.env.is_enabled("AUTOPACK_STATIC_MPA"),
        });
    }

    // A start script means the author intends to run a server.
    let has_server_script = package.script("start").is_some();

    match framework {
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
        _ => None,
    }
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
    /// Debian packages the runtime image needs that `ldd` cannot discover.
    ///
    /// Fonts only. A browser does not link them — it opens them through
    /// fontconfig at run time — so nothing about the binary reveals that they
    /// are needed, and without them Chromium renders text as empty boxes
    /// instead of failing in a way anyone would notice.
    runtime_packages: Vec<String>,
    /// Paths to `ldd` for the libraries the browser actually links.
    browser_binaries: Vec<String>,
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
        browser_binaries: Vec::new(),
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
    if playwright {
        // Playwright installs beside the package; both the full browser and
        // the headless shell ship, and they do not link the same set.
        tooling.browser_binaries.push(format!(
            "{APP_DIR}/node_modules/{PLAYWRIGHT_PACKAGE}/{PLAYWRIGHT_BROWSERS}/*/chrome-linux*/chrome*"
        ));
    }
    if puppeteer {
        tooling.browser_binaries.push(format!(
            "{APP_DIR}/.cache/puppeteer/*/*/chrome-linux*/chrome*"
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
    use super::{PLAYWRIGHT_BROWSERS, RUNTIME_DEPS_FILE};
    use crate::test_support::{plan_for, plan_with_env, write_app};
    use autopack_core::APP_DIR;

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
            .any(|input| input.image.as_deref() == Some(crate::support::CADDY_IMAGE)));
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
        // `ldd` found the browser to link, plus fonts, which are opened
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
                .any(|c| c.display_name().contains("ldd ")
                    && c.display_name().contains(".cache/puppeteer")
                    || c.display_name().contains(PLAYWRIGHT_BROWSERS)),
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
