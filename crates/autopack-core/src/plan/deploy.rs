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

    /// Named one-off commands the platform runs against this image, keyed by
    /// process name — `release` before a deploy goes live, `worker` alongside
    /// it.
    ///
    /// They share the image, so nothing extra is built for them. Keeping them
    /// out of the start command is what stops a migration running once per
    /// replica per restart.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub tasks: IndexMap<String, String>,
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

    /// Register a task the platform can run against the image.
    pub fn add_task(&mut self, name: impl Into<String>, command: impl Into<String>) -> &mut Self {
        self.tasks.insert(name.into(), command.into());
        self
    }

    /// The conventional pre-deploy task, if one was declared.
    pub fn release_task(&self) -> Option<&str> {
        self.tasks.get(crate::procfile::RELEASE).map(String::as_str)
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
