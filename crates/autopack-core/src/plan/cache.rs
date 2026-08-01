//! Build caches shared between builds of the same app.

use serde::{Deserialize, Serialize};

/// How concurrent builds may share a cache directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheType {
    /// Several builds may use the directory at once. Correct for content
    /// addressed download caches (npm, pip, cargo registry).
    #[default]
    Shared,
    /// Only one build may use the directory at a time. Required for caches
    /// that are not safe under concurrent writers (most compiler output dirs).
    Locked,
}

/// A directory persisted across builds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    /// Absolute path inside the build container.
    pub directory: String,
    /// Sharing mode. Defaults to [`CacheType::Shared`].
    #[serde(default, rename = "type")]
    pub cache_type: CacheType,
}

impl Cache {
    /// A shared cache over `directory`.
    pub fn shared(directory: impl Into<String>) -> Self {
        Self {
            directory: directory.into(),
            cache_type: CacheType::Shared,
        }
    }

    /// A locked cache over `directory`.
    pub fn locked(directory: impl Into<String>) -> Self {
        Self {
            directory: directory.into(),
            cache_type: CacheType::Locked,
        }
    }
}
