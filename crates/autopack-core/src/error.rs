//! Error types for autopack.
//!
//! Every fallible operation in the core returns [`Error`]. Providers surface
//! failures through the same type so the CLI can render a single, actionable
//! message instead of a stack of `anyhow` context lines.

use std::path::PathBuf;

/// Result alias used throughout autopack.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while analysing a source directory or
/// generating a build plan.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The source directory does not exist or is not a directory.
    #[error("source path `{0}` is not a directory")]
    InvalidSource(PathBuf),

    /// A file the provider expected to read could not be read.
    #[error("failed to read `{path}`: {source}")]
    ReadFile {
        /// Path that failed to read, relative to the app root when known.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A structured file (JSON/TOML) exists but could not be parsed.
    #[error("failed to parse `{path}`: {message}")]
    ParseFile {
        /// Path of the offending file.
        path: PathBuf,
        /// Human readable parser message.
        message: String,
    },

    /// The glob pattern supplied to [`crate::App::find_files`] is invalid.
    #[error("invalid glob pattern `{pattern}`: {message}")]
    InvalidGlob {
        /// The pattern that failed to compile.
        pattern: String,
        /// Reason the pattern is invalid.
        message: String,
    },

    /// No provider matched and no configuration forced one.
    #[error(
        "no provider could be detected for this app.\n\
         Set a provider explicitly with `AUTOPACK_PROVIDER=<name>` or an \
         `autopack.json` containing {{\"provider\": \"<name>\"}}"
    )]
    NoProviderDetected,

    /// `AUTOPACK_PROVIDER` / `autopack.json` named a provider that is not registered.
    #[error("unknown provider `{name}`. Available providers: {available}")]
    UnknownProvider {
        /// The requested provider id.
        name: String,
        /// Comma separated list of registered provider ids.
        available: String,
    },

    /// A provider detected the app but could not work out how to start it.
    #[error("{provider}: {message}")]
    Provider {
        /// Provider id that produced the failure.
        provider: String,
        /// What went wrong, phrased for the end user.
        message: String,
    },

    /// Nothing determined how the container should start.
    #[error(
        "could not work out how to start this app.\n\
         Set a start command with `AUTOPACK_START_CMD=...`, a `web:` line in a \
         Procfile, or {{\"deploy\": {{\"startCommand\": \"...\"}}}} in autopack.json"
    )]
    MissingStartCommand,

    /// The generated plan is internally inconsistent (dangling step reference, cycle, ...).
    #[error("invalid build plan: {0}")]
    InvalidPlan(String),

    /// Serialising or deserialising a plan/config failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Convenience constructor for [`Error::Provider`].
    pub fn provider(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Provider {
            provider: provider.into(),
            message: message.into(),
        }
    }
}
