//! Dart provider.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::procfile_web_command;

/// Where the compiled executable is placed.
const OUTPUT_BINARY: &str = "/app/bin/app";

/// Builds Dart applications ahead-of-time.
pub struct DartProvider;

impl Provider for DartProvider {
    fn id(&self) -> &'static str {
        "dart"
    }

    fn display_name(&self) -> &'static str {
        "Dart"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("pubspec.yaml"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let pubspec = ctx.app.read_file("pubspec.yaml")?;
        let entry = entrypoint(ctx.app, &pubspec)?;

        let channel = ctx.env.config("DART_VERSION").unwrap_or("stable");
        ctx.set_base_image(format!("dart:{channel}"));
        // `dart compile exe` produces a native binary, so the runtime needs no
        // Dart SDK — just libc.
        ctx.set_runtime_base_image("debian:bookworm-slim");
        ctx.set_runtime_includes_runtimes(false);
        ctx.add_metadata("dartChannel", channel);
        ctx.add_metadata("entrypoint", &entry);

        let cache = ctx.shared_cache("pub", "/root/.pub-cache");

        let manifests: Vec<&str> = ["pubspec.yaml", "pubspec.lock"]
            .into_iter()
            .filter(|file| ctx.app.has_file(file))
            .collect();

        let install = ctx.step(steps::INSTALL);
        install.add_input(Layer::local().including(manifests));
        install.add_cache(cache.clone());
        install.add_command(Command::shell("dart pub get"));

        let build = ctx.step(steps::BUILD);
        build.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
        build.add_cache(cache);
        // Re-resolving after the source arrives is cheap and covers path
        // dependencies that only exist once the full tree is present.
        build.add_command(Command::shell("dart pub get --offline || dart pub get"));
        build.add_command(Command::shell(format!(
            "mkdir -p /app/bin && dart compile exe {entry} -o {OUTPUT_BINARY}"
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

/// The Dart file to compile.
///
/// Convention places it at `bin/<package>.dart`, but `bin/main.dart` and
/// `bin/server.dart` are common enough to try.
fn entrypoint(app: &App, pubspec: &str) -> Result<String> {
    let package = pubspec.lines().find_map(|line| {
        line.strip_prefix("name:")
            .map(|name| name.trim().trim_matches(['"', '\'']).to_string())
    });

    let mut candidates = Vec::new();
    if let Some(package) = &package {
        candidates.push(format!("bin/{package}.dart"));
    }
    candidates.extend(["bin/main.dart".to_string(), "bin/server.dart".to_string()]);

    if let Some(found) = candidates.iter().find(|path| app.has_file(path)) {
        return Ok(found.clone());
    }

    // Anything else in bin/ is still unambiguous if there is exactly one.
    let in_bin = app.find_files("bin/*.dart")?;
    if in_bin.len() == 1 {
        return Ok(in_bin[0].clone());
    }

    Err(Error::provider(
        "dart",
        format!(
            "no obvious entrypoint in bin/ (looked for {}).\n\
             Set `AUTOPACK_BUILD_CMD` and `AUTOPACK_START_CMD`",
            candidates.join(", ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, try_plan_for, write_app};

    #[test]
    fn compiles_the_package_entrypoint() {
        let (_dir, app) = write_app(&[
            (
                "pubspec.yaml",
                "name: server\nenvironment:\n  sdk: ^3.5.0\n",
            ),
            ("bin/server.dart", "void main() {}"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "dart");
        assert_eq!(analysis.metadata["entrypoint"], "bin/server.dart");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("/app/bin/app")
        );
        // Native binary: no Dart SDK in the runtime image.
        assert!(analysis.packages.is_empty());
    }

    #[test]
    fn a_single_bin_file_is_unambiguous() {
        let (_dir, app) = write_app(&[
            ("pubspec.yaml", "name: other\n"),
            ("bin/entry.dart", "void main() {}"),
        ]);
        assert_eq!(plan_for(&app).metadata["entrypoint"], "bin/entry.dart");
    }

    #[test]
    fn no_entrypoint_is_an_actionable_error() {
        let (_dir, app) = write_app(&[("pubspec.yaml", "name: lib\n"), ("lib/lib.dart", "")]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("AUTOPACK_BUILD_CMD"), "{err}");
    }
}
