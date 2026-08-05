//! Node.js provider: npm, pnpm, yarn and bun apps, including SPA builds.

mod package_json;
mod package_manager;

pub use package_json::PackageJson;
pub use package_manager::PackageManager;

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::{
    caddy_layer, caddy_start_command, caddyfile, normalize_version_range, procfile_web_command,
    read_version_file, CADDYFILE_PATH,
};

/// Where a Next.js standalone bundle is staged for the runtime image.
const STANDALONE_DIR: &str = "/app/standalone";

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
            ctx.deploy_apt_packages.push("libatomic1".to_string());
        }

        self.plan_install(ctx, &package, manager)?;
        self.plan_build(ctx, &package, manager)?;

        let static_site = static_site(ctx, &package, framework);
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
        step.add_command(Command::shell(install_command));
        Ok(())
    }

    /// Application build. The step always exists — it is what carries source
    /// into the runtime image — but only runs a command when there is one.
    fn plan_build(
        &self,
        ctx: &mut BuildContext<'_>,
        package: &PackageJson,
        manager: PackageManager,
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
    use crate::test_support::{plan_for, plan_with_env, write_app};

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
