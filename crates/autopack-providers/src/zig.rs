//! Zig provider.

use autopack_core::plan::{Command, Layer};
use autopack_core::{steps, App, BuildContext, Environment, Provider, Result};

use crate::support::procfile_web_command;

/// Zig version installed when the project does not pin one.
const DEFAULT_ZIG_VERSION: &str = "0.13.0";

/// Where `zig build --prefix` places executables.
const OUTPUT_DIR: &str = "/app/out";

/// Builds Zig applications.
pub struct ZigProvider;

impl Provider for ZigProvider {
    fn id(&self) -> &'static str {
        "zig"
    }

    fn display_name(&self) -> &'static str {
        "Zig"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_any_file(["build.zig", "build.zig.zon"]))
    }

    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        let version = ctx
            .env
            .config("ZIG_VERSION")
            .unwrap_or(DEFAULT_ZIG_VERSION)
            .to_string();
        // Zig ships a self-contained toolchain tarball, so mise installs it in
        // seconds — no reason to reach for a language image here.
        ctx.packages.add("zig", &version, "autopack default");
        ctx.add_metadata("zigVersion", &version);

        let name = project_name(ctx.app)?;
        ctx.add_metadata("project", name.as_deref().unwrap_or("(from zig build)"));

        let cache = ctx.shared_cache("zig", "/root/.cache/zig");

        let step = ctx.step(steps::BUILD);
        step.inputs = vec![Layer::step(steps::PACKAGES), Layer::local()];
        step.add_cache(cache);
        step.add_command(Command::shell(format!(
            "zig build --prefix {OUTPUT_DIR} -Doptimize=ReleaseSafe"
        )));
        // Zig links statically against musl/glibc depending on the target, and
        // the executable name comes from build.zig rather than any manifest, so
        // the single artefact in bin/ is the reliable answer.
        step.add_command(Command::shell(format!(
            "test -d {OUTPUT_DIR}/bin && \
             mkdir -p /app/bin && \
             cp \"$(find {OUTPUT_DIR}/bin -maxdepth 1 -type f -perm -u+x -print -quit)\" /app/bin/app"
        )));

        // A Zig binary links libc at most; nothing else is needed at run time.
        ctx.set_runtime_includes_runtimes(false);
        ctx.add_deploy_input(Layer::step(steps::BUILD).including(["/app/bin/app"]));

        let start = match procfile_web_command(ctx.app)? {
            Some(command) => command,
            None => "/app/bin/app".to_string(),
        };
        ctx.set_start_command(start);
        Ok(())
    }
}

/// The `.name` field from `build.zig.zon`, when there is one.
fn project_name(app: &App) -> Result<Option<String>> {
    let Some(zon) = app.read_file_opt("build.zig.zon")? else {
        return Ok(None);
    };
    Ok(zon.split(".name").nth(1).and_then(|rest| {
        let rest = rest.trim_start().strip_prefix('=')?.trim_start();
        // Zig 0.14 switched the field from `.name = "x"` to an enum literal
        // `.name = .x`, so both spellings have to be accepted.
        let rest = match rest.strip_prefix('"') {
            Some(quoted) => quoted,
            None => rest.strip_prefix('.')?,
        };
        let end = rest.find(['"', ',', '\n'])?;
        let name = rest[..end].trim();
        (!name.is_empty()).then(|| name.to_string())
    }))
}

#[cfg(test)]
mod tests {
    use crate::test_support::{plan_for, write_app};

    #[test]
    fn builds_with_the_zig_build_system() {
        let (_dir, app) = write_app(&[
            ("build.zig", "pub fn build(b: *std.Build) void {}"),
            ("src/main.zig", "pub fn main() void {}"),
        ]);
        let analysis = plan_for(&app);

        assert_eq!(analysis.provider, "zig");
        assert!(analysis.plan.step("build").unwrap().commands[0]
            .display_name()
            .contains("zig build --prefix /app/out"));
        assert_eq!(
            analysis.plan.deploy.start_command.as_deref(),
            Some("/app/bin/app")
        );
    }

    #[test]
    fn reads_the_project_name_from_zon() {
        let (_dir, app) = write_app(&[
            ("build.zig", ""),
            (
                "build.zig.zon",
                ".{\n    .name = \"demo\",\n    .version = \"1.0.0\",\n}",
            ),
            ("src/main.zig", ""),
        ]);
        assert_eq!(plan_for(&app).metadata["project"], "demo");
    }
}
