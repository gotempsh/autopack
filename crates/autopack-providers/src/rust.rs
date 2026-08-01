//! Rust provider.

use serde::Deserialize;

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::procfile_web_command;

/// Toolchain used when the project does not pin one.
const DEFAULT_RUST_VERSION: &str = "stable";

/// Where the compiled binary is placed.
const OUTPUT_BINARY: &str = "/app/bin/app";

/// Builds Rust applications with cargo.
pub struct RustProvider;

/// The parts of `Cargo.toml` that identify the binary to run.
#[derive(Debug, Default, Deserialize)]
struct CargoManifest {
    #[serde(default)]
    package: Option<CargoPackage>,
    #[serde(default)]
    bin: Vec<CargoBin>,
    #[serde(default)]
    workspace: Option<CargoWorkspace>,
}

#[derive(Debug, Default, Deserialize)]
struct CargoPackage {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "default-run")]
    default_run: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CargoBin {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CargoWorkspace {
    #[serde(default)]
    #[allow(dead_code)]
    members: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RustToolchainFile {
    #[serde(default)]
    toolchain: Option<RustToolchain>,
}

#[derive(Debug, Default, Deserialize)]
struct RustToolchain {
    #[serde(default)]
    channel: Option<String>,
}

impl Provider for RustProvider {
    fn id(&self) -> &'static str {
        "rust"
    }

    fn display_name(&self) -> &'static str {
        "Rust"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("Cargo.toml"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let (version, source) = rust_version(ctx.app)?;
        ctx.packages.add("rust", &version, source);
        ctx.add_metadata("rustVersion", &version);

        // rustc shells out to `cc` for linking, and debian-slim has no
        // compiler at all. pkg-config comes along because it is what every
        // `*-sys` crate's build script reaches for next.
        ctx.build_apt_packages.extend(
            ["build-essential", "pkg-config"]
                .into_iter()
                .map(String::from),
        );

        let manifest: CargoManifest = ctx.app.read_toml("Cargo.toml")?;
        let binary = binary_name(ctx.app, &manifest)?;
        ctx.add_metadata("binary", &binary);

        // Cached at cargo's default location rather than by moving CARGO_HOME:
        // mise installs Rust through rustup, and repointing CARGO_HOME makes
        // its `cargo` shim resolve to nothing ("cargo is not a valid shim").
        let registry_cache = ctx.shared_cache("cargo-registry", "/root/.cargo/registry");
        let git_cache = ctx.shared_cache("cargo-git", "/root/.cargo/git");
        // Concurrent cargo builds sharing one target dir corrupt each other's
        // fingerprints, so this cache has to be exclusive.
        let target_cache = ctx.locked_cache("cargo-target", "/app/target");

        let locked = if ctx.app.has_file("Cargo.lock") {
            " --locked"
        } else {
            ""
        };

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];
        step.add_variable("CARGO_TERM_COLOR", "always");
        step.add_cache(registry_cache);
        step.add_cache(git_cache);
        step.add_cache(target_cache);
        // `target/` is a cache mount, so its contents do not survive the step.
        // The binary has to be copied somewhere real in the same command.
        step.add_command(Command::shell(format!(
            "cargo build --release{locked} --bin {binary} && \
             mkdir -p /app/bin && cp target/release/{binary} {OUTPUT_BINARY}"
        )));

        ctx.set_runtime_includes_runtimes(false);
        ctx.add_deploy_input(Layer::step(steps::BUILD).including([OUTPUT_BINARY]));

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None => OUTPUT_BINARY.to_string(),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

/// The toolchain channel to install.
fn rust_version(app: &App) -> Result<(String, String)> {
    if let Some(file) = app.read_toml_opt::<RustToolchainFile>("rust-toolchain.toml")? {
        if let Some(channel) = file.toolchain.and_then(|t| t.channel) {
            return Ok((channel, "rust-toolchain.toml".into()));
        }
    }

    // The legacy `rust-toolchain` file is a bare channel string.
    if let Some(contents) = app.read_file_opt("rust-toolchain")? {
        if let Some(channel) = contents.lines().map(str::trim).find(|l| !l.is_empty()) {
            return Ok((channel.to_string(), "rust-toolchain".into()));
        }
    }

    Ok((DEFAULT_RUST_VERSION.to_string(), "autopack default".into()))
}

/// Which binary target to build and run.
fn binary_name(app: &App, manifest: &CargoManifest) -> Result<String> {
    if let Some(package) = &manifest.package {
        if let Some(default_run) = &package.default_run {
            return Ok(default_run.clone());
        }
    }

    let declared: Vec<String> = manifest
        .bin
        .iter()
        .filter_map(|bin| bin.name.clone())
        .collect();
    if declared.len() == 1 {
        return Ok(declared[0].clone());
    }
    if declared.len() > 1 {
        return Err(Error::provider(
            "rust",
            format!(
                "Cargo.toml declares several binaries ({}). Choose one with \
                 `default-run` in Cargo.toml, or set \
                 `AUTOPACK_BUILD_CMD` and `AUTOPACK_START_CMD`",
                declared.join(", ")
            ),
        ));
    }

    if let Some(name) = manifest.package.as_ref().and_then(|p| p.name.clone()) {
        return Ok(name);
    }

    // A virtual workspace root has no `[package]`, so there is no single
    // obvious binary to run.
    if manifest.workspace.is_some() {
        return Err(Error::provider(
            "rust",
            "this is a Cargo workspace root with no `[package]` section, so there is no \
             single binary to build.\nSet `AUTOPACK_BUILD_CMD` and `AUTOPACK_START_CMD`, \
             or point autopack at a member crate",
        ));
    }

    let _ = app;
    Err(Error::provider(
        "rust",
        "Cargo.toml has no `[package].name`, so the binary to build is unknown",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{plan_for, try_plan_for, write_app};

    #[test]
    fn builds_the_package_binary() {
        let (_dir, app) = write_app(&[
            (
                "Cargo.toml",
                "[package]\nname = \"api\"\nversion = \"0.1.0\"\n",
            ),
            ("Cargo.lock", ""),
            ("src/main.rs", "fn main() {}"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "rust");
        assert_eq!(analysis.metadata["binary"], "api");
        let build = analysis.plan.step("build").unwrap();
        assert!(build.commands[0]
            .display_name()
            .contains("cargo build --release --locked --bin api"));
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some(OUTPUT_BINARY)
        );
    }

    #[test]
    fn a_c_toolchain_is_installed_for_linking() {
        let (_dir, app) = write_app(&[
            ("Cargo.toml", "[package]\nname = \"api\"\n"),
            ("src/main.rs", "fn main() {}"),
        ]);
        let analysis = plan_for(&app);
        let packages = analysis.plan.step("packages").unwrap();
        assert!(
            packages.commands[0]
                .display_name()
                .contains("build-essential"),
            "{}",
            packages.commands[0].display_name()
        );
    }

    #[test]
    fn omits_locked_without_a_lockfile() {
        let (_dir, app) = write_app(&[
            ("Cargo.toml", "[package]\nname = \"api\"\n"),
            ("src/main.rs", "fn main() {}"),
        ]);
        let analysis = plan_for(&app);
        let build = analysis.plan.step("build").unwrap();
        assert!(!build.commands[0].display_name().contains("--locked"));
    }

    #[test]
    fn rust_toolchain_pins_the_channel() {
        let (_dir, app) = write_app(&[
            ("Cargo.toml", "[package]\nname = \"api\"\n"),
            ("rust-toolchain.toml", "[toolchain]\nchannel = \"1.82.0\"\n"),
            ("src/main.rs", "fn main() {}"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["rustVersion"], "1.82.0");
    }

    #[test]
    fn workspace_roots_ask_for_an_explicit_command() {
        let (_dir, app) = write_app(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            ("crates/api/Cargo.toml", "[package]\nname = \"api\"\n"),
        ]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("workspace root"), "{err}");
    }
}
