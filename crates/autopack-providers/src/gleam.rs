//! Gleam provider.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result};

use crate::support::procfile_web_command;

/// Gleam release used when the project does not pin one.
const DEFAULT_GLEAM_VERSION: &str = "1.18.0";

/// Where `gleam export erlang-shipment` writes its output.
const SHIPMENT_DIR: &str = "/app/build/erlang-shipment";

/// Builds Gleam applications targeting Erlang.
pub struct GleamProvider;

impl Provider for GleamProvider {
    fn id(&self) -> &'static str {
        "gleam"
    }

    fn display_name(&self) -> &'static str {
        "Gleam"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("gleam.toml"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let version = ctx
            .env
            .config("GLEAM_VERSION")
            .unwrap_or(DEFAULT_GLEAM_VERSION)
            .trim_start_matches('v')
            .to_string();

        // The official image pairs a Gleam release with the Erlang/OTP it was
        // tested against. A shipment needs a compatible OTP at run time, so
        // build and runtime deliberately use the same image.
        let image = format!("ghcr.io/gleam-lang/gleam:v{version}-erlang-slim");
        ctx.set_base_image(&image);
        ctx.set_runtime_base_image(&image);
        ctx.set_runtime_includes_runtimes(false);
        ctx.add_metadata("gleamVersion", &version);
        ctx.add_metadata("image", &image);

        let cache = ctx.shared_cache("gleam", "/cache/gleam");

        let manifests: Vec<&str> = ["gleam.toml", "manifest.toml"]
            .into_iter()
            .filter(|file| ctx.app.has_file(file))
            .collect();

        let install = ctx.step(steps::INSTALL);
        install.add_input(Layer::local().including(manifests));
        install.add_variable("HEX_HOME", "/cache/gleam");
        install.add_cache(cache);
        install.add_command(Command::shell("gleam deps download"));

        let build = ctx.step(steps::BUILD);
        build.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
        build.add_variable("HEX_HOME", "/cache/gleam");
        build.add_command(Command::shell("gleam export erlang-shipment"));

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([SHIPMENT_DIR]));

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None => format!("{SHIPMENT_DIR}/entrypoint.sh run"),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, plan_with_env, write_app};

    #[test]
    fn exports_an_erlang_shipment() {
        let (_dir, app) = write_app(&[
            ("gleam.toml", "name = \"app\"\nversion = \"1.0.0\"\n"),
            ("manifest.toml", ""),
            ("src/app.gleam", "pub fn main() { Nil }"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "gleam");
        assert_eq!(
            analysis.plan.step("build").unwrap().commands[0].display_name(),
            "gleam export erlang-shipment"
        );
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("/app/build/erlang-shipment/entrypoint.sh run")
        );
    }

    #[test]
    fn the_gleam_version_is_configurable() {
        let (_dir, app) = write_app(&[("gleam.toml", "name = \"app\"")]);
        let analysis = plan_with_env(&app, &[("AUTOPACK_GLEAM_VERSION", "v1.11.1")]).unwrap();
        assert_eq!(
            analysis.metadata["image"],
            "ghcr.io/gleam-lang/gleam:v1.11.1-erlang-slim"
        );
    }
}
