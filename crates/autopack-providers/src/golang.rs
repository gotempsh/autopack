//! Go provider.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::procfile_web_command;

/// Go version used when `go.mod` does not declare one.
const DEFAULT_GO_VERSION: &str = "1.23";

/// Where the compiled binary is placed.
const OUTPUT_BINARY: &str = "/app/bin/app";

/// Builds Go applications into a single static binary.
pub struct GoProvider;

impl Provider for GoProvider {
    fn id(&self) -> &'static str {
        "go"
    }

    fn display_name(&self) -> &'static str {
        "Go"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("go.mod") || app.has_match("**/*.go"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let (version, source) = go_version(ctx.app)?;
        ctx.packages.add("go", &version, source);
        ctx.add_metadata("goVersion", &version);

        let module_cache = ctx.shared_cache("go-mod", "/cache/go-mod");
        let build_cache = ctx.shared_cache("go-build", "/cache/go-build");

        // Without a go.sum, `go build` refuses to resolve anything ("missing
        // go.sum entry") — Go stopped updating it implicitly in 1.16.
        // `go mod tidy` writes the missing entries, but it decides what is
        // needed by reading the imports, so it can only run once the source is
        // present. That rules out the manifest-only install step, whose whole
        // point is to run before the source is copied.
        let has_lockfile = ctx.app.has_file("go.sum");
        ctx.add_metadata(
            "moduleResolution",
            if has_lockfile {
                "go mod download (go.sum present)"
            } else {
                "go mod tidy (no go.sum; runs with the source)"
            },
        );

        if ctx.app.has_file("go.mod") && has_lockfile {
            let step = ctx.step(steps::INSTALL);
            step.add_input(Layer::local().including(["go.mod", "go.sum"]));
            step.add_variable("GOMODCACHE", "/cache/go-mod");
            step.add_cache(module_cache.clone());
            step.add_command(Command::shell("go mod download"));
        }

        let package = main_package(ctx.app)?;
        ctx.add_metadata("mainPackage", &package);

        let base = if ctx.has_step(steps::INSTALL) {
            Layer::step(steps::INSTALL)
        } else {
            Layer::step(steps::PACKAGES)
        };

        let needs_tidy = !has_lockfile && ctx.app.has_file("go.mod");

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![base, Layer::local()];
        step.add_variable("GOMODCACHE", "/cache/go-mod");
        step.add_variable("GOCACHE", "/cache/go-build");
        // Static linking is what lets the runtime image be a bare Debian slim
        // with no Go toolchain and no libc version coupling.
        step.add_variable("CGO_ENABLED", "0");
        step.add_cache(module_cache);
        step.add_cache(build_cache);
        if needs_tidy {
            step.add_command(Command::shell("go mod tidy"));
        }
        step.add_command(Command::shell(format!(
            "go build -ldflags='-s -w' -o {OUTPUT_BINARY} {package}"
        )));

        // The compiled binary is self-contained, so the runtime image needs
        // neither mise nor the Go toolchain.
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

/// The Go version from the `go` directive in `go.mod`.
fn go_version(app: &App) -> Result<(String, String)> {
    let Some(contents) = app.read_file_opt("go.mod")? else {
        return Ok((DEFAULT_GO_VERSION.to_string(), "autopack default".into()));
    };

    for line in contents.lines() {
        let line = line.trim();
        // `toolchain go1.23.4` also pins a version but only as an upper bound;
        // the `go` directive is the language version the module targets.
        if let Some(version) = line.strip_prefix("go ") {
            let version = version.trim();
            if !version.is_empty() {
                return Ok((version.to_string(), "go.mod".into()));
            }
        }
    }

    Ok((DEFAULT_GO_VERSION.to_string(), "autopack default".into()))
}

/// The package path to build.
///
/// Guessing wrong here produces a build failure rather than a broken image, but
/// the failure is confusing, so only unambiguous layouts are accepted.
fn main_package(app: &App) -> Result<String> {
    if app.has_file("main.go") {
        return Ok(".".to_string());
    }

    let cmd_mains: Vec<String> = app
        .find_files("cmd/*/main.go")?
        .iter()
        .filter_map(|path| path.rsplit_once('/').map(|(dir, _)| format!("./{dir}")))
        .collect();

    match cmd_mains.len() {
        1 => Ok(cmd_mains[0].clone()),
        0 => Err(Error::provider(
            "go",
            "no `main.go` in the repository root and no `cmd/<name>/main.go`.\n\
             Point autopack at the right package with \
             `AUTOPACK_BUILD_CMD='go build -o /app/bin/app ./path/to/pkg'`",
        )),
        _ => Err(Error::provider(
            "go",
            format!(
                "found several commands ({}). Pick one with \
                 `AUTOPACK_BUILD_CMD='go build -o /app/bin/app ./cmd/<name>'`",
                cmd_mains.join(", ")
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{plan_for, try_plan_for, write_app};

    #[test]
    fn builds_a_root_main_package() {
        let (_dir, app) = write_app(&[
            ("go.mod", "module example.com/api\n\ngo 1.23\n"),
            ("main.go", "package main\nfunc main() {}\n"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "go");
        assert_eq!(analysis.metadata["goVersion"], "1.23");
        assert_eq!(analysis.metadata["mainPackage"], ".");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some(OUTPUT_BINARY)
        );
    }

    #[test]
    fn runtime_image_carries_only_the_binary() {
        let (_dir, app) = write_app(&[
            ("go.mod", "module example.com/api\n\ngo 1.23\n"),
            ("main.go", "package main\nfunc main() {}\n"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.plan.deploy.inputs.len(), 1);
        assert_eq!(
            analysis.plan.deploy.inputs[0].filter.include,
            vec![OUTPUT_BINARY.to_string()]
        );
        assert!(analysis.plan.deploy.paths.is_empty());
    }

    #[test]
    fn a_module_without_a_lockfile_is_tidied() {
        let (_dir, app) = write_app(&[
            ("go.mod", "module example.com/api\n\ngo 1.23\n"),
            ("main.go", "package main\nfunc main() {}\n"),
        ]);
        let analysis = plan_for(&app);
        assert!(analysis.metadata["moduleResolution"].starts_with("go mod tidy"));
        // Tidy needs the imports, so it belongs to the build step.
        assert!(analysis.plan.step("install").is_none());
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("go mod tidy"));
    }

    #[test]
    fn a_module_with_a_lockfile_only_downloads() {
        let (_dir, app) = write_app(&[
            ("go.mod", "module example.com/api\n\ngo 1.23\n"),
            ("go.sum", ""),
            ("main.go", "package main\nfunc main() {}\n"),
        ]);
        assert!(plan_for(&app).metadata["moduleResolution"].starts_with("go mod download"));
    }

    #[test]
    fn single_cmd_directory_is_unambiguous() {
        let (_dir, app) = write_app(&[
            ("go.mod", "module example.com/api\n\ngo 1.22\n"),
            ("cmd/server/main.go", "package main\nfunc main() {}\n"),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["mainPackage"], "./cmd/server");
    }

    #[test]
    fn several_commands_ask_the_user_to_choose() {
        let (_dir, app) = write_app(&[
            ("go.mod", "module example.com/api\n\ngo 1.22\n"),
            ("cmd/server/main.go", "package main"),
            ("cmd/worker/main.go", "package main"),
        ]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("AUTOPACK_BUILD_CMD"), "{err}");
        assert!(err.to_string().contains("./cmd/server"), "{err}");
    }
}
