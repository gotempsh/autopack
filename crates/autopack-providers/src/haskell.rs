//! Haskell provider: Cabal and Stack projects.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Error, Provider, Result};

use crate::support::{foreign_manifest, procfile_web_command};

/// GHC version used when the project does not pin one.
const DEFAULT_GHC_VERSION: &str = "9.6";

/// Where the compiled executable is placed.
const OUTPUT_DIR: &str = "/app/bin";

/// Builds Haskell applications.
pub struct HaskellProvider;

/// Which build tool drives the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildTool {
    /// Stack, which manages its own package snapshot.
    Stack,
    /// cabal-install, driving the package's own `build-depends`.
    Cabal,
}

impl BuildTool {
    fn detect(app: &App) -> Self {
        if app.has_file("stack.yaml") {
            Self::Stack
        } else {
            Self::Cabal
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::Cabal => "cabal",
        }
    }
}

impl Provider for HaskellProvider {
    fn id(&self) -> &'static str {
        "haskell"
    }

    fn display_name(&self) -> &'static str {
        "Haskell"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        if app.has_any_file(["stack.yaml", "cabal.project"]) || app.has_match("*.cabal") {
            return Ok(true);
        }
        // `package.yaml` is hpack's manifest, but it is also a plausible file
        // name in other ecosystems, so it only counts on its own.
        Ok(foreign_manifest(app, &[]).is_none()
            && (app.has_file("package.yaml") || app.has_match("**/*.hs")))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let tool = BuildTool::detect(ctx.app);
        let executable = executable_name(ctx.app)?;
        let (version, source) = ghc_version(ctx.app)?;

        // GHC is ~2GB installed and mise has no Haskell plugin worth relying
        // on; the official image ships GHC, cabal and stack together.
        ctx.set_base_image(format!("haskell:{version}"));
        // A GHC-compiled binary is dynamically linked against libgmp and libc
        // only, so the runtime image needs neither GHC nor the package store.
        ctx.set_runtime_base_image("debian:bookworm-slim");
        ctx.set_runtime_includes_runtimes(false);
        ctx.deploy_apt_packages
            .extend(["libgmp10", "zlib1g"].into_iter().map(String::from));

        ctx.add_metadata("ghcVersion", &version);
        ctx.add_metadata("ghcVersionSource", source);
        ctx.add_metadata("buildTool", tool.id());
        ctx.add_metadata("executable", &executable);

        match tool {
            BuildTool::Cabal => self.plan_cabal(ctx, &executable)?,
            BuildTool::Stack => self.plan_stack(ctx)?,
        }

        ctx.add_deploy_input(Layer::step(steps::BUILD).including([OUTPUT_DIR]));

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None => format!("{OUTPUT_DIR}/{executable}"),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

impl HaskellProvider {
    fn plan_cabal(&self, ctx: &mut BuildContext<'_>, executable: &str) -> Result<()> {
        // Cached at cabal's default location rather than by moving CABAL_DIR,
        // for the same reason cargo's cache is not relocated: tool homes are
        // where compilers look for themselves.
        let store = ctx.shared_cache("cabal-store", "/root/.cabal");
        let dist = ctx.locked_cache("cabal-dist", "/app/dist-newstyle");

        let mut manifests = ctx.app.find_files("*.cabal")?;
        for extra in ["cabal.project", "cabal.project.freeze", "package.yaml"] {
            if ctx.app.has_file(extra) {
                manifests.push(extra.to_string());
            }
        }

        let install = ctx.step(steps::INSTALL);
        install.add_input(Layer::local().including(manifests));
        install.add_cache(store.clone());
        // The package index is ~100MB; fetching it in its own step keeps it
        // out of the cache key for every later source change.
        install.add_command(Command::shell("cabal update"));
        install.add_command(Command::shell("cabal build --only-dependencies"));

        let build = ctx.step(steps::BUILD);
        build.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
        build.add_cache(store);
        build.add_cache(dist);
        // `dist-newstyle` is a cache mount and does not survive the step, so
        // the executable is installed straight into its final location.
        build.add_command(Command::shell(format!(
            "cabal install exe:{executable} --installdir={OUTPUT_DIR} \
             --install-method=copy --overwrite-policy=always"
        )));
        Ok(())
    }

    fn plan_stack(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let root = ctx.shared_cache("stack-root", "/root/.stack");
        let work = ctx.locked_cache("stack-work", "/app/.stack-work");

        let mut manifests: Vec<String> = ["stack.yaml", "stack.yaml.lock", "package.yaml"]
            .into_iter()
            .filter(|file| ctx.app.has_file(file))
            .map(String::from)
            .collect();
        manifests.extend(ctx.app.find_files("*.cabal")?);

        let install = ctx.step(steps::INSTALL);
        install.add_input(Layer::local().including(manifests));
        install.add_cache(root.clone());
        // The image already has a GHC; `--system-ghc` stops Stack downloading
        // a second one for the same version.
        install.add_variable("STACK_YAML", "stack.yaml");
        install.add_command(Command::shell(
            "stack build --system-ghc --only-dependencies",
        ));

        let build = ctx.step(steps::BUILD);
        build.inputs = vec![Layer::step(steps::INSTALL), Layer::local()];
        build.add_cache(root);
        build.add_cache(work);
        build.add_command(Command::shell(format!(
            "stack build --system-ghc --copy-bins --local-bin-path {OUTPUT_DIR}"
        )));
        Ok(())
    }
}

/// The executable to build and run.
fn executable_name(app: &App) -> Result<String> {
    for cabal_file in app.find_files("*.cabal")? {
        let contents = app.read_file(&cabal_file)?;
        if let Some(name) = first_executable_stanza(&contents) {
            return Ok(name);
        }
    }

    if let Some(contents) = app.read_file_opt("package.yaml")? {
        if let Some(name) = hpack_executable(&contents) {
            return Ok(name);
        }
    }

    Err(Error::provider(
        "haskell",
        "no `executable <name>` stanza found in a .cabal file or package.yaml, \
         so there is nothing to run.\n\
         Set `AUTOPACK_START_CMD` if the binary is produced some other way",
    ))
}

/// The name from the first `executable <name>` stanza in a cabal file.
fn first_executable_stanza(cabal: &str) -> Option<String> {
    cabal.lines().find_map(|line| {
        // Stanza headers start at column zero; `build-depends` entries that
        // merely mention the word are always indented.
        if line.starts_with(char::is_whitespace) {
            return None;
        }
        let rest = line.trim().strip_prefix("executable")?;
        let name = rest.split_whitespace().next()?;
        (!name.is_empty()).then(|| name.to_string())
    })
}

/// The first key under hpack's `executables:` map.
fn hpack_executable(package_yaml: &str) -> Option<String> {
    let mut in_executables = false;
    for line in package_yaml.lines() {
        if line.starts_with("executables:") {
            in_executables = true;
            continue;
        }
        if in_executables {
            if !line.starts_with(char::is_whitespace) {
                // Dedented back out of the block without finding a key.
                return None;
            }
            let trimmed = line.trim_end();
            if let Some((name, _)) = trimmed.trim().split_once(':') {
                let name = name.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// The GHC version to build with.
fn ghc_version(app: &App) -> Result<(String, String)> {
    if let Some(project) = app.read_file_opt("cabal.project")? {
        for line in project.lines() {
            if let Some(rest) = line.trim().strip_prefix("with-compiler:") {
                if let Some(version) = rest.trim().strip_prefix("ghc-") {
                    return Ok((version.to_string(), "cabal.project with-compiler".into()));
                }
            }
        }
    }

    Ok((DEFAULT_GHC_VERSION.to_string(), "autopack default".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{plan_for, try_plan_for, write_app};

    const CABAL_FILE: &str = "cabal-version: 2.4\n\
                              name: demo\n\
                              version: 1.0.0\n\
                              \n\
                              executable demo-server\n\
                              \x20   main-is: Main.hs\n\
                              \x20   build-depends: base, network\n";

    #[test]
    fn cabal_projects_install_the_executable() {
        let (_dir, app) = write_app(&[
            ("demo.cabal", CABAL_FILE),
            ("app/Main.hs", "main = pure ()"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "haskell");
        assert_eq!(analysis.metadata["buildTool"], "cabal");
        assert_eq!(analysis.metadata["executable"], "demo-server");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("cabal install exe:demo-server --installdir=/app/bin"));
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("/app/bin/demo-server")
        );
    }

    #[test]
    fn stack_projects_copy_bins() {
        let (_dir, app) = write_app(&[
            ("stack.yaml", "resolver: lts-22.28\n"),
            ("demo.cabal", CABAL_FILE),
            ("app/Main.hs", ""),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.metadata["buildTool"], "stack");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("--copy-bins --local-bin-path /app/bin"));
    }

    #[test]
    fn the_runtime_image_keeps_no_compiler() {
        let (_dir, app) = write_app(&[("demo.cabal", CABAL_FILE), ("app/Main.hs", "")]);
        let analysis = plan_for(&app);

        assert!(analysis.packages.is_empty());
        assert!(analysis.plan.deploy.paths.is_empty());
        assert_eq!(
            analysis.plan.deploy.inputs[0].filter.include,
            vec![OUTPUT_DIR.to_string()]
        );
    }

    #[test]
    fn build_depends_mentioning_executable_is_not_a_stanza() {
        // An indented line is a field, never a stanza header.
        let cabal = "name: demo\n\
                     \x20 build-depends: executable-helper\n\
                     executable real-binary\n\
                     \x20 main-is: Main.hs\n";
        assert_eq!(
            first_executable_stanza(cabal).as_deref(),
            Some("real-binary")
        );
    }

    #[test]
    fn hpack_projects_are_supported() {
        let (_dir, app) = write_app(&[
            (
                "package.yaml",
                "name: demo\nexecutables:\n  demo-exe:\n    main: Main.hs\n",
            ),
            ("app/Main.hs", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["executable"], "demo-exe");
    }

    #[test]
    fn a_project_without_an_executable_is_an_actionable_error() {
        let (_dir, app) = write_app(&[
            (
                "demo.cabal",
                "name: demo\nlibrary\n  exposed-modules: Demo\n",
            ),
            ("src/Demo.hs", ""),
        ]);
        let err = try_plan_for(&app).unwrap_err();
        assert!(err.to_string().contains("AUTOPACK_START_CMD"), "{err}");
    }

    #[test]
    fn cabal_project_can_pin_the_compiler() {
        let (_dir, app) = write_app(&[
            ("demo.cabal", CABAL_FILE),
            ("cabal.project", "packages: .\nwith-compiler: ghc-9.8.2\n"),
            ("app/Main.hs", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["ghcVersion"], "9.8.2");
    }
}
