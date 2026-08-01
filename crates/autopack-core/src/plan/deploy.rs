//! The runtime image description.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::Layer;

/// What the final image looks like and how the container starts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deploy {
    /// Base filesystem for the runtime image.
    #[serde(default)]
    pub base: Layer,

    /// Additional layers copied onto the base, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<Layer>,

    /// Command the container runs.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startCommand"
    )]
    pub start_command: Option<String>,

    /// Environment variables baked into the image.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, String>,

    /// Directories prepended to `PATH` in the runtime image.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,

    /// User the container runs as. `None` means root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<RuntimeUser>,
}

/// An unprivileged user created in the runtime image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeUser {
    /// Account name.
    pub name: String,
    /// Numeric uid. Used for `COPY --chown`, which cannot resolve names.
    pub uid: u32,
    /// Numeric gid.
    pub gid: u32,
    /// Home directory, which must be writable for tools that cache there.
    pub home: String,
}

impl Deploy {
    /// Names of every step the runtime image reads from.
    pub fn roots(&self) -> impl Iterator<Item = &str> {
        std::iter::once(&self.base)
            .chain(self.inputs.iter())
            .filter_map(|layer| layer.step.as_deref())
    }

    /// Set an environment variable on the runtime image.
    pub fn add_variable(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.variables.insert(key.into(), value.into());
        self
    }

    /// Prepend a directory to the runtime `PATH`, ignoring duplicates.
    pub fn add_path(&mut self, path: impl Into<String>) -> &mut Self {
        let path = path.into();
        if !self.paths.contains(&path) {
            self.paths.push(path);
        }
        self
    }
}
