//! Procfile provider: a repository that says how to start itself and nothing else.

use autopack_core::plan::Layer;
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

use crate::support::procfile_web_command;

/// Runs the `web:` process from a Procfile with no build step.
///
/// Every language provider already reads a Procfile for its start command.
/// This provider is what handles the remaining case: a repository with a
/// Procfile and no recognisable manifest — a prebuilt binary, a shell script,
/// or a directory of assets driven by a checked-in tool.
pub struct ProcfileProvider;

impl Provider for ProcfileProvider {
    fn id(&self) -> &'static str {
        "procfile"
    }

    fn display_name(&self) -> &'static str {
        "Procfile"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(procfile_web_command(app)?.is_some())
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let command = procfile_web_command(ctx.app)?.ok_or_else(|| {
            autopack_core::Error::provider("procfile", "the Procfile has no `web:` process")
        })?;
        ctx.add_metadata("webProcess", &command);

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([APP_DIR]));
        ctx.set_start_command(command);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, write_app};

    #[test]
    fn runs_the_web_process_from_a_bare_repository() {
        let (_dir, app) = write_app(&[
            ("Procfile", "web: ./bin/server --port $PORT\n"),
            ("bin/server", "#!/bin/sh"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "procfile");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("./bin/server --port $PORT")
        );
        // Nothing is installed or compiled.
        assert!(analysis.plan.step("install").is_none());
        assert!(analysis.packages.is_empty());
    }

    #[test]
    fn language_providers_still_win() {
        let (_dir, app) = write_app(&[
            ("Procfile", "web: node index.js"),
            ("package.json", "{}"),
            ("index.js", ""),
        ]);
        assert_eq!(plan_for(&app).provider, "node");
    }
}
