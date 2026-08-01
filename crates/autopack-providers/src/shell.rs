//! Shell provider: an escape hatch driven entirely by configuration.

use autopack_core::plan::Layer;
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result, APP_DIR};

/// Builds whatever the user's configuration says to build.
///
/// This provider never detects — selecting it is always deliberate, with
/// `AUTOPACK_PROVIDER=shell` or `{"provider": "shell"}`. It exists so an app
/// autopack does not understand is still buildable without abandoning the tool
/// and hand-writing a Dockerfile:
///
/// ```json
/// {
///   "provider": "shell",
///   "packages": { "node": "22", "python": "3.12" },
///   "aptPackages": ["imagemagick"],
///   "steps": { "build": { "commands": ["./scripts/build.sh"] } },
///   "deploy": { "startCommand": "./scripts/run.sh" }
/// }
/// ```
pub struct ShellProvider;

impl Provider for ShellProvider {
    fn id(&self) -> &'static str {
        "shell"
    }

    fn display_name(&self) -> &'static str {
        "Shell"
    }

    fn detect(&self, _app: &App, _env: &Environment) -> Result<bool> {
        // Detecting would mean claiming every repository, since this provider
        // makes no assumptions at all.
        Ok(false)
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        ctx.add_metadata(
            "configured",
            "commands come from autopack.json / AUTOPACK_*",
        );

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([APP_DIR]));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_with_env, write_app};

    #[test]
    fn never_detects_on_its_own() {
        let (_dir, app) = write_app(&[("anything.txt", "")]);
        let err = plan_with_env(&app, &[]).unwrap_err();
        assert!(err.to_string().contains("no provider"), "{err}");
    }

    #[test]
    fn runs_configured_commands_when_selected() {
        let (_dir, app) = write_app(&[("scripts/build.sh", "#!/bin/sh")]);
        let analysis = plan_with_env(
            &app,
            &[
                ("AUTOPACK_PROVIDER", "shell"),
                ("AUTOPACK_PACKAGES", "node@22"),
                ("AUTOPACK_BUILD_CMD", "./scripts/build.sh"),
                ("AUTOPACK_START_CMD", "./scripts/run.sh"),
            ],
        )
        .unwrap();

        assert_eq!(analysis.provider, "shell");
        assert_eq!(
            analysis.plan.step("build").unwrap().commands[0].display_name(),
            "./scripts/build.sh"
        );
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("./scripts/run.sh")
        );
        assert!(analysis.packages.iter().any(|(name, _)| name == "node"));
    }
}
