//! Clojure provider: Leiningen and tools.deps.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::procfile_web_command;

/// JDK the official Clojure images are tagged against.
const DEFAULT_JDK: &str = "temurin-21";

/// Where the runnable jar is placed.
const OUTPUT_JAR: &str = "/app/bin/app.jar";

/// Builds Clojure applications.
pub struct ClojureProvider;

/// Which build tool the project uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildTool {
    /// Leiningen, driven by `project.clj`.
    Leiningen,
    /// tools.deps with a `build.clj`, driven by `deps.edn`.
    ToolsDeps,
}

impl BuildTool {
    /// The official image variant that carries this tool.
    fn image(self, jdk: &str) -> String {
        match self {
            Self::Leiningen => format!("clojure:{jdk}-lein"),
            Self::ToolsDeps => format!("clojure:{jdk}-tools-deps"),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Leiningen => "leiningen",
            Self::ToolsDeps => "tools.deps",
        }
    }
}

impl Provider for ClojureProvider {
    fn id(&self) -> &'static str {
        "clojure"
    }

    fn display_name(&self) -> &'static str {
        "Clojure"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_any_file(["project.clj", "deps.edn"]))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        // project.clj wins: a repo with both is a Leiningen project that also
        // exposes deps.edn for tooling.
        let tool = if ctx.app.has_file("project.clj") {
            BuildTool::Leiningen
        } else {
            BuildTool::ToolsDeps
        };

        if tool == BuildTool::ToolsDeps && !ctx.app.has_file("build.clj") {
            return Err(Error::provider(
                "clojure",
                "deps.edn projects need a `build.clj` with an `uber` task to \
                 produce a runnable jar (tools.deps has no built-in packaging).\n\
                 Add one, or set `AUTOPACK_BUILD_CMD` and `AUTOPACK_START_CMD`",
            ));
        }

        let jdk = ctx.env.config("CLOJURE_JDK").unwrap_or(DEFAULT_JDK);
        let image = tool.image(jdk);
        ctx.set_base_image(&image);
        // The output is a plain jar, so the runtime only needs a JRE.
        ctx.set_runtime_base_image("eclipse-temurin:21-jre");
        ctx.set_runtime_includes_runtimes(false);
        ctx.add_metadata("buildTool", tool.id());
        ctx.add_metadata("image", &image);

        let maven = ctx.shared_cache("maven", "/root/.m2");
        let gitlibs = ctx.shared_cache("gitlibs", "/root/.gitlibs");

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];
        step.add_cache(maven);
        step.add_cache(gitlibs);

        match tool {
            BuildTool::Leiningen => {
                step.add_command(Command::shell("lein uberjar"));
                // Leiningen writes both a thin and a `-standalone` jar; only
                // the standalone one carries its dependencies.
                step.add_command(Command::shell(format!(
                    "mkdir -p /app/bin && cp \"$(find target -name '*standalone*.jar' -print -quit)\" {OUTPUT_JAR}"
                )));
            }
            BuildTool::ToolsDeps => {
                step.add_command(Command::shell("clojure -T:build uber"));
                step.add_command(Command::shell(format!(
                    "mkdir -p /app/bin && cp \"$(find target -name '*.jar' -print -quit)\" {OUTPUT_JAR}"
                )));
            }
        }

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([OUTPUT_JAR]));

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None => format!("java -jar {OUTPUT_JAR}"),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, try_plan_for, write_app};

    #[test]
    fn leiningen_projects_build_an_uberjar() {
        let (_dir, app) = write_app(&[
            (
                "project.clj",
                "(defproject demo \"1.0.0\" :main demo.core :aot :all)",
            ),
            ("src/demo/core.clj", "(ns demo.core)"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "clojure");
        assert_eq!(analysis.metadata["buildTool"], "leiningen");
        assert_eq!(analysis.metadata["image"], "clojure:temurin-21-lein");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("java -jar /app/bin/app.jar")
        );
    }

    #[test]
    fn tools_deps_projects_use_the_build_task() {
        let (_dir, app) = write_app(&[
            ("deps.edn", "{:paths [\"src\"]}"),
            ("build.clj", "(ns build)"),
            ("src/core.clj", ""),
        ]);
        let analysis = plan_for(&app);
        assert_eq!(analysis.metadata["buildTool"], "tools.deps");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("clojure -T:build uber"));
    }

    #[test]
    fn deps_edn_without_a_build_task_explains_why() {
        let (_dir, app) = write_app(&[("deps.edn", "{:paths [\"src\"]}"), ("src/core.clj", "")]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("build.clj"), "{err}");
    }

    #[test]
    fn project_clj_wins_over_deps_edn() {
        let (_dir, app) = write_app(&[
            ("project.clj", "(defproject demo \"1.0.0\")"),
            ("deps.edn", "{}"),
            ("src/demo/core.clj", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["buildTool"], "leiningen");
    }
}
