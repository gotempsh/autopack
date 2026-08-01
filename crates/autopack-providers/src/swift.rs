//! Swift provider.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::procfile_web_command;

/// Swift toolchain used when the project does not pin one.
const DEFAULT_SWIFT_VERSION: &str = "6.0";

/// Where the compiled executable is placed.
const OUTPUT_BINARY: &str = "/app/bin/app";

/// Builds Swift packages with SwiftPM.
pub struct SwiftProvider;

impl Provider for SwiftProvider {
    fn id(&self) -> &'static str {
        "swift"
    }

    fn display_name(&self) -> &'static str {
        "Swift"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("Package.swift"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let manifest = ctx.app.read_file("Package.swift")?;
        let executable = executable_name(&manifest)?;
        let version = ctx
            .env
            .config("SWIFT_VERSION")
            .unwrap_or(DEFAULT_SWIFT_VERSION)
            .to_string();

        // The `-slim` tag carries the Swift runtime libraries without the
        // compiler, which is exactly the split a compiled binary needs.
        ctx.set_base_image(format!("swift:{version}"));
        ctx.set_runtime_base_image(format!("swift:{version}-slim"));
        ctx.set_runtime_includes_runtimes(false);

        ctx.add_metadata("swiftVersion", &version);
        ctx.add_metadata("executable", &executable);

        // SwiftPM resolves into .build, which is a cache mount, so the binary
        // has to be copied somewhere real in the same command.
        let build_cache = ctx.locked_cache("swift-build", "/app/.build");

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];
        step.add_cache(build_cache);
        step.add_command(Command::shell(format!(
            "swift build -c release --product {executable} && \
             mkdir -p /app/bin && cp \"$(swift build -c release --product {executable} \
             --show-bin-path)/{executable}\" {OUTPUT_BINARY}"
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

/// The executable product to build.
///
/// `Package.swift` is Swift source, not data, so this reads the declarations
/// rather than truly parsing it. An `.executableTarget` is the strongest
/// signal; a `.executable` product is next; the package name is the fallback.
fn executable_name(manifest: &str) -> Result<String> {
    for marker in [".executableTarget(", ".executable("] {
        if let Some(name) = declaration_name(manifest, marker) {
            return Ok(name);
        }
    }

    if let Some(name) = declaration_name(manifest, "Package(") {
        return Ok(name);
    }

    Err(Error::provider(
        "swift",
        "could not find an executable product or target in Package.swift.\n\
         Set `AUTOPACK_START_CMD` if the binary is produced another way",
    ))
}

/// The `name: "..."` argument following `marker`.
fn declaration_name(manifest: &str, marker: &str) -> Option<String> {
    let start = manifest.find(marker)? + marker.len();
    let rest = &manifest[start..];
    let name_at = rest.find("name:")? + "name:".len();
    let quoted = rest[name_at..].trim_start();
    let quoted = quoted.strip_prefix('"')?;
    let end = quoted.find('"')?;
    let name = &quoted[..end];
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{plan_for, try_plan_for, write_app};

    const PACKAGE_SWIFT: &str = r#"// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "hello",
    dependencies: [],
    targets: [
        .executableTarget(name: "Server", path: "Sources/Server")
    ]
)
"#;

    #[test]
    fn builds_the_executable_target() {
        let (_dir, app) = write_app(&[
            ("Package.swift", PACKAGE_SWIFT),
            ("Sources/Server/main.swift", "print(\"hi\")"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "swift");
        assert_eq!(analysis.metadata["executable"], "Server");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("swift build -c release --product Server"));
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some(OUTPUT_BINARY)
        );
    }

    #[test]
    fn the_runtime_image_has_no_compiler() {
        let (_dir, app) = write_app(&[
            ("Package.swift", PACKAGE_SWIFT),
            ("Sources/Server/main.swift", ""),
        ]);
        let analysis = plan_for(&app);
        assert!(analysis.packages.is_empty());
        assert!(analysis.plan.deploy.paths.is_empty());
    }

    #[test]
    fn falls_back_to_the_package_name() {
        let manifest = "let package = Package(name: \"solo\", targets: [.target(name: \"lib\")])";
        assert_eq!(executable_name(manifest).unwrap(), "solo");
    }

    #[test]
    fn a_manifest_without_names_is_an_actionable_error() {
        let (_dir, app) = write_app(&[("Package.swift", "// nothing useful here")]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("AUTOPACK_START_CMD"), "{err}");
    }
}
