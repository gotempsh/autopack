//! Elixir provider, including Phoenix.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::procfile_web_command;

/// Elixir version used when `mix.exs` does not constrain one.
const DEFAULT_ELIXIR_VERSION: &str = "1.18";

/// Where `mix release` writes the self-contained release.
const RELEASE_DIR: &str = "/app/release";

/// Builds Elixir applications as OTP releases.
pub struct ElixirProvider;

impl Provider for ElixirProvider {
    fn id(&self) -> &'static str {
        "elixir"
    }

    fn display_name(&self) -> &'static str {
        "Elixir"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("mix.exs"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let mix = ctx.app.read_file("mix.exs")?;
        let app_name = otp_app_name(&mix).ok_or_else(|| {
            Error::provider(
                "elixir",
                "could not find `app: :name` in mix.exs, so the release binary is unknown.\n\
                 Set `AUTOPACK_START_CMD=/app/release/bin/<name> start`",
            )
        })?;

        let (version, source) = elixir_version(ctx.app, &mix)?;
        // Building Erlang under mise means compiling OTP from source. The
        // official image has both Elixir and Erlang prebuilt.
        ctx.set_base_image(format!("elixir:{version}"));
        // A release bundles ERTS, so the runtime image needs no Erlang — only
        // the shared libraries ERTS links against.
        ctx.set_runtime_base_image("debian:bookworm-slim");
        ctx.set_runtime_includes_runtimes(false);
        ctx.deploy_apt_packages.extend(
            ["libssl3", "libncurses6", "libstdc++6", "locales"]
                .into_iter()
                .map(String::from),
        );

        ctx.add_metadata("elixirVersion", &version);
        ctx.add_metadata("elixirVersionSource", source);
        ctx.add_metadata("otpApp", &app_name);

        let is_phoenix = mix.contains(":phoenix");
        let has_assets = ctx.app.has_dir("assets");
        if is_phoenix {
            ctx.add_metadata("framework", "phoenix");
        }

        self.plan_install(ctx)?;
        self.plan_build(ctx, is_phoenix && has_assets)?;

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([RELEASE_DIR]));
        ctx.add_deploy_variable("MIX_ENV", "prod");
        ctx.add_deploy_variable("LANG", "C.UTF-8");
        // Releases refuse to boot without a cookie; a stable one avoids a
        // different value on every restart breaking clustering.
        ctx.add_deploy_variable("RELEASE_DISTRIBUTION", "none");

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None => format!("{RELEASE_DIR}/bin/{app_name} start"),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

impl ElixirProvider {
    fn plan_install(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        // Cache only the download cache, never MIX_HOME/HEX_HOME themselves.
        // `mix local.hex` installs an archive into MIX_HOME; if MIX_HOME is a
        // cache mount, that archive is not in the layer and the next step
        // fails with the memorably unhelpful "Could not find an SCM for
        // dependency".
        let deps_cache = ctx.shared_cache("hex", "/root/.hex/cache");

        let mut manifests: Vec<String> = ["mix.exs", "mix.lock"]
            .into_iter()
            .filter(|file| ctx.app.has_file(file))
            .map(String::from)
            .collect();
        // Compile-time configuration is read while dependencies build.
        if ctx.app.has_dir("config") {
            manifests.push("config".to_string());
        }

        let step = ctx.step(steps::INSTALL);
        step.add_input(Layer::local().including(manifests));
        step.add_variable("MIX_ENV", "prod");
        step.add_cache(deps_cache);
        step.add_command(Command::shell(
            "mix local.hex --force && mix local.rebar --force",
        ));
        step.add_command(Command::shell("mix deps.get --only prod"));
        step.add_command(Command::shell("mix deps.compile"));
        Ok(())
    }

    fn plan_build(&self, ctx: &mut BuildContext<'_>, deploy_assets: bool) -> Result<()> {
        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
        step.add_variable("MIX_ENV", "prod");
        step.add_command(Command::shell("mix compile"));
        if deploy_assets {
            step.add_command(Command::shell("mix assets.deploy"));
        }
        step.add_command(Command::shell(format!(
            "mix release --overwrite --path {RELEASE_DIR}"
        )));
        Ok(())
    }
}

/// The OTP application name from `app: :name` in `mix.exs`.
fn otp_app_name(mix: &str) -> Option<String> {
    let start = mix.find("app:")? + "app:".len();
    let rest = mix[start..].trim_start();
    let rest = rest.strip_prefix(':')?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The Elixir version, narrowed to `major.minor` for the image tag.
fn elixir_version(app: &App, mix: &str) -> Result<(String, String)> {
    if let Some(tool_versions) = app.read_file_opt(".tool-versions")? {
        for line in tool_versions.lines() {
            if let Some(rest) = line.trim().strip_prefix("elixir ") {
                // `.tool-versions` entries look like `1.18.1-otp-27`.
                if let Some(version) = major_minor(rest.split('-').next().unwrap_or(rest)) {
                    return Ok((version, ".tool-versions".into()));
                }
            }
        }
    }

    if let Some(start) = mix.find("elixir:") {
        let rest = &mix[start + "elixir:".len()..];
        if let Some(quoted) = rest.split('"').nth(1) {
            if let Some(version) = major_minor(quoted) {
                return Ok((version, "mix.exs".into()));
            }
        }
    }

    Ok((
        DEFAULT_ELIXIR_VERSION.to_string(),
        "autopack default".into(),
    ))
}

fn major_minor(value: &str) -> Option<String> {
    let cleaned = value
        .trim()
        .trim_start_matches(['~', '>', '=', '<', '^', ' ']);
    let mut parts = cleaned.split('.');
    let major: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let minor: String = parts
        .next()
        .unwrap_or("")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if major.is_empty() || minor.is_empty() {
        return None;
    }
    Some(format!("{major}.{minor}"))
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, try_plan_for, write_app};

    const MIX_EXS: &str = r#"
defmodule MyApp.MixProject do
  use Mix.Project

  def project do
    [app: :my_app, version: "0.1.0", elixir: "~> 1.17"]
  end
end
"#;

    #[test]
    fn builds_a_release_and_runs_it() {
        let (_dir, app) = write_app(&[("mix.exs", MIX_EXS), ("mix.lock", "%{}")]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "elixir");
        assert_eq!(analysis.metadata["elixirVersion"], "1.17");
        assert_eq!(analysis.metadata["otpApp"], "my_app");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("/app/release/bin/my_app start")
        );
        // The release bundles ERTS: no Elixir in the runtime image.
        assert!(analysis.plan.deploy.paths.is_empty());
    }

    #[test]
    fn phoenix_projects_deploy_assets() {
        let (_dir, app) = write_app(&[
            (
                "mix.exs",
                "def project do [app: :web, deps: [{:phoenix, \"~> 1.7\"}]] end",
            ),
            ("assets/app.js", ""),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.metadata["framework"], "phoenix");
        assert!(analysis
            .plan
            .step("build")
            .unwrap()
            .commands
            .iter()
            .any(|command| command.display_name() == "mix assets.deploy"));
    }

    #[test]
    fn a_mix_project_without_an_app_name_is_an_actionable_error() {
        let (_dir, app) = write_app(&[("mix.exs", "defmodule X do end")]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("AUTOPACK_START_CMD"), "{err}");
    }

    #[test]
    fn tool_versions_beats_the_mix_constraint() {
        let (_dir, app) = write_app(&[
            ("mix.exs", MIX_EXS),
            (".tool-versions", "erlang 27.2\nelixir 1.18.1-otp-27\n"),
        ]);
        assert_eq!(plan_for(&app).metadata["elixirVersion"], "1.18");
    }
}
