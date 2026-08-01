//! `autopack.lock` — exact runtime versions and base image digests.
//!
//! Without a lock, `node = "22"` means "whatever 22.x mise considers latest on
//! the day you build", and `debian:bookworm-slim` means "whatever that tag
//! points at today". Two builds of the same commit six months apart can differ
//! in the interpreter, the system libraries, and every CVE in between.
//!
//! This is the one dimension where Nix-based builders are genuinely stronger
//! out of the box: a Nix pin *is* an exact closure. The lock closes that gap
//! for autopack by recording, once, what the fuzzy specifications resolved to,
//! and then pinning them on every later build.
//!
//! Two things are locked, and the second matters more than the first:
//!
//! * **Runtime versions** — `node = "22"` becomes `node = "22.14.0"`.
//! * **Image digests** — `debian:bookworm-slim` becomes
//!   `debian:bookworm-slim@sha256:…`. A tag is mutable; a digest is not, so
//!   this is what actually stops the base image moving under you.

use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// File name of the lock, relative to the app root.
pub const LOCK_FILE: &str = "autopack.lock";

/// Schema version, so a future format change can be detected rather than
/// silently misread.
pub const LOCK_VERSION: u32 = 1;

/// Resolved versions and digests for one application.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    /// Format version.
    pub version: u32,

    /// Exact runtime versions, keyed by mise tool name.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub tools: IndexMap<String, String>,

    /// Image reference to digest, e.g. `debian:bookworm-slim` ->
    /// `sha256:abc…`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub images: IndexMap<String, String>,
}

impl Lock {
    /// An empty lock at the current schema version.
    pub fn new() -> Self {
        Self {
            version: LOCK_VERSION,
            ..Default::default()
        }
    }

    /// Read the lock from an app directory, if one exists.
    pub fn load(app_root: &Path) -> Result<Option<Self>> {
        let path = app_root.join(LOCK_FILE);
        if !path.is_file() {
            return Ok(None);
        }

        let contents = std::fs::read_to_string(&path).map_err(|source| Error::ReadFile {
            path: LOCK_FILE.into(),
            source,
        })?;
        let lock: Lock = serde_json::from_str(&contents).map_err(|e| Error::ParseFile {
            path: LOCK_FILE.into(),
            message: e.to_string(),
        })?;

        if lock.version != LOCK_VERSION {
            return Err(Error::ParseFile {
                path: LOCK_FILE.into(),
                message: format!(
                    "lock format version {} is not supported (expected {LOCK_VERSION}). \
                     Regenerate it with `autopack lock`",
                    lock.version
                ),
            });
        }

        Ok(Some(lock))
    }

    /// Serialise the lock, newline-terminated so it is diff-friendly.
    pub fn to_json(&self) -> Result<String> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        Ok(json)
    }

    /// The locked version of `tool`, if any.
    pub fn tool(&self, name: &str) -> Option<&str> {
        self.tools.get(name).map(String::as_str)
    }

    /// Pin `image` to its locked digest.
    ///
    /// Returns the reference unchanged when nothing is locked for it, and
    /// leaves an already-digest-pinned reference alone.
    pub fn pin_image(&self, image: &str) -> String {
        if image.contains('@') {
            return image.to_string();
        }
        match self.images.get(image) {
            Some(digest) => format!("{image}@{digest}"),
            None => image.to_string(),
        }
    }

    /// Record a resolved tool version.
    pub fn set_tool(&mut self, name: impl Into<String>, version: impl Into<String>) {
        self.tools.insert(name.into(), version.into());
    }

    /// Record an image digest.
    pub fn set_image(&mut self, image: impl Into<String>, digest: impl Into<String>) {
        self.images.insert(image.into(), digest.into());
    }

    /// True when nothing is pinned.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.images.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinning_appends_the_digest() {
        let mut lock = Lock::new();
        lock.set_image("debian:bookworm-slim", "sha256:abc");
        assert_eq!(
            lock.pin_image("debian:bookworm-slim"),
            "debian:bookworm-slim@sha256:abc"
        );
    }

    #[test]
    fn an_unlocked_image_is_left_alone() {
        let lock = Lock::new();
        assert_eq!(lock.pin_image("alpine:3.20"), "alpine:3.20");
    }

    #[test]
    fn an_already_pinned_reference_is_not_double_pinned() {
        let mut lock = Lock::new();
        lock.set_image("debian:bookworm-slim", "sha256:abc");
        let pinned = "debian:bookworm-slim@sha256:def";
        assert_eq!(lock.pin_image(pinned), pinned);
    }

    #[test]
    fn round_trips_through_json() {
        let mut lock = Lock::new();
        lock.set_tool("node", "22.14.0");
        lock.set_image("debian:bookworm-slim", "sha256:abc");

        let json = lock.to_json().unwrap();
        let parsed: Lock = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, lock);
        assert!(json.ends_with('\n'));
    }

    #[test]
    fn a_future_format_version_is_rejected_rather_than_misread() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(LOCK_FILE),
            r#"{"version":999,"tools":{"node":"22.0.0"}}"#,
        )
        .unwrap();

        let err = Lock::load(dir.path()).unwrap_err();
        assert!(err.to_string().contains("autopack lock"), "{err}");
    }

    #[test]
    fn a_missing_lock_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Lock::load(dir.path()).unwrap(), None);
    }
}
