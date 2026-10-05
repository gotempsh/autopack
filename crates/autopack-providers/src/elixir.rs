//! Elixir provider, including Phoenix.

use autopack_core::config::SPREAD;
use autopack_core::plan::{Command, GeneratedValue, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result, APP_DIR};

use crate::support::procfile_web_command;
use crate::version::{Requirement, Version};

/// Elixir version used when `mix.exs` does not constrain one.
const DEFAULT_ELIXIR_VERSION: &str = "1.18";

/// `major.minor` lines the official `elixir` image publishes.
const ELIXIR_MINORS: &[&str] = &[
    "1.12", "1.13", "1.14", "1.15", "1.16", "1.17", "1.18", "1.19", "1.20",
];

/// Newest line a `mix.exs` range resolves to while it admits one. Newer lines
/// also move to a newer OTP, which old dependencies are the likeliest to trip
/// over.
const ELIXIR_RANGE_CEILING: &str = "1.18";

/// Where `mix release` writes the self-contained release.
const RELEASE_DIR: &str = "/app/release";

/// Mix's home when the app runs under `mix` rather than as a release: inside
/// the app directory so it reaches the runtime image and the unprivileged
/// user can read the Hex archive installed there.
const APP_MIX_HOME: &str = "/app/.mix";

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
        // Configured `erlang`/`elixir` runtimes (a legacy nixpacks.toml lists
        // them) would install a second OTP next to the image's own.
        ctx.set_base_image_runtimes(["erlang", "elixir"]);
        // A release bundles ERTS, which links against the builder's glibc,
        // OpenSSL and ncurses. The `-slim` variant of the same tag is built on
        // the same Debian release, so those always match; a fixed
        // `debian:<release>-slim` breaks as soon as the official image moves
        // to a newer release (`GLIBC_2.38' not found`).
        ctx.set_runtime_base_image(format!("elixir:{version}-slim"));
        ctx.set_runtime_includes_runtimes(false);

        ctx.add_metadata("elixirVersion", &version);
        ctx.add_metadata("elixirVersionSource", source);
        ctx.add_metadata("otpApp", &app_name);

        let is_phoenix = mix.contains(":phoenix");
        let has_assets = ctx.app.has_dir("assets");
        if is_phoenix {
            ctx.add_metadata("framework", "phoenix");
        }
        let builds_release = config_builds_release(ctx);
        if !builds_release {
            ctx.add_note(
                "the configured build commands do not run `mix release`; \
                 shipping the compiled app and running it with mix",
            );
        }

        self.plan_install(ctx, builds_release)?;
        self.plan_build(ctx, is_phoenix && has_assets, builds_release)?;

        ctx.add_deploy_variable("MIX_ENV", "prod");
        ctx.add_deploy_variable("LANG", "C.UTF-8");
        if builds_release {
            // Keep the matching slim image's system libraries, but a release
            // bundles its own ERTS and never needs the Elixir/Mix toolchain.
            ctx.add_runtime_command(Command::shell(
                "rm -rf /usr/local/lib/elixir && rm -f /usr/local/bin/elixir /usr/local/bin/elixirc /usr/local/bin/mix /usr/local/bin/iex",
            ));
            ctx.add_deploy_input(Layer::step(steps::BUILD).including([RELEASE_DIR]));
            // Releases refuse to boot without a cookie; a stable one avoids a
            // different value on every restart breaking clustering.
            ctx.add_deploy_variable("RELEASE_DISTRIBUTION", "none");
        } else {
            ctx.add_deploy_input(Layer::step(steps::BUILD).including([APP_DIR]));
            ctx.add_deploy_variable("MIX_HOME", APP_MIX_HOME);
            // The slim image ships Elixir; mix needs it at runtime.
            ctx.set_runtime_includes_runtimes(true);
            // Every mix task checks dependencies first, and for a git
            // dependency that runs `git`; without it the app crashes at boot
            // with `ErlangError :enoent` from `Mix.SCM.Git`.
            // Declared in mix.exs (`git:`/`github:`), or only visible in
            // mix.lock for a transitive one.
            let lock = ctx.app.read_file_opt("mix.lock")?.unwrap_or_default();
            if lock.contains("{:git,") || mix.contains("github:") || mix.contains("git:") {
                ctx.deploy_apt_packages.push("git".to_string());
            }
        }
        if is_phoenix {
            // A release only starts the endpoint when asked to; without it the
            // container boots, listens on nothing and fails its health check.
            ctx.add_deploy_variable("PHX_SERVER", "true");
            // `config/runtime.exs` raises at boot when either is missing.
            ctx.require_generated_variable(
                "SECRET_KEY_BASE",
                GeneratedValue::HexSecret { bytes: 64 },
            );
            ctx.require_generated_variable("PHX_HOST", GeneratedValue::PublicHost);
        }

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None if builds_release => format!("{RELEASE_DIR}/bin/{app_name} start"),
            None if is_phoenix => "mix phx.server".to_string(),
            None => "mix run --no-halt".to_string(),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

/// Whether the build step still runs `mix release`: true unless configuration
/// (a `nixpacks.toml` `[phases.build]`, say) replaced the generated commands
/// with ones that never call it.
fn config_builds_release(ctx: &BuildContext<'_>) -> bool {
    let Some(commands) = ctx
        .config
        .steps
        .get(steps::BUILD)
        .and_then(|patch| patch.commands.as_ref())
    else {
        return true;
    };
    commands.iter().any(|command| match command {
        Command::Exec(exec) => exec.cmd == SPREAD || exec.cmd.contains("mix release"),
        _ => false,
    })
}

impl ElixirProvider {
    fn plan_install(&self, ctx: &mut BuildContext<'_>, builds_release: bool) -> Result<()> {
        // Cache only the download cache, never MIX_HOME/HEX_HOME themselves.
        // `mix local.hex` installs an archive into MIX_HOME; if MIX_HOME is a
        // cache mount, that archive is not in the layer and the next step
        // fails with the memorably unhelpful "Could not find an SCM for
        // dependency".
        // Keyed by image: Hex and rebar artefacts built for one OTP release
        // fail to load on another, so builds on different versions must never
        // share a cache.
        let image_key: String = ctx
            .base_image()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let deps_cache = ctx.shared_cache(format!("hex-{image_key}"), "/root/.hex/cache");

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
        if !builds_release {
            step.add_variable("MIX_HOME", APP_MIX_HOME);
        }
        step.add_cache(deps_cache);
        step.add_command(Command::shell(
            "mix local.hex --force && mix local.rebar --force",
        ));
        step.add_command(Command::shell("mix deps.get --only prod"));
        step.add_command(Command::shell("mix deps.compile"));
        Ok(())
    }

    fn plan_build(
        &self,
        ctx: &mut BuildContext<'_>,
        deploy_assets: bool,
        builds_release: bool,
    ) -> Result<()> {
        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
        step.add_variable("MIX_ENV", "prod");
        if !builds_release {
            step.add_variable("MIX_HOME", APP_MIX_HOME);
        }
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

/// The `elixir` image tag to build with.
///
/// `.tool-versions` pins are used as written, keeping their OTP release:
/// `elixir 1.18.1-otp-27` is published as `elixir:1.18.1-otp-27`, and an
/// `erlang 27.2` line next to a plain Elixir version selects the `-otp-27`
/// variant. A `mix.exs` requirement (`~> 1.14`) is a range; it resolves to the
/// newest published line it admits rather than its lower bound, because the
/// lockfile was most likely written on a recent toolchain and newer
/// dependency options (`depth:` on git dependencies) do not exist on old ones.
fn elixir_version(app: &App, mix: &str) -> Result<(String, String)> {
    if let Some(tool_versions) = app.read_file_opt(".tool-versions")? {
        let entry = |tool: &str| {
            tool_versions.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some(tool))
                    .then(|| words.next().map(str::to_string))
                    .flatten()
            })
        };
        if let Some(elixir) = entry("elixir") {
            let (elixir_version, otp) = match elixir.split_once("-otp-") {
                Some((version, otp)) => (version.to_string(), Some(otp.to_string())),
                None => (elixir.clone(), None),
            };
            let otp = otp.or_else(|| {
                entry("erlang").and_then(|erlang| {
                    Version::parse(&erlang).map(|version| version.major.to_string())
                })
            });
            if Version::parse(&elixir_version).is_some()
                && elixir_version
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.')
            {
                let tag = match otp.filter(|otp| otp.chars().all(|c| c.is_ascii_digit())) {
                    Some(otp) => format!("{elixir_version}-otp-{otp}"),
                    None => elixir_version,
                };
                return Ok((tag, ".tool-versions".into()));
            }
        }
    }

    if let Some(requirement) = mix_elixir_requirement(mix) {
        if let Some(exact) = requirement.exact() {
            return Ok((exact.to_string(), "mix.exs".into()));
        }
        let ceiling =
            Version::parse(ELIXIR_RANGE_CEILING).map(|version| (version.major, version.minor));
        let settled: Vec<&str> = ELIXIR_MINORS
            .iter()
            .copied()
            .filter(|minor| Version::parse(minor).map(|v| (v.major, v.minor)) <= ceiling)
            .collect();
        if let Some(minor) = requirement
            .newest(&settled)
            .or_else(|| requirement.newest(ELIXIR_MINORS))
        {
            return Ok((minor.to_string(), "mix.exs".into()));
        }
    }

    Ok((
        DEFAULT_ELIXIR_VERSION.to_string(),
        "autopack default".into(),
    ))
}

/// The requirement in `elixir: "..."` inside `mix.exs`'s project config.
fn mix_elixir_requirement(mix: &str) -> Option<Requirement> {
    let start = mix.find("elixir:")?;
    let rest = &mix[start + "elixir:".len()..];
    let quoted = rest.split('"').nth(1)?;
    Requirement::parse(quoted)
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, try_plan_for, write_app};
    use autopack_core::plan::GeneratedValue;

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
        // `~> 1.17` admits every later 1.x; the newest settled line wins.
        assert_eq!(analysis.metadata["elixirVersion"], "1.18");
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
    fn phoenix_releases_start_the_endpoint_with_generated_secrets() {
        let (_dir, app) = write_app(&[(
            "mix.exs",
            "def project do [app: :web, deps: [{:phoenix, \"~> 1.7\"}]] end",
        )]);
        let deploy = plan_for(&app).plan.deploy;

        assert_eq!(deploy.variables["PHX_SERVER"], "true");
        assert_eq!(
            deploy
                .generated_variables
                .get("SECRET_KEY_BASE")
                .map(|v| &v.value),
            Some(&GeneratedValue::HexSecret { bytes: 64 })
        );
        assert_eq!(
            deploy.generated_variables.get("PHX_HOST").map(|v| &v.value),
            Some(&GeneratedValue::PublicHost)
        );
        assert!(!deploy.variables.contains_key("SECRET_KEY_BASE"));
    }

    #[test]
    fn plain_mix_apps_need_no_generated_secrets() {
        let (_dir, app) = write_app(&[("mix.exs", "def project do [app: :worker] end")]);
        let deploy = plan_for(&app).plan.deploy;
        assert!(deploy.generated_variables.is_empty());
        assert!(!deploy.variables.contains_key("PHX_SERVER"));
    }

    #[test]
    fn a_build_override_without_mix_release_ships_the_compiled_app() {
        let (_dir, app) = write_app(&[
            (
                "mix.exs",
                "def project do [app: :web, deps: [{:phoenix, \"~> 1.7\"}]] end",
            ),
            (
                "nixpacks.toml",
                "[phases.build]\ncmds = ['mix compile', 'mix assets.deploy']\n",
            ),
        ]);
        let plan = plan_for(&app).plan;

        assert_eq!(plan.deploy.start_command.as_deref(), Some("mix phx.server"));
        assert!(!format!("{:?}", plan.step("runtime").unwrap().commands)
            .contains("rm -rf /usr/local/lib/elixir"));
        assert_eq!(plan.deploy.variables["MIX_HOME"], "/app/.mix");
        let inputs = format!("{:?}", plan.deploy.inputs);
        assert!(!inputs.contains("/app/release"), "{inputs}");
        assert_eq!(
            plan.step("install").unwrap().variables["MIX_HOME"],
            "/app/.mix"
        );
    }

    #[test]
    fn running_under_mix_with_git_dependencies_ships_git() {
        let (_dir, app) = write_app(&[
            (
                "mix.exs",
                "def project do [app: :web, deps: [{:phoenix, \"~> 1.7\"}]] end",
            ),
            (
                "mix.lock",
                r#"%{"heroicons": {:git, "https://github.com/tailwindlabs/heroicons.git", "abc", []}}"#,
            ),
            ("nixpacks.toml", "[phases.build]\ncmds = ['mix compile']\n"),
        ]);
        let plan = format!("{:?}", plan_for(&app).plan);
        assert!(plan.contains("'tini' 'git'"), "{plan}");
    }

    #[test]
    fn a_git_dependency_declared_only_in_mix_exs_also_ships_git() {
        let (_dir, app) = write_app(&[
            (
                "mix.exs",
                "def project do [app: :web, deps: [{:phoenix, \"~> 1.7\"}, \
                 {:heroicons, github: \"tailwindlabs/heroicons\", tag: \"v2.1.1\"}]] end",
            ),
            ("nixpacks.toml", "[phases.build]\ncmds = ['mix compile']\n"),
        ]);
        let plan = format!("{:?}", plan_for(&app).plan);
        assert!(plan.contains("'tini' 'git'"), "{plan}");
    }

    #[test]
    fn a_build_override_that_still_releases_keeps_the_release() {
        let (_dir, app) = write_app(&[
            ("mix.exs", "def project do [app: :web] end"),
            (
                "nixpacks.toml",
                "[phases.build]\ncmds = ['...', 'echo done']\n",
            ),
        ]);
        let plan = plan_for(&app).plan;
        assert_eq!(
            plan.deploy.start_command.as_deref(),
            Some("/app/release/bin/web start")
        );
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
        assert_eq!(plan_for(&app).metadata["elixirVersion"], "1.18.1-otp-27");
    }

    #[test]
    fn an_erlang_pin_selects_the_matching_otp_variant() {
        let (_dir, app) = write_app(&[
            ("mix.exs", MIX_EXS),
            (".tool-versions", "elixir 1.17.3\nerlang 26.2.5\n"),
        ]);
        assert_eq!(plan_for(&app).metadata["elixirVersion"], "1.17.3-otp-26");
    }

    #[test]
    fn a_lower_bound_constraint_resolves_to_the_newest_settled_line() {
        // A Phoenix lockfile using newer git-dependency options does not
        // compile on the constraint's lower bound.
        let mix = MIX_EXS.replace("~> 1.17", "~> 1.14");
        let (_dir, app) = write_app(&[("mix.exs", mix.as_str())]);
        assert_eq!(plan_for(&app).metadata["elixirVersion"], "1.18");

        let mix = MIX_EXS.replace("~> 1.17", "== 1.15.7");
        let (_dir, app) = write_app(&[("mix.exs", mix.as_str())]);
        assert_eq!(plan_for(&app).metadata["elixirVersion"], "1.15.7");

        // Only newer lines admit `~> 1.19`.
        let mix = MIX_EXS.replace("~> 1.17", "~> 1.19");
        let (_dir, app) = write_app(&[("mix.exs", mix.as_str())]);
        assert_eq!(plan_for(&app).metadata["elixirVersion"], "1.20");
    }

    #[test]
    fn the_runtime_image_is_the_slim_variant_of_the_builder() {
        // ERTS in the release links against the builder's glibc; a runtime on
        // an older Debian fails with "GLIBC_2.38 not found".
        let (_dir, app) = write_app(&[("mix.exs", MIX_EXS)]);
        let analysis = plan_for(&app);
        assert!(analysis.plan.steps.iter().any(|step| step
            .inputs
            .first()
            .and_then(|input| input.image.as_deref())
            == Some("elixir:1.18")));
        assert_eq!(
            analysis.plan.step("runtime").unwrap().inputs[0]
                .image
                .as_deref(),
            Some("elixir:1.18-slim")
        );
    }

    #[test]
    fn release_runtime_removes_elixir_tools() {
        let (_dir, app) = write_app(&[("mix.exs", MIX_EXS)]);
        let analysis = plan_for(&app);
        let commands = format!("{:?}", analysis.plan.step("runtime").unwrap().commands);
        assert!(commands.contains("rm -rf /usr/local/lib/elixir"));
        assert!(commands.contains("/usr/local/bin/mix"));
    }

    #[test]
    fn the_hex_cache_is_keyed_by_image() {
        let (_dir, app) = write_app(&[("mix.exs", MIX_EXS)]);
        let analysis = plan_for(&app);
        assert!(analysis
            .plan
            .caches
            .keys()
            .any(|key| key.contains("elixir-1.18")));
        assert!(!analysis.plan.caches.keys().any(|key| key == "hex"));
    }

    #[test]
    fn a_legacy_nixpacks_erlang_does_not_install_a_second_otp() {
        let (_dir, app) = write_app(&[
            ("mix.exs", MIX_EXS),
            (
                "nixpacks.toml",
                "[phases.setup]\nnixPkgs = ['...', 'erlang']\n",
            ),
        ]);
        let analysis = plan_for(&app);
        assert!(analysis.packages.iter().all(|(tool, _)| tool != "erlang"));
        assert!(analysis
            .metadata
            .values()
            .any(|value| value.contains("erlang") && value.contains("already provides")));
    }
}
