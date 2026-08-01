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
}
