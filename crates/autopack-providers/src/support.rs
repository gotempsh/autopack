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

/// Package that provides the non-executing ELF inspector below.
pub const ELF_INSPECTION_PACKAGE: &str = "binutils";

/// Record which Debian packages own the shared libraries `binary` links against.
///
/// Runs in the build stage, where the `-dev` packages are still installed.
///
/// This exists because hardcoding runtime package names does not survive a
/// Debian release bump — ICU is `libicu72` on bookworm and `libicu76` on
/// trixie, and the `t64` transition renamed a whole set of others. Asking the
/// linker and then dpkg is release-agnostic, and it installs exactly what the
/// binary links rather than a hand-maintained superset.
///
/// The binary is app-produced and untrusted. `readelf` parses its dynamic
/// section without invoking its ELF interpreter; `ldd` must not be used here
/// because it can execute a hostile interpreter. Installed libraries are
/// resolved from trusted system directories, and absent ones through
/// `apt-file`.
///
/// It is validated against the characters a library name can actually contain,
/// because `|` would otherwise escape the path anchor through alternation and
/// let a crafted `DT_NEEDED` select any package in the archive, which then gets
/// installed as root in the runtime image.
///
/// The survivors are then escaped, because two characters a library name
/// legitimately contains are also regex metacharacters. `+` is a quantifier,
/// so `libxml++-2.6.so.2` matches nothing and the build fails claiming no
/// package provides a library that plainly exists — 238 sonames in bookworm
/// carry a `+`. And `.` matches any character including `/`, so
/// `gio.modules.libgioremote-volume-monitor.so` reaches `gvfs` two directories
/// below the anchor. Escaping both collapses the query to the exact basename.
///
/// `apt-file` and its ~90MB index are only fetched when something is actually
/// missing.
///
pub fn record_runtime_libraries(binary: &str, record_to: &str) -> String {
    let binary = shell_quote(binary);
    let record_to = shell_quote(record_to);
    format!(
        "set -eu; \
         export LC_ALL=C; \
         mkdir -p \"$(dirname {record_to})\"; \
         : > {record_to}; \
         if ! dynamic=$(readelf -d -- {binary}); then \
           echo 'autopack: failed to inspect runtime libraries' >&2; \
           exit 1; \
         fi; \
         needed=$(printf '%s\\n' \"$dynamic\" \
           | awk '$2 == \"(NEEDED)\" {{ print $5 }}' | tr -d '[]' | sort -u); \
         set -f; \
         for lib in $needed; do \
           case \"$lib\" in \
             ''|*[!A-Za-z0-9._+-]*) \
               echo \"autopack: refusing to look up '$lib': not a library name\" >&2; \
               exit 1 ;; \
           esac; \
         done; \
         set +f; \
         for lib in $needed; do \
           case \"$lib\" in \
             ''|*[!A-Za-z0-9._+-]*) \
               echo \"autopack: refusing to look up '$lib': not a library name\" >&2; \
               exit 1 ;; \
           esac; \
           owners=$(for path in /lib/\"$lib\" /usr/lib/\"$lib\" \
             /lib/*-linux-gnu*/\"$lib\" /usr/lib/*-linux-gnu*/\"$lib\"; do \
               if [ -e \"$path\" ]; then readlink -f \"$path\"; fi; \
             done | sort -u \
             | xargs -r dpkg-query -S 2>/dev/null \
             | cut -d: -f1 | sort -u); \
           if [ -z \"$owners\" ]; then \
             if ! command -v apt-file >/dev/null 2>&1; then \
               apt-get update >/dev/null; \
               apt-get install -y --no-install-recommends -- apt-file >/dev/null; \
               apt-file update >/dev/null; \
             fi; \
             pattern=$(printf '%s' \"$lib\" | sed 's/[.+]/\\\\&/g'); \
             owners=$(apt-file search -x \"^/(usr/)?lib/[a-z0-9_]*-linux-gnu[a-z0-9]*/$pattern\\$\" \
               | cut -d: -f1 | sort -u); \
           fi; \
           count=$(printf '%s' \"$owners\" | grep -c . || true); \
           if [ \"$count\" -eq 0 ]; then \
             echo \"autopack: no package provides $lib\" >&2; \
             exit 1; \
           fi; \
           if [ \"$count\" -gt 1 ]; then \
             echo \"autopack: $lib is provided by more than one package:\" >&2; \
             printf '  %s\\n' $owners >&2; \
             echo \"Choose one and add it to apt_packages in autopack.json.\" >&2; \
             exit 1; \
           fi; \
           printf '%s\\n' \"$owners\" >> {record_to}; \
         done; \
         sort -u -o {record_to} {record_to}; \
         cat {record_to}"
    )
}

/// Install the packages a previous [`record_runtime_libraries`] call recorded.
pub fn install_recorded_runtime_libraries(record_to: &str) -> String {
    let record_to = shell_quote(record_to);
    format!(
        "set -eu; \
         if [ -s {record_to} ]; then \
           count=$(awk 'END {{ print NR }}' {record_to}); \
           bytes=$(wc -c < {record_to}); \
           if [ \"$count\" -gt 256 ] || [ \"$bytes\" -gt 16384 ]; then \
             echo 'autopack: refusing an oversized runtime package list' >&2; \
             exit 1; \
           fi; \
           while IFS= read -r package || [ -n \"$package\" ]; do \
             case \"$package\" in \
               ''|?|[!a-z0-9]*|*[!a-z0-9+.-]*) \
                 echo \"autopack: invalid recorded runtime package: $package\" >&2; exit 1 ;; \
             esac; \
           done < {record_to}; \
           apt-get update; \
           apt-get install -y --no-install-recommends -- $(cat {record_to}); \
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
    use std::process::Command as ProcessCommand;

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
    fn missing_libraries_are_resolved_and_hostile_sonames_refused() {
        let script = record_runtime_libraries("/app/bin/app", "/tmp/deps");

        // App-produced ELF files are parsed statically. `ldd` can execute a
        // hostile ELF interpreter and must never appear in this command.
        assert!(script.contains("readelf -d --"));
        assert!(!script.contains("ldd "));

        // Installed libraries are resolved through trusted system paths.
        assert!(script.contains("dpkg-query -S"));
        assert!(!script.contains("find /lib /usr/lib"));
        // Missing libraries are looked up rather than dropped on the floor.
        assert!(script.contains("apt-file search"));

        // A soname comes from a binary the app produced. Anything outside the
        // character set a library name can hold is refused before it reaches
        // the regex — `|` alone escapes the anchor through alternation.
        assert!(script.contains("*[!A-Za-z0-9._+-]*"));
        assert!(script.contains("refusing to look up"));

        // `.` and `+` survive that guard because a library name may contain
        // them, and both are regex metacharacters — `+` quantifies, so a C++
        // soname matches nothing, and `.` matches `/`, reaching packages below
        // the anchored directory.
        assert!(script.contains("sed 's/[.+]/"));

        // Globbing is disabled while untrusted sonames are validated, then
        // restored so direct multiarch loader-directory patterns expand.
        assert!(script.contains("set -eu"));
        let disable = script.find("set -f;").unwrap();
        let enable = script.find("set +f;").unwrap();
        let loader_glob = script.find("/lib/*-linux-gnu*/").unwrap();
        assert!(disable < enable && enable < loader_glob);
        assert!(script.contains("LC_ALL=C"));

        // The anchor covers /lib as well as /usr/lib: on Debian the essential
        // libraries are still recorded unmerged, so a /usr-only anchor fails
        // the build for libc, libz and friends.
        assert!(script.contains("^/(usr/)?lib/[a-z0-9_]*-linux-gnu[a-z0-9]*/"));

        // Ambiguity and absence both stop the build with something actionable
        // rather than guessing.
        assert!(script.contains("no package provides"));
        assert!(script.contains("provided by more than one package"));
        assert!(script.contains("add it to apt_packages"));

        // The ~90MB index is fetched only when the library is not installed.
        let guard = script.find("if [ -z \"$owners\" ]").unwrap();
        assert!(guard < script.find("apt-file update").unwrap());
    }

    #[test]
    fn recorded_runtime_packages_are_revalidated_before_apt() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("runtime-deps");
        // No trailing newline: validation and bounds must still include it.
        fs::write(&record, "-o").unwrap();

        let script = install_recorded_runtime_libraries(record.to_str().unwrap());
        assert!(script.contains("-- $(cat"));
        let output = ProcessCommand::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid recorded runtime package")
        );
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
