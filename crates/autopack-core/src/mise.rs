//! Language runtime installation via [mise](https://mise.jdx.dev).
//!
//! autopack does not ship a curated package set. Providers declare *what* they
//! need (`node@22`, `python@3.12`) and mise resolves and installs it inside the
//! build. That keeps autopack out of the business of tracking every language
//! release, and gives users the same version syntax they already use locally
//! via `.tool-versions`, `.nvmrc`, or `mise.toml`.

use indexmap::IndexMap;

/// mise release installed into the builder image.
///
/// Pinned so a rebuild of the same commit resolves the same tool versions.
/// Override with `AUTOPACK_MISE_VERSION`.
pub const DEFAULT_MISE_VERSION: &str = "v2026.7.18";

/// Directory mise uses for installed tools, shims, and its config.
pub const MISE_DIR: &str = "/mise";

/// Directory added to `PATH` so installed tools are callable.
pub const MISE_SHIMS: &str = "/mise/shims";

/// Full fingerprint of the release key documented by mise.
const MISE_RELEASE_KEY_FINGERPRINT: &str = "24853EC9F655CE80B48E6C3A8B81C9D17413A06D";

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Shell command that downloads and runs the pinned mise installer.
///
/// The signed, versioned release asset is verified against mise's pinned
/// release-key fingerprint before execution. Temporary files are private and
/// removed on success, failure, or interruption.
pub fn installer_command(version: &str) -> String {
    let quoted_version = shell_quote(version);
    let signature_url = shell_quote(&format!(
        "https://github.com/jdx/mise/releases/download/{version}/install.sh.sig"
    ));
    format!(
        "mise_tmp=$(mktemp -d) \
         && chmod 700 \"$mise_tmp\" \
         && trap 'rm -rf \"$mise_tmp\"' EXIT HUP INT TERM \
         && mkdir -m 700 \"$mise_tmp/gnupg\" \
         && curl --fail --silent --show-error --location \
              --retry 5 --retry-all-errors --retry-delay 2 \
              --connect-timeout 20 --max-time 120 \
              --output \"$mise_tmp/key.asc\" https://mise.jdx.dev/gpg-key.pub \
         && test \"$(gpg --homedir \"$mise_tmp/gnupg\" --batch --with-colons \
              --show-keys \"$mise_tmp/key.asc\" \
              | awk -F: '$1 == \"fpr\" {{ print $10; exit }}')\" = \
              '{MISE_RELEASE_KEY_FINGERPRINT}' \
         && gpg --homedir \"$mise_tmp/gnupg\" --batch \
              --import \"$mise_tmp/key.asc\" >/dev/null 2>&1 \
         && curl --fail --silent --show-error --location \
              --retry 5 --retry-all-errors --retry-delay 2 \
              --connect-timeout 20 --max-time 120 \
              --output \"$mise_tmp/install.sh.sig\" {signature_url} \
         && gpg --homedir \"$mise_tmp/gnupg\" --batch \
              --status-fd 3 --decrypt \"$mise_tmp/install.sh.sig\" \
              3> \"$mise_tmp/gpg-status\" > \"$mise_tmp/install.sh\" \
         && grep -Fq '[GNUPG:] VALIDSIG {MISE_RELEASE_KEY_FINGERPRINT} ' \
              \"$mise_tmp/gpg-status\" \
         && MISE_VERSION={quoted_version} sh \"$mise_tmp/install.sh\""
    )
}

/// Command used by `autopack lock` to resolve one tool specification.
pub fn latest_command(tool: &str, version: &str) -> String {
    format!(
        "/usr/local/bin/mise latest {}",
        shell_quote(&format!("{tool}@{version}"))
    )
}

/// A runtime a provider asked for, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRequest {
    /// Version specifier in mise syntax: `22`, `3.12.4`, `latest`, `lts`.
    pub version: String,
    /// Where the version came from, shown by `autopack info` (e.g. `.nvmrc`).
    pub source: String,
}

/// The set of runtimes to install, in declaration order.
#[derive(Debug, Clone, Default)]
pub struct MisePackages {
    packages: IndexMap<String, PackageRequest>,
}

impl MisePackages {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request `tool` at `version`, recording `source` for diagnostics.
    ///
    /// A later request for the same tool wins, which is what makes user
    /// configuration override provider defaults.
    pub fn add(
        &mut self,
        tool: impl Into<String>,
        version: impl Into<String>,
        source: impl Into<String>,
    ) -> &mut Self {
        self.packages.insert(
            tool.into(),
            PackageRequest {
                version: version.into(),
                source: source.into(),
            },
        );
        self
    }

    /// Request `tool` at `version` only if it was not already requested.
    pub fn add_default(
        &mut self,
        tool: impl Into<String>,
        version: impl Into<String>,
        source: impl Into<String>,
    ) -> &mut Self {
        let tool = tool.into();
        if !self.packages.contains_key(&tool) {
            self.add(tool, version, source);
        }
        self
    }

    /// Requested runtimes.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &PackageRequest)> {
        self.packages
            .iter()
            .map(|(name, request)| (name.as_str(), request))
    }

    /// True when nothing needs installing.
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// Render the `mise.toml` that pins every requested runtime.
    ///
    /// Emitted as a file asset rather than a series of `mise use` commands so
    /// the install layer's cache key is exactly "the set of versions changed".
    pub fn to_toml(&self) -> String {
        let mut out = String::from("[tools]\n");
        for (name, request) in &self.packages {
            out.push_str(&format!("{name} = \"{}\"\n", request.version));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_requests_win() {
        let mut packages = MisePackages::new();
        packages.add("node", "20", "default");
        packages.add("node", "22", ".nvmrc");
        assert_eq!(packages.to_toml(), "[tools]\nnode = \"22\"\n");
    }

    #[test]
    fn add_default_does_not_clobber() {
        let mut packages = MisePackages::new();
        packages.add("node", "22", ".nvmrc");
        packages.add_default("node", "20", "provider default");
        let (_, request) = packages.iter().next().unwrap();
        assert_eq!(request.version, "22");
        assert_eq!(request.source, ".nvmrc");
    }

    #[test]
    fn installer_download_retries_without_piping_partial_content_to_shell() {
        let command = installer_command("v2026.7.18");

        assert!(command.contains("--retry 5 --retry-all-errors"));
        assert!(command.contains("--connect-timeout 20 --max-time 120"));
        assert!(command.contains("install.sh.sig"));
        assert!(!command.contains("https://mise.run |"));
        assert!(command.contains(MISE_RELEASE_KEY_FINGERPRINT));
        assert!(command.contains(&format!(
            "[GNUPG:] VALIDSIG {MISE_RELEASE_KEY_FINGERPRINT} "
        )));
        assert!(command.contains("MISE_VERSION='v2026.7.18' sh \"$mise_tmp/install.sh\""));
        assert!(command.contains("mise_tmp=$(mktemp -d)"));
        assert!(command.contains("trap 'rm -rf \"$mise_tmp\"' EXIT HUP INT TERM"));
    }

    #[test]
    fn installer_version_is_shell_quoted() {
        let command = installer_command("v1'; touch /tmp/pwned; '");

        assert!(command.contains("MISE_VERSION='v1'\"'\"'; touch /tmp/pwned; '\"'\"''"));
        assert!(command
            .contains("releases/download/v1'\"'\"'; touch /tmp/pwned; '\"'\"'/install.sh.sig'"));
    }

    #[test]
    fn latest_tool_spec_is_shell_quoted() {
        assert_eq!(
            latest_command("node; touch /tmp/pwned", "22' && id"),
            "/usr/local/bin/mise latest 'node; touch /tmp/pwned@22'\"'\"' && id'"
        );
    }
}
