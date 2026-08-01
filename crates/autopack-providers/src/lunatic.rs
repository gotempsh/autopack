//! Lunatic provider: Rust compiled to WebAssembly, run on the Lunatic runtime.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result};

use crate::support::procfile_web_command;

/// Lunatic release installed into the runtime image.
const DEFAULT_LUNATIC_VERSION: &str = "0.13.2";

/// WASI target the module is compiled for.
const WASM_TARGET: &str = "wasm32-wasip1";

/// Where the compiled module is placed.
const OUTPUT_WASM: &str = "/app/bin/app.wasm";

/// Builds Lunatic applications.
///
/// Registered ahead of the Rust provider: a Lunatic app *is* a Cargo project,
/// so the plain Rust provider would claim it and produce a native binary that
/// the Lunatic runtime cannot execute.
pub struct LunaticProvider;

impl Provider for LunaticProvider {
    fn id(&self) -> &'static str {
        "lunatic"
    }

    fn display_name(&self) -> &'static str {
        "Lunatic"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        let Some(manifest) = app.read_file_opt("Cargo.toml")? else {
            return Ok(false);
        };
        // `lunatic` as a dependency, or a `.cargo/config.toml` that already
        // targets WASI with the lunatic runner.
        Ok(manifest.contains("lunatic"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let version = ctx
            .env
            .config("LUNATIC_VERSION")
            .unwrap_or(DEFAULT_LUNATIC_VERSION)
            .to_string();
        ctx.packages.add("rust", "stable", "lunatic requires cargo");
        // The wasm module itself needs no C toolchain, but proc-macro crates
        // and the lunatic runtime's own build scripts compile natively and
        // shell out to `cc`.
        ctx.build_apt_packages.extend(
            // lunatic-runtime pulls openssl-sys, which needs the headers.
            ["build-essential", "pkg-config", "libssl-dev"]
                .into_iter()
                .map(String::from),
        );
        ctx.add_metadata("lunaticVersion", &version);
        ctx.add_metadata("target", WASM_TARGET);

        // Compiling to wasm needs no C toolchain, but fetching crates does need
        // git and TLS.
        let registry = ctx.shared_cache("cargo-registry", "/root/.cargo/registry");
        let target_cache = ctx.locked_cache("cargo-target", "/app/target");

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];
        step.add_cache(registry);
        step.add_cache(target_cache);
        step.add_command(Command::shell(format!("rustup target add {WASM_TARGET}")));
        step.add_command(Command::shell(format!(
            "cargo build --release --target {WASM_TARGET}"
        )));
        // `target/` is a cache mount and does not survive the step, so the
        // module is copied out in the same command.
        step.add_command(Command::shell(format!(
            "mkdir -p /app/bin && cp \"$(find target/{WASM_TARGET}/release -maxdepth 1 -name '*.wasm' -print -quit)\" {OUTPUT_WASM}"
        )));
        // The runtime binary is built from source rather than downloaded:
        // upstream publishes only `lunatic-linux-amd64`, so a release tarball
        // cannot run on arm64 at all. Cargo is already in this stage.
        step.add_command(Command::shell(format!(
            // Deliberately not `--locked`: lunatic-runtime 0.13.2 ships a 2023
            // lockfile pinning a `time` release that no longer compiles on
            // current Rust (E0282 in its Box inference). Letting cargo
            // re-resolve is what makes it build at all.
            "cargo install lunatic-runtime --version {version} --root /app/lunatic"
        )));

        // The runtime image needs the lunatic binary but no Rust toolchain.
        ctx.set_runtime_includes_runtimes(false);
        ctx.add_deploy_input(
            Layer::step(steps::BUILD).including([OUTPUT_WASM, "/app/lunatic/bin/lunatic"]),
        );
        ctx.add_deploy_path("/app/lunatic/bin");

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None => format!("lunatic run {OUTPUT_WASM}"),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, write_app};

    const CARGO_TOML: &str = "[package]\nname = \"demo\"\nversion = \"1.0.0\"\n\n\
                              [dependencies]\nlunatic = \"0.14\"\n";

    #[test]
    fn compiles_to_wasm_and_runs_on_lunatic() {
        let (_dir, app) = write_app(&[("Cargo.toml", CARGO_TOML), ("src/main.rs", "fn main() {}")]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "lunatic");
        assert_eq!(analysis.metadata["target"], "wasm32-wasip1");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("lunatic run /app/bin/app.wasm")
        );
    }

    #[test]
    fn a_plain_cargo_project_is_still_rust() {
        // Only a lunatic dependency diverts a Cargo project away from the
        // native Rust build.
        let (_dir, app) = write_app(&[
            ("Cargo.toml", "[package]\nname = \"demo\"\n"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(plan_for(&app).provider, "rust");
    }
}
