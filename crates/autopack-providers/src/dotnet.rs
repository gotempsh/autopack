//! .NET provider.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::procfile_web_command;

/// .NET version used when no project file declares a target framework.
const DEFAULT_DOTNET_VERSION: &str = "8.0";

/// Where `dotnet publish` writes the application.
const PUBLISH_DIR: &str = "/app/out";

/// Builds .NET applications.
///
/// The SDK image is ~800MB and the ASP.NET runtime image is ~220MB, so the
/// build and runtime images are deliberately different.
pub struct DotnetProvider;

impl Provider for DotnetProvider {
    fn id(&self) -> &'static str {
        "dotnet"
    }

    fn display_name(&self) -> &'static str {
        ".NET"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(!app.find_files("**/*.csproj")?.is_empty()
            || !app.find_files("**/*.fsproj")?.is_empty()
            || !app.find_files("*.sln")?.is_empty())
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let project = main_project(ctx.app)?;
        let contents = ctx.app.read_file(&project)?;

        let (version, source) = target_framework(&contents);
        ctx.set_base_image(format!("mcr.microsoft.com/dotnet/sdk:{version}"));
        ctx.set_runtime_base_image(format!("mcr.microsoft.com/dotnet/aspnet:{version}"));

        let assembly = assembly_name(&contents, &project);
        ctx.add_metadata("dotnetVersion", &version);
        ctx.add_metadata("dotnetVersionSource", source);
        ctx.add_metadata("project", &project);
        ctx.add_metadata("assembly", &assembly);

        let cache = ctx.shared_cache("nuget", "/cache/nuget");

        // Restoring from the project files alone means a source edit does not
        // re-download the whole package graph.
        let mut manifests = ctx.app.find_files("**/*.csproj")?;
        manifests.extend(ctx.app.find_files("**/*.fsproj")?);
        manifests.extend(ctx.app.find_files("*.sln")?);
        manifests.extend(ctx.app.find_files("**/Directory.Build.props")?);
        manifests.extend(ctx.app.find_files("**/nuget.config")?);

        let install = ctx.step(steps::INSTALL);
        install.add_input(Layer::local().including(manifests));
        install.add_variable("NUGET_PACKAGES", "/cache/nuget");
        install.add_variable("DOTNET_CLI_TELEMETRY_OPTOUT", "1");
        install.add_variable("DOTNET_NOLOGO", "1");
        install.add_cache(cache.clone());
        install.add_command(Command::shell(format!("dotnet restore {project}")));

        let build = ctx.step(steps::BUILD);
        build.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
        build.add_variable("NUGET_PACKAGES", "/cache/nuget");
        build.add_variable("DOTNET_CLI_TELEMETRY_OPTOUT", "1");
        build.add_variable("DOTNET_NOLOGO", "1");
        build.add_cache(cache);
        build.add_command(Command::shell(format!(
            "dotnet publish {project} -c Release --no-restore -o {PUBLISH_DIR}"
        )));

        ctx.set_runtime_includes_runtimes(false);
        ctx.add_deploy_input(Layer::step(steps::BUILD).including([PUBLISH_DIR]));
        ctx.add_deploy_variable("DOTNET_RUNNING_IN_CONTAINER", "true");

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            // ASP.NET binds to port 8080 by default in containers; the URL is
            // set on the command line so `$PORT` still expands.
            None => {
                format!("dotnet {PUBLISH_DIR}/{assembly}.dll --urls http://0.0.0.0:${{PORT:-8080}}")
            }
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

/// The project file to publish.
fn main_project(app: &App) -> Result<String> {
    let mut projects = app.find_files("**/*.csproj")?;
    projects.extend(app.find_files("**/*.fsproj")?);

    match projects.len() {
        0 => Err(Error::provider(
            "dotnet",
            "found a solution but no .csproj or .fsproj to publish",
        )),
        1 => Ok(projects.remove(0)),
        _ => {
            // A solution usually has one web project and several libraries.
            // Guessing wrong publishes a library and produces an image that
            // starts and immediately exits.
            let web: Vec<String> = projects
                .iter()
                .filter(|path| {
                    app.read_file(path)
                        .map(|contents| contents.contains("Microsoft.NET.Sdk.Web"))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            match web.len() {
                1 => Ok(web[0].clone()),
                _ => Err(Error::provider(
                    "dotnet",
                    format!(
                        "several projects found ({}). Choose one with \
                         `AUTOPACK_BUILD_CMD='dotnet publish <project> -c Release -o /app/out'` \
                         and a matching `AUTOPACK_START_CMD`",
                        projects.join(", ")
                    ),
                )),
            }
        }
    }
}

/// `<TargetFramework>net8.0</TargetFramework>` -> `8.0`.
fn target_framework(project: &str) -> (String, &'static str) {
    for tag in ["TargetFramework", "TargetFrameworks"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let Some(start) = project.find(&open) else {
            continue;
        };
        let start = start + open.len();
        let Some(end) = project[start..].find(&close) else {
            continue;
        };
        let value = project[start..start + end].trim();
        // `net8.0;net9.0` — build against the first, which is what `dotnet
        // publish` picks without an explicit framework.
        let first = value.split(';').next().unwrap_or(value).trim();
        if let Some(version) = first.strip_prefix("net") {
            if version.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return (version.to_string(), "TargetFramework");
            }
        }
    }
    (DEFAULT_DOTNET_VERSION.to_string(), "autopack default")
}

/// The name of the produced assembly, which is the DLL to run.
fn assembly_name(project_contents: &str, project_path: &str) -> String {
    const OPEN: &str = "<AssemblyName>";
    const CLOSE: &str = "</AssemblyName>";
    if let Some(start) = project_contents.find(OPEN) {
        let start = start + OPEN.len();
        if let Some(end) = project_contents[start..].find(CLOSE) {
            let name = project_contents[start..start + end].trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }

    // Defaults to the project file's stem.
    project_path
        .rsplit('/')
        .next()
        .unwrap_or(project_path)
        .trim_end_matches(".csproj")
        .trim_end_matches(".fsproj")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{try_plan_for, write_app};

    const WEB_PROJECT: &str = r#"<Project Sdk="Microsoft.NET.Sdk.Web">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
  </PropertyGroup>
</Project>"#;

    #[test]
    fn publishes_the_only_project() {
        let (_dir, app) = write_app(&[("Api.csproj", WEB_PROJECT), ("Program.cs", "")]);
        let analysis = try_plan_for(&app).unwrap();

        assert_eq!(analysis.provider, "dotnet");
        assert_eq!(analysis.metadata["dotnetVersion"], "8.0");
        assert_eq!(analysis.metadata["assembly"], "Api");
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("dotnet /app/out/Api.dll --urls http://0.0.0.0:${PORT:-8080}")
        );
    }

    #[test]
    fn the_web_project_wins_in_a_solution() {
        let (_dir, app) = write_app(&[
            ("App.sln", ""),
            ("src/Api/Api.csproj", WEB_PROJECT),
            (
                "src/Domain/Domain.csproj",
                r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"#,
            ),
        ]);
        let analysis = try_plan_for(&app).unwrap();
        assert_eq!(analysis.metadata["project"], "src/Api/Api.csproj");
    }

    #[test]
    fn ambiguous_solutions_ask_the_user_to_choose() {
        let library = r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"#;
        let (_dir, app) = write_app(&[
            ("App.sln", ""),
            ("src/A/A.csproj", library),
            ("src/B/B.csproj", library),
        ]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("AUTOPACK_BUILD_CMD"), "{err}");
    }

    #[test]
    fn multi_targeting_uses_the_first_framework() {
        let (version, _) = target_framework("<TargetFrameworks>net8.0;net9.0</TargetFrameworks>");
        assert_eq!(version, "8.0");
    }
}
