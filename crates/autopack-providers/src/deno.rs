//! Deno provider.

use indexmap::IndexMap;
use serde::Deserialize;

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::{foreign_manifest, procfile_web_command};

/// Deno version installed when the project does not pin one.
const DEFAULT_DENO_VERSION: &str = "2";

/// Manifests that belong to Deno itself.
///
/// `package.json` is deliberately absent: Deno reads one for npm interop, but
/// its presence alongside a bare `main.ts` means the repository is a Node
/// project, not a Deno one.
const OWN_MANIFESTS: &[&str] = &["deno.json", "deno.jsonc"];

/// Entry points tried when no `start` task exists.
const ENTRY_POINTS: &[&str] = &[
    "main.ts",
    "mod.ts",
    "server.ts",
    "src/main.ts",
    "src/mod.ts",
    "main.js",
];

/// Builds Deno applications.
pub struct DenoProvider;

/// The parts of `deno.json` autopack reads.
#[derive(Debug, Default, Deserialize)]
struct DenoConfig {
    #[serde(default)]
    tasks: IndexMap<String, String>,
}

impl Provider for DenoProvider {
    fn id(&self) -> &'static str {
        "deno"
    }

    fn display_name(&self) -> &'static str {
        "Deno"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        if app.has_any_file(["deno.json", "deno.jsonc", "deno.lock"]) {
            return Ok(true);
        }

        // A bare `main.ts` is only a Deno signal when nothing else claims the
        // repository — plenty of Node projects have one too.
        Ok(foreign_manifest(app, OWN_MANIFESTS).is_none()
            && app.has_any_file(["main.ts", "mod.ts"]))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let config = deno_config(ctx.app)?;

        let version = ctx
            .env
            .config("DENO_VERSION")
            .unwrap_or(DEFAULT_DENO_VERSION)
            .to_string();
        ctx.packages.add("deno", &version, "autopack default");
        ctx.add_metadata("denoVersion", &version);

        let cache = ctx.shared_cache("deno", "/cache/deno");

        let manifests: Vec<&str> = ["deno.json", "deno.jsonc", "deno.lock", "package.json"]
            .into_iter()
            .filter(|file| ctx.app.has_file(file))
            .collect();

        if !manifests.is_empty() {
            let install = ctx.step(steps::INSTALL);
            install.add_input(Layer::local().including(manifests));
            install.add_variable("DENO_DIR", "/cache/deno");
            install.add_cache(cache.clone());
            install.add_command(Command::shell("deno install"));
        }

        let base = if ctx.has_step(steps::INSTALL) {
            Layer::step(steps::INSTALL)
        } else {
            Layer::step(steps::PACKAGES)
        };

        let build_task = config.tasks.contains_key("build");
        let build = ctx.step(steps::BUILD);
        build.inputs = vec![base, Layer::local()];
        build.add_variable("DENO_DIR", "/cache/deno");
        build.add_cache(cache);
        if build_task {
            build.add_command(Command::shell("deno task build"));
        }

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([APP_DIR]));
        ctx.add_deploy_variable("DENO_DIR", "/deno-dir");

        if let Some(command) = start_command(ctx.app, &config)? {
            ctx.set_start_command(command);
        }
        Ok(())
    }
}

fn deno_config(app: &App) -> Result<DenoConfig> {
    for file in ["deno.json", "deno.jsonc"] {
        if let Some(config) = app.read_json_opt::<DenoConfig>(file)? {
            return Ok(config);
        }
    }
    Ok(DenoConfig::default())
}

fn start_command(app: &App, config: &DenoConfig) -> Result<Option<String>> {
    if let Some(command) = procfile_web_command(app)? {
        return Ok(Some(command));
    }

    if config.tasks.contains_key("start") {
        return Ok(Some("deno task start".to_string()));
    }

    Ok(ENTRY_POINTS
        .iter()
        .find(|entry| app.has_file(entry))
        // Deno is deny-by-default; a server needs at least net and env, and
        // guessing a narrower set produces a runtime permission prompt that
        // nobody is there to answer.
        .map(|entry| format!("deno run --allow-all {entry}")))
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, write_app};

    #[test]
    fn deno_json_tasks_drive_build_and_start() {
        let (_dir, app) = write_app(&[
            (
                "deno.json",
                r#"{"tasks":{"build":"deno bundle","start":"deno run -A main.ts"}}"#,
            ),
            ("main.ts", "Deno.serve(() => new Response('hi'));"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "deno");
        assert_eq!(
            analysis.plan.step("build").unwrap().commands[0].display_name(),
            "deno task build"
        );
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("deno task start")
        );
    }

    #[test]
    fn falls_back_to_the_entry_point() {
        let (_dir, app) = write_app(&[
            ("deno.json", "{}"),
            ("main.ts", "Deno.serve(() => new Response('hi'));"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("deno run --allow-all main.ts")
        );
    }

    #[test]
    fn a_node_project_with_a_main_ts_is_not_deno() {
        let (_dir, app) = write_app(&[
            ("package.json", r#"{"scripts":{"start":"node index.js"}}"#),
            ("index.js", ""),
            ("main.ts", ""),
        ]);
        assert_eq!(plan_for(&app).provider, "node");
    }

    #[test]
    fn deno_json_beats_package_json() {
        let (_dir, app) = write_app(&[
            ("deno.json", r#"{"tasks":{"start":"deno run -A main.ts"}}"#),
            ("package.json", "{}"),
            ("main.ts", ""),
        ]);
        assert_eq!(plan_for(&app).provider, "deno");
    }
}
