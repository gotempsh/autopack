//! Scala provider, via sbt.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::procfile_web_command;

/// sbt image used when the project does not pin one.
///
/// The `sbtscala/scala-sbt` tags encode the JDK, sbt and Scala versions
/// together, so there is no floating "latest" to track.
const DEFAULT_SBT_IMAGE: &str = "sbtscala/scala-sbt:eclipse-temurin-21.0.5_11_1.10.7_3.6.2";

/// Where the runnable artefact is placed.
const OUTPUT_JAR: &str = "/app/bin/app.jar";

/// Builds Scala applications with sbt.
pub struct ScalaProvider;

/// How the project produces something runnable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Packaging {
    /// sbt-assembly, producing a fat jar.
    Assembly,
    /// sbt-native-packager, producing a start script under `target/universal`.
    NativePackager,
}

impl Provider for ScalaProvider {
    fn id(&self) -> &'static str {
        "scala"
    }

    fn display_name(&self) -> &'static str {
        "Scala"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("build.sbt") || app.has_file("project/build.properties"))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let packaging = packaging(ctx.app)?;
        let image = ctx
            .env
            .config("SBT_IMAGE")
            .unwrap_or(DEFAULT_SBT_IMAGE)
            .to_string();

        ctx.set_base_image(&image);
        // A jar only needs a JRE, and the sbt image is well over a gigabyte.
        ctx.set_runtime_base_image("eclipse-temurin:21-jre");
        ctx.set_runtime_includes_runtimes(false);
        ctx.add_metadata("image", &image);
        ctx.add_metadata(
            "packaging",
            match packaging {
                Packaging::Assembly => "sbt-assembly",
                Packaging::NativePackager => "sbt-native-packager",
            },
        );

        // Ivy and Coursier both cache under the home directory, and a Scala
        // dependency graph is large enough that re-resolving dominates a rebuild.
        let ivy = ctx.shared_cache("ivy", "/root/.ivy2");
        let coursier = ctx.shared_cache("coursier", "/root/.cache/coursier");
        let sbt_cache = ctx.shared_cache("sbt", "/root/.sbt");

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];
        step.add_cache(ivy);
        step.add_cache(coursier);
        step.add_cache(sbt_cache);

        match packaging {
            Packaging::Assembly => {
                step.add_command(Command::shell("sbt -batch assembly"));
                // The jar's name carries the project and Scala versions, which
                // is why it is located rather than constructed.
                step.add_command(Command::shell(format!(
                    "mkdir -p /app/bin && cp \"$(find target -name '*assembly*.jar' -print -quit)\" {OUTPUT_JAR}"
                )));
            }
            Packaging::NativePackager => {
                step.add_command(Command::shell("sbt -batch stage"));
                step.add_command(Command::shell(
                    "mkdir -p /app/bin && cp -r target/universal/stage /app/stage",
                ));
            }
        }

        let start = match (procfile_web_command(ctx.app)?, packaging) {
            (Some(command), _) => command,
            (None, Packaging::Assembly) => {
                ctx.add_deploy_input(Layer::step(steps::BUILD).including([OUTPUT_JAR]));
                format!("java -jar {OUTPUT_JAR}")
            }
            (None, Packaging::NativePackager) => {
                ctx.add_deploy_input(Layer::step(steps::BUILD).including(["/app/stage"]));
                "/app/stage/bin/$(ls /app/stage/bin | head -1)".to_string()
            }
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

/// Which packaging plugin the project uses.
///
/// Plain `sbt package` produces a thin jar with no dependencies on the
/// classpath, which cannot be run standalone — so a packaging plugin is
/// required rather than optional.
fn packaging(app: &App) -> Result<Packaging> {
    let mut plugins = String::new();
    for file in ["project/plugins.sbt", "project/assembly.sbt", "build.sbt"] {
        if let Some(contents) = app.read_file_opt(file)? {
            plugins.push_str(&contents);
            plugins.push('\n');
        }
    }

    if plugins.contains("sbt-assembly") {
        return Ok(Packaging::Assembly);
    }
    if plugins.contains("sbt-native-packager") {
        return Ok(Packaging::NativePackager);
    }

    Err(Error::provider(
        "scala",
        "no packaging plugin found in project/plugins.sbt.\n\
         `sbt package` alone produces a jar without its dependencies, which \
         cannot start on its own. Add sbt-assembly:\n\
         \n    addSbtPlugin(\"com.eed3si9n\" % \"sbt-assembly\" % \"2.2.0\")\n\n\
         or sbt-native-packager, or set `AUTOPACK_BUILD_CMD` and \
         `AUTOPACK_START_CMD`",
    ))
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, try_plan_for, write_app};

    #[test]
    fn assembly_projects_build_a_fat_jar() {
        let (_dir, app) = write_app(&[
            ("build.sbt", "name := \"demo\"\nscalaVersion := \"3.6.2\"\n"),
            (
                "project/plugins.sbt",
                "addSbtPlugin(\"com.eed3si9n\" % \"sbt-assembly\" % \"2.2.0\")",
            ),
            (
                "src/main/scala/Main.scala",
                "@main def run() = println(\"hi\")",
            ),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "scala");
        assert_eq!(analysis.metadata["packaging"], "sbt-assembly");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("java -jar /app/bin/app.jar")
        );
    }

    #[test]
    fn native_packager_projects_stage_a_start_script() {
        let (_dir, app) = write_app(&[
            ("build.sbt", "enablePlugins(JavaAppPackaging)"),
            (
                "project/plugins.sbt",
                "addSbtPlugin(\"com.github.sbt\" % \"sbt-native-packager\" % \"1.10.4\")",
            ),
            ("src/main/scala/Main.scala", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["packaging"], "sbt-native-packager");
    }

    #[test]
    fn a_project_without_a_packaging_plugin_says_what_to_add() {
        let (_dir, app) = write_app(&[
            ("build.sbt", "name := \"demo\""),
            ("src/main/scala/Main.scala", ""),
        ]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("sbt-assembly"), "{err}");
    }
}
