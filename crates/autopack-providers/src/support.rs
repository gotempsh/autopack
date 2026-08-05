//! Helpers shared by providers.

use autopack_core::plan::Layer;
use autopack_core::{App, Procfile, Result};

/// Image the static file server binary is copied from.
///
/// Caddy is a single static Go binary, so it can be lifted out of the official
/// image into a Debian runtime without pulling in Alpine's userland.
pub const CADDY_IMAGE: &str = "caddy:2-alpine";

/// Path of the Caddy binary inside [`CADDY_IMAGE`] and in the runtime image.
pub const CADDY_BIN: &str = "/usr/bin/caddy";

/// Path the generated Caddyfile is written to.
pub const CADDYFILE_PATH: &str = "/app/Caddyfile";

/// A layer that copies the Caddy binary into the runtime image.
pub fn caddy_layer() -> Layer {
    Layer::image(CADDY_IMAGE).including([CADDY_BIN])
}

/// Command that runs the generated Caddyfile.
pub fn caddy_start_command() -> String {
    format!("caddy run --config {CADDYFILE_PATH} --adapter caddyfile")
}

/// Generate a Caddyfile serving `root`.
///
/// `spa` rewrites unknown paths to `index.html`, which is what client-side
/// routers need and what breaks deep links when it is missing.
pub fn caddyfile(root: &str, spa: bool) -> String {
    let try_files = if spa {
        "\ttry_files {path} {path}/ /index.html\n"
    } else {
        "\ttry_files {path} {path}/ {path}.html\n"
    };

    format!(
        "{{\n\
         \tadmin off\n\
         \tpersist_config off\n\
         \tauto_https off\n\
         \tlog {{\n\t\tformat console\n\t}}\n\
         }}\n\
         \n\
         # PORT is supplied by the platform; 3000 keeps `docker run -p 3000:3000` working.\n\
         :{{$PORT:3000}} {{\n\
         \troot * {root}\n\
         \tencode zstd gzip\n\
         {try_files}\
         \tfile_server\n\
         \theader /assets/* Cache-Control \"public, max-age=31536000, immutable\"\n\
         }}\n"
    )
}

/// The `web:` process from a Procfile, if there is one.
///
/// Procfiles are the closest thing to a cross-language declaration of "how do
/// I start", so every provider checks for one before guessing. The non-`web`
/// processes become tasks, registered centrally by `analyze`.
pub fn procfile_web_command(app: &App) -> Result<Option<String>> {
    Ok(Procfile::load(app)?.and_then(|p| p.web().map(str::to_string)))
}

/// Manifest files that unambiguously identify an ecosystem.
///
/// Used to gate *weak* detection signals. "There is a `.py` file somewhere" is
/// true of plenty of Node repositories with a helper script, so a provider that
/// falls back to a file-extension scan must first check that no other
/// ecosystem has staked a claim.
const ECOSYSTEM_MANIFESTS: &[&str] = &[
    "package.json",
    "deno.json",
    "deno.jsonc",
    "composer.json",
    "Gemfile",
    "go.mod",
    "Cargo.toml",
    "mix.exs",
    "gleam.toml",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "requirements.txt",
    "pyproject.toml",
    "Pipfile",
];

/// The first foreign ecosystem manifest found in the app root, if any.
///
/// `own` names the caller's own manifests, which are not "foreign".
pub fn foreign_manifest(app: &App, own: &[&str]) -> Option<&'static str> {
    ECOSYSTEM_MANIFESTS
        .iter()
        .find(|manifest| !own.contains(manifest) && app.has_file(manifest))
        .copied()
}

/// Read a version file such as `.nvmrc` or `.python-version`.
///
/// Returns the first non-empty, non-comment line with any leading `v` removed.
pub fn read_version_file(app: &App, name: &str) -> Result<Option<String>> {
    let Some(contents) = app.read_file_opt(name)? else {
        return Ok(None);
    };

    Ok(contents
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.trim_start_matches('v').to_string()))
}

/// Turn a semver range such as `>=20`, `^22.1.0` or `~3.12` into a mise version.
///
/// mise understands plain prefixes (`22`, `3.12`) but not range operators, so
/// the operators are stripped and the resulting prefix is used. Ranges with an
/// upper bound (`>=18 <21`) keep only the first constraint, which is the
/// version the app is most likely developed against.
pub fn normalize_version_range(range: &str) -> Option<String> {
    let first = range.split_whitespace().next()?.trim();
    let first = first.split("||").next()?.trim();
    let stripped = first
        .trim_start_matches(['^', '~', '>', '=', '<', 'v'])
        .trim();

    if stripped.is_empty() || stripped == "*" || stripped.eq_ignore_ascii_case("x") {
        return None;
    }

    // `18.x` -> `18`
    let cleaned = stripped
        .split('.')
        .take_while(|part| part.chars().all(|c| c.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join(".");

    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Shell-quote a value for safe interpolation into a generated command.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Record which Debian packages own the shared libraries `glob` links against.
///
/// Runs in the build stage, where the `-dev` packages are still installed:
/// `ldd` reports a missing library as "not found" with no path, so the lookup
/// cannot be deferred to the runtime image.
///
/// This exists because hardcoding runtime package names does not survive a
/// Debian release bump — ICU is `libicu72` on bookworm and `libicu76` on
/// trixie, and the `t64` transition renamed a whole set of others. Asking the
/// linker and then dpkg is release-agnostic.
///
/// Two lookups, because `ldd` answers in two different shapes:
///
/// - `libfoo.so.1 => /usr/lib/.../libfoo.so.1` — resolved, so the file is on
///   disk and `dpkg-query -S` names its owner.
/// - `libfoo.so.1 => not found` — absent from the build image too, so there is
///   no path and no file to ask dpkg about. These used to be dropped on the
///   floor by the `=> /` filter, which meant a genuinely missing library
///   produced a short list and an image that failed at run time with a loader
///   error rather than a build that failed with an explanation.
///
/// The second shape is resolved with `apt-file`, which maps a filename to the
/// package providing it without needing the file present. The query is
/// anchored to the multiarch library directory on purpose: a bare basename
/// search for `libatomic.so.1` also matches `lib32atomic1` and the
/// `-cross` packages, and for `libnss3.so` it matches `firefox-esr` and
/// `thunderbird`, so an unanchored `head -1` installs something wildly wrong.
///
/// `apt-file` and its index are only fetched when there is something to look
/// up — the index is around 90MB, which is not worth paying for on every
/// build that has nothing missing.
pub fn record_runtime_libraries(glob: &str, record_to: &str) -> String {
    format!(
        "set -eu; \
         mkdir -p \"$(dirname {record_to})\"; \
         ldd {glob} 2>/dev/null \
           | awk '/=> \\// {{ print $3 }}' | sort -u \
           | xargs -r readlink -f 2>/dev/null | sort -u \
           | xargs -r dpkg-query -S 2>/dev/null \
           | cut -d: -f1 | sort -u > {record_to}; \
         missing=$(ldd {glob} 2>/dev/null | awk '/not found/ {{ print $1 }}' | sort -u); \
         if [ -n \"$missing\" ]; then \
           apt-get update >/dev/null; \
           apt-get install -y --no-install-recommends apt-file >/dev/null; \
           apt-file update >/dev/null; \
           for lib in $missing; do \
             owner=$(apt-file search -x \"^/usr/lib/[a-z0-9_]*-linux-gnu/$lib$\" 2>/dev/null \
               | cut -d: -f1 | sort -u | head -1); \
             if [ -n \"$owner\" ]; then \
               echo \"$owner\" >> {record_to}; \
             else \
               echo \"autopack: no package provides $lib\" >&2; \
               exit 1; \
             fi; \
           done; \
           sort -u -o {record_to} {record_to}; \
         fi; \
         cat {record_to}"
    )
}

/// Install the packages a previous [`record_runtime_libraries`] call recorded.
pub fn install_recorded_runtime_libraries(record_to: &str) -> String {
    format!(
        "set -eu; \
         if [ -s {record_to} ]; then \
           apt-get update; \
           apt-get install -y --no-install-recommends $(cat {record_to}); \
           rm -rf /var/lib/apt/lists/*; \
         fi"
    )
}

/// Default location for the recorded runtime package list.
pub const RUNTIME_DEPS_FILE: &str = "/usr/local/share/autopack-runtime-deps";

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn app_with(files: &[(&str, &str)]) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, contents).unwrap();
        }
        let app = App::new(dir.path()).unwrap();
        (dir, app)
    }

    #[test]
    fn missing_libraries_are_resolved_rather_than_dropped() {
        let script = record_runtime_libraries("/app/bin/app", "/tmp/deps");

        // The resolved branch is unchanged: ldd gives a path, dpkg names the
        // owner.
        assert!(script.contains("dpkg-query -S"));

        // The "not found" branch is the point of this: those lines carry no
        // path, so they used to be filtered out and the library silently
        // omitted from the runtime image.
        assert!(script.contains("/not found/"));
        assert!(script.contains("apt-file search"));

        // Anchored to the multiarch directory. A bare basename search for
        // libatomic.so.1 also matches lib32atomic1 and the -cross packages,
        // and libnss3.so matches firefox-esr and thunderbird, so an
        // unanchored match would install something wildly wrong.
        assert!(script.contains("^/usr/lib/[a-z0-9_]*-linux-gnu/$lib$"));

        // A library nothing provides fails the build rather than producing a
        // short list and an image that dies with a loader error.
        assert!(script.contains("no package provides"));
        assert!(script.contains("exit 1"));

        // apt-file's index is ~90MB, so it is only fetched when there is
        // something to look up.
        let index_fetch = script.find("apt-file update").unwrap();
        let guard = script.find("if [ -n \"$missing\" ]").unwrap();
        assert!(guard < index_fetch);
    }

    #[test]
    fn parses_the_web_process() {
        let (_dir, app) = app_with(&[(
            "Procfile",
            "# comment\nrelease: ./migrate\nweb: gunicorn app:app\n",
        )]);
        assert_eq!(
            procfile_web_command(&app).unwrap().as_deref(),
            Some("gunicorn app:app")
        );
    }

    #[test]
    fn missing_procfile_is_not_an_error() {
        let (_dir, app) = app_with(&[]);
        assert_eq!(procfile_web_command(&app).unwrap(), None);
    }

    #[test]
    fn version_files_drop_the_v_prefix() {
        let (_dir, app) = app_with(&[(".nvmrc", "v22.3.0\n")]);
        assert_eq!(
            read_version_file(&app, ".nvmrc").unwrap().as_deref(),
            Some("22.3.0")
        );
    }

    #[test]
    fn ranges_become_mise_prefixes() {
        assert_eq!(
            normalize_version_range("^22.1.0").as_deref(),
            Some("22.1.0")
        );
        assert_eq!(normalize_version_range(">=20").as_deref(), Some("20"));
        assert_eq!(normalize_version_range("18.x").as_deref(), Some("18"));
        assert_eq!(normalize_version_range(">=18 <21").as_deref(), Some("18"));
        assert_eq!(normalize_version_range("*"), None);
    }

    #[test]
    fn spa_caddyfile_falls_back_to_index() {
        let config = caddyfile("/app/dist", true);
        assert!(config.contains("try_files {path} {path}/ /index.html"));
        assert!(config.contains("root * /app/dist"));
    }
}
