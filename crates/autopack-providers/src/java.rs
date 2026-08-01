//! Java provider: Maven and Gradle projects.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result};

use crate::support::{procfile_web_command, read_version_file};

/// JDK major version used when the project does not declare one.
const DEFAULT_JAVA_VERSION: &str = "21";

/// Where the runnable jar is placed.
const OUTPUT_JAR: &str = "/app/bin/app.jar";

/// Builds JVM applications.
pub struct JavaProvider;

/// Which build tool drives the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildTool {
    Maven,
    MavenWrapper,
    Gradle,
    GradleWrapper,
}

impl BuildTool {
    fn detect(app: &App) -> Option<Self> {
        if app.has_file("mvnw") {
            Some(Self::MavenWrapper)
        } else if app.has_file("pom.xml") {
            Some(Self::Maven)
        } else if app.has_file("gradlew") {
            Some(Self::GradleWrapper)
        } else if app.has_any_file(["build.gradle", "build.gradle.kts"]) {
            Some(Self::Gradle)
        } else {
            None
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Maven | Self::MavenWrapper => "maven",
            Self::Gradle | Self::GradleWrapper => "gradle",
        }
    }

    /// The mise tool needed, or `None` when the project ships a wrapper.
    fn mise_tool(self) -> Option<&'static str> {
        match self {
            Self::Maven => Some("maven"),
            Self::Gradle => Some("gradle"),
            Self::MavenWrapper | Self::GradleWrapper => None,
        }
    }

    fn build_command(self) -> &'static str {
        match self {
            // Tests belong in CI, not in an image build: they need services
            // the builder does not have and double the build time.
            Self::Maven => "mvn -B -DskipTests package",
            Self::MavenWrapper => "./mvnw -B -DskipTests package",
            // The Gradle daemon outlives the RUN step and wastes memory.
            Self::Gradle => "gradle --no-daemon -x test build",
            Self::GradleWrapper => "./gradlew --no-daemon -x test build",
        }
    }

    /// Copy the application jar to a fixed path.
    ///
    /// The jar's name comes from the project's artifact id and version, which
    /// autopack does not want to parse; finding it at build time is both
    /// simpler and correct. Sources, javadoc and Gradle's `-plain` jars are
    /// excluded because they are not runnable.
    fn collect_jar_command(self) -> &'static str {
        match self {
            Self::Maven | Self::MavenWrapper => {
                "mkdir -p /app/bin && jar=$(find target -maxdepth 1 -name \"*.jar\" \
                 ! -name \"*-sources.jar\" ! -name \"*-javadoc.jar\" -print -quit) && \
                 test -n \"$jar\" && cp \"$jar\" /app/bin/app.jar"
            }
            Self::Gradle | Self::GradleWrapper => {
                "mkdir -p /app/bin && jar=$(find build/libs -maxdepth 1 -name \"*.jar\" \
                 ! -name \"*-plain.jar\" ! -name \"*-sources.jar\" -print -quit) && \
                 test -n \"$jar\" && cp \"$jar\" /app/bin/app.jar"
            }
        }
    }

    fn cache(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Maven | Self::MavenWrapper => ("maven", "/cache/maven", "MAVEN_OPTS"),
            Self::Gradle | Self::GradleWrapper => ("gradle", "/cache/gradle", "GRADLE_USER_HOME"),
        }
    }
}

impl Provider for JavaProvider {
    fn id(&self) -> &'static str {
        "java"
    }

    fn display_name(&self) -> &'static str {
        "Java"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(BuildTool::detect(app).is_some())
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let tool = BuildTool::detect(ctx.app)
            .ok_or_else(|| autopack_core::Error::provider("java", "no Maven or Gradle build"))?;

        let (version, source) = java_version(ctx.app)?;
        ctx.packages
            .add("java", format!("temurin-{version}"), source);

        // Run on a JRE image rather than carrying the build toolchain: a JDK
        // plus Maven in the runtime image is ~200MB of things a running jar
        // never calls.
        ctx.set_runtime_base_image(format!("eclipse-temurin:{version}-jre"));
        ctx.set_runtime_includes_runtimes(false);
        if let Some(mise_tool) = tool.mise_tool() {
            ctx.packages
                .add(mise_tool, "latest", "no wrapper in the repo");
        }
        ctx.add_metadata("javaVersion", &version);
        ctx.add_metadata("buildTool", tool.id());

        let (cache_name, cache_dir, _) = tool.cache();
        let cache = ctx.shared_cache(cache_name, cache_dir);

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];
        step.add_cache(cache);
        match tool {
            BuildTool::Maven | BuildTool::MavenWrapper => {
                step.add_variable("MAVEN_OPTS", "-Dmaven.repo.local=/cache/maven");
            }
            BuildTool::Gradle | BuildTool::GradleWrapper => {
                step.add_variable("GRADLE_USER_HOME", "/cache/gradle");
            }
        }
        step.add_command(Command::shell(tool.build_command()));
        step.add_command(Command::shell(tool.collect_jar_command()));

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([OUTPUT_JAR]));

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None => format!("java -jar {OUTPUT_JAR}"),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

/// The JDK major version the project targets.
fn java_version(app: &App) -> Result<(String, String)> {
    if let Some(version) = read_version_file(app, ".java-version")? {
        if let Some(major) = major_version(&version) {
            return Ok((major, ".java-version".into()));
        }
    }

    if let Some(pom) = app.read_file_opt("pom.xml")? {
        for tag in [
            "maven.compiler.release",
            "java.version",
            "maven.compiler.target",
        ] {
            if let Some(value) = xml_element(&pom, tag) {
                if let Some(major) = major_version(value) {
                    return Ok((major, format!("pom.xml <{tag}>")));
                }
            }
        }
    }

    for file in ["build.gradle", "build.gradle.kts"] {
        let Some(gradle) = app.read_file_opt(file)? else {
            continue;
        };
        for line in gradle.lines() {
            let line = line.trim();
            // `languageVersion = JavaLanguageVersion.of(21)` and
            // `sourceCompatibility = '17'` are both common.
            if let Some(rest) = line.split("JavaLanguageVersion.of(").nth(1) {
                let digits: String = rest
                    .trim_start()
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Some(major) = major_version(&digits) {
                    return Ok((major, format!("{file} toolchain")));
                }
            }
            if line.starts_with("sourceCompatibility") {
                if let Some((_, value)) = line.split_once('=') {
                    if let Some(major) = major_version(value.trim().trim_matches(['"', '\''])) {
                        return Ok((major, format!("{file} sourceCompatibility")));
                    }
                }
            }
        }
    }

    Ok((DEFAULT_JAVA_VERSION.to_string(), "autopack default".into()))
}

/// `1.8` -> `8`, `21` -> `21`, `net21` -> `None`.
fn major_version(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches("JAVA_").trim();
    let mut parts = value.split('.');
    let first = parts.next()?.trim();
    if !first.chars().all(|c| c.is_ascii_digit()) || first.is_empty() {
        return None;
    }
    // Legacy `1.8` notation means Java 8.
    if first == "1" {
        return parts.next().map(|minor| minor.trim().to_string());
    }
    Some(first.to_string())
}

/// The text content of the first `<tag>` element in `xml`.
fn xml_element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{plan_for, write_app};

    #[test]
    fn maven_wrapper_projects_build_and_run_a_jar() {
        let (_dir, app) = write_app(&[
            (
                "pom.xml",
                "<project><properties><java.version>21</java.version></properties></project>",
            ),
            ("mvnw", "#!/bin/sh"),
            ("src/main/java/App.java", "class App {}"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "java");
        assert_eq!(analysis.metadata["buildTool"], "maven");
        assert_eq!(analysis.metadata["javaVersion"], "21");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .starts_with("./mvnw"));
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("java -jar /app/bin/app.jar")
        );
    }

    #[test]
    fn gradle_without_a_wrapper_installs_gradle() {
        let (_dir, app) = write_app(&[
            (
                "build.gradle",
                "java { toolchain { languageVersion = JavaLanguageVersion.of(17) } }",
            ),
            ("src/main/java/App.java", ""),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.metadata["buildTool"], "gradle");
        assert_eq!(analysis.metadata["javaVersion"], "17");
        assert!(analysis.packages.iter().any(|(name, _)| name == "gradle"));
    }

    #[test]
    fn wrappers_are_preferred_over_installing_the_tool() {
        let (_dir, app) = write_app(&[
            ("build.gradle", ""),
            ("gradlew", "#!/bin/sh"),
            ("src/main/java/App.java", ""),
        ]);
        let analysis = plan_for(&app);
        assert!(!analysis.packages.iter().any(|(name, _)| name == "gradle"));
    }

    #[test]
    fn legacy_source_compatibility_maps_to_a_major_version() {
        assert_eq!(major_version("1.8").as_deref(), Some("8"));
        assert_eq!(major_version("21").as_deref(), Some("21"));
        assert_eq!(major_version("net8.0"), None);
    }
}
