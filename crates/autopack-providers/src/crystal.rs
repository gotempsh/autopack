//! Crystal provider.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::{
    install_recorded_runtime_libraries, procfile_web_command, record_runtime_libraries,
    ELF_INSPECTION_PACKAGE, RUNTIME_DEPS_FILE,
};

/// Crystal release used when the project does not pin one.
const DEFAULT_CRYSTAL_VERSION: &str = "1.14.0";

/// Where the compiled binary is placed.
const OUTPUT_BINARY: &str = "/app/bin/app";

/// Builds Crystal applications.
pub struct CrystalProvider;

impl Provider for CrystalProvider {
    fn id(&self) -> &'static str {
        "crystal"
    }

    fn display_name(&self) -> &'static str {
        "Crystal"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("shard.yml"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let shard = ctx.app.read_file("shard.yml")?;
        let entry = entrypoint(ctx.app, &shard)?;

        let version = ctx
            .env
            .config("CRYSTAL_VERSION")
            .unwrap_or(DEFAULT_CRYSTAL_VERSION)
            .to_string();
        let image = format!("crystallang/crystal:{version}");
        ctx.set_base_image(&image);
        // The compiled binary links libgc, libevent and libpcre2. Their package
        // names are release-specific, so they are resolved from the binary
        // rather than hardcoded — but the runtime must stay on the same distro
        // family as the builder or the glibc versions will not match.
        ctx.set_runtime_base_image(&image);
        ctx.set_runtime_includes_runtimes(false);
        ctx.build_apt_packages
            .push(ELF_INSPECTION_PACKAGE.to_string());

        ctx.add_metadata("crystalVersion", &version);
        ctx.add_metadata("entrypoint", &entry);

        let cache = ctx.shared_cache("shards", "/root/.cache/shards");

        if ctx.app.has_file("shard.yml") {
            let manifests: Vec<&str> = ["shard.yml", "shard.lock"]
                .into_iter()
                .filter(|file| ctx.app.has_file(file))
                .collect();
            // `--production` is a frozen install: it refuses to run without a
            // shard.lock rather than resolving one ("E: Missing shard.lock").
            let install_command = if ctx.app.has_file("shard.lock") {
                "shards install --production"
            } else {
                "shards install"
            };

            let install = ctx.step(steps::INSTALL);
            install.add_input(Layer::local().including(manifests));
            install.add_cache(cache.clone());
            install.add_command(Command::shell(install_command));
        }

        let build = ctx.step(steps::BUILD);
        build.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
        build.add_cache(cache);
        build.add_command(Command::shell(format!(
            "mkdir -p /app/bin && crystal build --release --no-debug {entry} -o {OUTPUT_BINARY}"
        )));
        build.add_command(Command::shell(record_runtime_libraries(
            OUTPUT_BINARY,
            RUNTIME_DEPS_FILE,
        )));

        ctx.add_runtime_input(
            Layer::step(steps::BUILD).including([OUTPUT_BINARY, RUNTIME_DEPS_FILE]),
        );
        ctx.add_runtime_command(Command::shell(install_recorded_runtime_libraries(
            RUNTIME_DEPS_FILE,
        )));
        ctx.add_deploy_input(Layer::step(steps::BUILD).including([OUTPUT_BINARY]));

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None => OUTPUT_BINARY.to_string(),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

/// The Crystal file to compile.
fn entrypoint(app: &App, shard: &str) -> Result<String> {
    // `targets:` names the executables; the first one is the app.
    if let Some(rest) = shard.split("targets:").nth(1) {
        for line in rest.lines() {
            let trimmed = line.trim();
            if let Some(main) = trimmed.strip_prefix("main:") {
                let path = main.trim().trim_matches(['"', '\'']);
                if !path.is_empty() {
                    return Ok(path.to_string());
                }
            }
        }
    }

    let name = shard.lines().find_map(|line| {
        line.strip_prefix("name:")
            .map(|n| n.trim().trim_matches(['"', '\'']).to_string())
    });

    let mut candidates = Vec::new();
    if let Some(name) = &name {
        candidates.push(format!("src/{name}.cr"));
    }
    candidates.push("src/main.cr".to_string());

    if let Some(found) = candidates.iter().find(|path| app.has_file(path)) {
        return Ok(found.clone());
    }

    Err(Error::provider(
        "crystal",
        format!(
            "no entrypoint found (looked for {}). Declare one under `targets:` \
             in shard.yml, or set `AUTOPACK_BUILD_CMD` and `AUTOPACK_START_CMD`",
            candidates.join(", ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, try_plan_for, write_app};

    #[test]
    fn compiles_the_named_target() {
        let (_dir, app) = write_app(&[
            (
                "shard.yml",
                "name: demo\nversion: 1.0.0\ntargets:\n  demo:\n    main: src/demo.cr\n",
            ),
            ("src/demo.cr", "puts \"hi\""),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "crystal");
        assert_eq!(analysis.metadata["entrypoint"], "src/demo.cr");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("crystal build --release"));
    }

    #[test]
    fn frozen_install_only_with_a_lockfile() {
        let (_dir, no_lock) = write_app(&[
            (
                "shard.yml",
                "name: demo
targets:
  demo:
    main: src/demo.cr
",
            ),
            ("src/demo.cr", ""),
        ]);
        assert_eq!(
            plan_for(&no_lock).plan.step("install").unwrap().commands[0].display_name(),
            "shards install"
        );

        let (_dir2, locked) = write_app(&[
            (
                "shard.yml",
                "name: demo
targets:
  demo:
    main: src/demo.cr
",
            ),
            (
                "shard.lock",
                "version: 2.0
shards: {}
",
            ),
            ("src/demo.cr", ""),
        ]);
        assert_eq!(
            plan_for(&locked).plan.step("install").unwrap().commands[0].display_name(),
            "shards install --production"
        );
    }

    #[test]
    fn falls_back_to_the_shard_name() {
        let (_dir, app) = write_app(&[
            ("shard.yml", "name: server\nversion: 1.0.0\n"),
            ("src/server.cr", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["entrypoint"], "src/server.cr");
    }

    #[test]
    fn a_shard_without_an_entrypoint_is_an_actionable_error() {
        let (_dir, app) = write_app(&[("shard.yml", "name: lib\n"), ("src/other.cr", "")]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("AUTOPACK_BUILD_CMD"), "{err}");
    }
}
