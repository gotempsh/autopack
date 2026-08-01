//! Static site provider: a directory of files served by Caddy.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::{caddy_layer, caddy_start_command, caddyfile, CADDYFILE_PATH};

/// Directories checked for an `index.html`, in order of preference.
const ROOTS: &[&str] = &["", "public", "dist", "build", "_site", "site", "out"];

/// Serves a directory of static files. Registered last, so it only ever sees
/// apps no language provider claimed.
pub struct StaticProvider;

impl Provider for StaticProvider {
    fn id(&self) -> &'static str {
        "static"
    }

    fn display_name(&self) -> &'static str {
        "Static site"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(document_root(app).is_some())
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let root = ctx
            .env
            .config("STATIC_DIR")
            .map(str::to_string)
            .or_else(|| document_root(ctx.app))
            .unwrap_or_default();

        let served = if root.is_empty() {
            APP_DIR.to_string()
        } else {
            format!("{APP_DIR}/{}", root.trim_matches('/'))
        };
        ctx.add_metadata("documentRoot", &served);

        // Static sites are usually multi-page: falling back to index.html for
        // every unknown path would turn real 404s into a soft 200.
        let spa = ctx.env.is_enabled("AUTOPACK_SPA");
        ctx.add_metadata("routing", if spa { "spa" } else { "multi-page" });

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::local()];
        let asset = step.add_asset("Caddyfile", caddyfile(&served, spa));
        step.add_command(Command::file(CADDYFILE_PATH, asset));

        ctx.set_runtime_includes_runtimes(false);
        ctx.add_deploy_input(Layer::step(steps::BUILD).including([APP_DIR]));
        ctx.add_deploy_input(caddy_layer());
        ctx.set_start_command(caddy_start_command());
        Ok(())
    }
}

/// The first directory containing an `index.html`, relative to the app root.
fn document_root(app: &App) -> Option<String> {
    ROOTS.iter().find_map(|root| {
        let candidate = if root.is_empty() {
            "index.html".to_string()
        } else {
            format!("{root}/index.html")
        };
        app.has_file(&candidate).then(|| root.to_string())
    })
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, write_app};

    #[test]
    fn serves_a_root_index_html() {
        let (_dir, app) = write_app(&[("index.html", "<h1>hi</h1>")]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "static");
        assert_eq!(analysis.metadata["documentRoot"], "/app");
        assert_eq!(analysis.metadata["routing"], "multi-page");
        assert!(analysis
            .plan
            .deploy
            .start_command
            .as_deref()
            .unwrap()
            .starts_with("caddy run"));
        // No language runtime is installed at all.
        assert!(analysis.plan.step("packages").is_none());
        assert!(analysis.packages.is_empty());
    }

    #[test]
    fn prefers_a_public_directory() {
        let (_dir, app) = write_app(&[("public/index.html", ""), ("README.md", "")]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["documentRoot"], "/app/public");
    }

    #[test]
    fn node_provider_wins_over_static() {
        let (_dir, app) = write_app(&[
            ("index.html", ""),
            ("package.json", r#"{"scripts":{"start":"node server.js"}}"#),
            ("server.js", ""),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.provider, "node");
    }
}
