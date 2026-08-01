//! C and C++ provider.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::{foreign_manifest, procfile_web_command};

/// Where the compiled binary is placed.
const OUTPUT_BINARY: &str = "/app/bin/app";

/// Builds C and C++ applications with CMake or Make.
pub struct CppProvider;

/// How the project is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildSystem {
    CMake,
    Make,
}

impl Provider for CppProvider {
    fn id(&self) -> &'static str {
        "cpp"
    }

    fn display_name(&self) -> &'static str {
        "C/C++"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        if app.has_file("CMakeLists.txt") {
            return Ok(true);
        }
        // A `Makefile` alone means very little — Go, Rust and Python projects
        // ship them too — so it only counts when nothing else claims the repo
        // and there are actually C or C++ sources.
        Ok(app.has_file("Makefile")
            && foreign_manifest(app, &[]).is_none()
            && (app.has_match("**/*.c") || app.has_match("**/*.cpp") || app.has_match("**/*.cc")))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let system = if ctx.app.has_file("CMakeLists.txt") {
            BuildSystem::CMake
        } else {
            BuildSystem::Make
        };
        ctx.add_metadata(
            "buildSystem",
            if system == BuildSystem::CMake {
                "cmake"
            } else {
                "make"
            },
        );

        // The compiler comes from apt rather than mise: distribution
        // toolchains are what the system libraries were built against.
        ctx.build_apt_packages.extend(
            ["build-essential", "pkg-config"]
                .into_iter()
                .map(String::from),
        );
        if system == BuildSystem::CMake {
            ctx.build_apt_packages.push("cmake".to_string());
        }
        // debian-slim has no C++ standard library; a C++ binary needs it.
        ctx.deploy_apt_packages.push("libstdc++6".to_string());

        let command = match system {
            BuildSystem::CMake => {
                let target = cmake_target(ctx.app)?;
                ctx.add_metadata("target", &target);
                format!(
                    "cmake -S . -B build -DCMAKE_BUILD_TYPE=Release && \
                     cmake --build build --parallel \"$(nproc)\" && \
                     mkdir -p /app/bin && cp \"build/{target}\" {OUTPUT_BINARY}"
                )
            }
            BuildSystem::Make => {
                let target = ctx.env.config("CPP_BINARY").ok_or_else(|| {
                    Error::provider(
                        "cpp",
                        "a Makefile does not say which file it produces.\n\
                         Set `AUTOPACK_CPP_BINARY=<path relative to the repo root>`, or \
                         set `AUTOPACK_BUILD_CMD` and `AUTOPACK_START_CMD` outright",
                    )
                })?;
                ctx.add_metadata("target", target);
                format!(
                    "make -j \"$(nproc)\" && mkdir -p /app/bin && cp \"{target}\" {OUTPUT_BINARY}"
                )
            }
        };

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];
        step.add_command(Command::shell(command));

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

/// The executable target name from `project(<name> ...)` in `CMakeLists.txt`.
///
/// CMake writes executables into the build directory under the target name,
/// and `add_executable(<name> ...)` is the authoritative source; `project()`
/// is the convention when the two agree.
fn cmake_target(app: &App) -> Result<String> {
    if let Some(target) = app
        .read_file("CMakeLists.txt")?
        .split("add_executable(")
        .nth(1)
    {
        let name: String = target
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if !name.is_empty() && !name.starts_with("${") {
            return Ok(name);
        }
    }

    Err(Error::provider(
        "cpp",
        "no literal `add_executable(<name> ...)` found in CMakeLists.txt, so the \
         binary to run is unknown.\n\
         Set `AUTOPACK_BUILD_CMD` and `AUTOPACK_START_CMD`",
    ))
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, try_plan_for, write_app};

    #[test]
    fn cmake_projects_build_the_named_executable() {
        let (_dir, app) = write_app(&[
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.20)\nproject(server)\nadd_executable(server main.cpp)\n",
            ),
            ("main.cpp", "int main() { return 0; }"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "cpp");
        assert_eq!(analysis.metadata["target"], "server");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("cp \"build/server\" /app/bin/app"));
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("/app/bin/app")
        );
    }

    #[test]
    fn a_makefile_without_a_named_binary_asks_for_one() {
        let (_dir, app) = write_app(&[
            ("Makefile", "all:\n\tgcc -o server main.c\n"),
            ("main.c", ""),
        ]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("AUTOPACK_CPP_BINARY"), "{err}");
    }

    #[test]
    fn a_makefile_in_a_rust_repo_is_not_a_cpp_project() {
        let (_dir, app) = write_app(&[
            ("Cargo.toml", "[package]\nname = \"api\"\n"),
            ("Makefile", "all:\n\tcargo build\n"),
            ("src/main.rs", "fn main() {}"),
            ("vendor.c", ""),
        ]);
        assert_eq!(plan_for(&app).provider, "rust");
    }
}
