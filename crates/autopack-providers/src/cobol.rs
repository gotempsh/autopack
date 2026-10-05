//! COBOL provider, via GnuCOBOL.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::{
    install_recorded_runtime_libraries, procfile_web_command, record_runtime_libraries,
    ELF_INSPECTION_PACKAGE, RUNTIME_DEPS_FILE,
};

/// Where the compiled program is placed.
const OUTPUT_BINARY: &str = "/app/bin/app";

/// Builds COBOL programs with GnuCOBOL.
pub struct CobolProvider;

impl Provider for CobolProvider {
    fn id(&self) -> &'static str {
        "cobol"
    }

    fn display_name(&self) -> &'static str {
        "COBOL"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_match("**/*.cbl") || app.has_match("**/*.cob") || app.has_match("**/*.CBL"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let source = main_source(ctx.app)?;
        ctx.add_metadata("source", &source);

        // GnuCOBOL comes from apt: there is no upstream toolchain tarball and
        // no mise plugin worth depending on.
        ctx.build_apt_packages.push("gnucobol".to_string());
        ctx.build_apt_packages
            .push(ELF_INSPECTION_PACKAGE.to_string());

        // `-free` selects free-format source. Fixed-format COBOL (columns 7-72)
        // is still common, and the two are not interchangeable, so it is
        // detected rather than assumed.
        let format_flag = if is_free_format(&ctx.app.read_file(&source)?) {
            "-free"
        } else {
            "-fixed"
        };
        ctx.add_metadata("sourceFormat", format_flag.trim_start_matches('-'));

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];
        step.add_command(Command::shell(format!(
            "mkdir -p /app/bin && cobc -x {format_flag} -O2 -o {OUTPUT_BINARY} {source}"
        )));
        step.add_command(Command::shell(record_runtime_libraries(
            OUTPUT_BINARY,
            RUNTIME_DEPS_FILE,
        )));

        // The binary links libcob, whose package name carries a soname that
        // moves between Debian releases, so it is resolved from the binary.
        ctx.set_runtime_includes_runtimes(false);
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

/// The program to compile.
fn main_source(app: &App) -> Result<String> {
    for candidate in ["main.cbl", "main.cob", "src/main.cbl", "src/main.cob"] {
        if app.has_file(candidate) {
            return Ok(candidate.to_string());
        }
    }

    let mut sources = app.find_files("**/*.cbl")?;
    sources.extend(app.find_files("**/*.cob")?);
    match sources.len() {
        1 => Ok(sources.remove(0)),
        0 => Err(Error::provider("cobol", "no .cbl or .cob source found")),
        _ => Err(Error::provider(
            "cobol",
            format!(
                "several COBOL sources found ({}). Name one `main.cbl`, or set \
                 `AUTOPACK_BUILD_CMD` and `AUTOPACK_START_CMD`",
                sources.join(", ")
            ),
        )),
    }
}

/// Whether the source is free-format rather than the traditional fixed layout.
///
/// Fixed-format reserves columns 1-6 for sequence numbers and column 7 for
/// indicators, so `IDENTIFICATION DIVISION` starts at column 8. Free-format
/// puts it at the margin.
fn is_free_format(source: &str) -> bool {
    source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(20)
        .any(|line| {
            let upper = line.to_ascii_uppercase();
            !line.starts_with(' ') && (upper.contains("DIVISION") || upper.contains("SECTION"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{plan_for, write_app};

    const FREE: &str = "IDENTIFICATION DIVISION.\nPROGRAM-ID. HELLO.\n";
    const FIXED: &str = "       IDENTIFICATION DIVISION.\n       PROGRAM-ID. HELLO.\n";

    #[test]
    fn compiles_a_free_format_program() {
        let (_dir, app) = write_app(&[("main.cbl", FREE)]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "cobol");
        assert_eq!(analysis.metadata["sourceFormat"], "free");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("cobc -x -free"));
    }

    #[test]
    fn fixed_format_is_not_compiled_as_free() {
        // Compiling fixed-format source with -free produces a wall of syntax
        // errors about column positions.
        assert!(!is_free_format(FIXED));
        assert!(is_free_format(FREE));
    }

    #[test]
    fn gnucobol_is_installed_at_build_time() {
        let (_dir, app) = write_app(&[("main.cbl", FREE)]);
        let analysis = plan_for(&app);
        assert!(analysis.plan.step("packages").unwrap().commands[0]
            .display_name()
            .contains("gnucobol"));
    }
}
