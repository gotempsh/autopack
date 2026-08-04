//! # autopack-core
//!
//! Turns a source directory into a [`BuildPlan`]: a description of the steps,
//! layers, caches and runtime image needed to build and run an application,
//! worked out from the source rather than written by hand.
//!
//! The plan is backend-agnostic — nothing here emits a Dockerfile. Lowering it
//! to one is the `autopack-dockerfile` crate's job, and is the only backend
//! today.
//!
//! The pipeline is three stages:
//!
//! 1. [`App`] indexes the source directory.
//! 2. A [`Provider`] recognises the app and fills in a [`BuildContext`].
//! 3. [`BuildContext::generate`] adds the shared runtime layer, applies user
//!    [`Config`], and emits a validated [`BuildPlan`].
//!
//! A backend (see `autopack-dockerfile`) then lowers the plan into something
//! executable. Keeping the plan as the interface means provider knowledge is
//! written once and reused by every backend.
//!
//! ```no_run
//! use autopack_core::{analyze, App, Environment, ProviderRegistry};
//!
//! let app = App::new(".")?;
//! let env = Environment::from_process();
//! let registry = ProviderRegistry::new(); // autopack-providers::registry() in practice
//! let analysis = analyze(&app, &env, &registry)?;
//! println!("{}", analysis.plan.to_json()?);
//! # Ok::<(), autopack_core::Error>(())
//! ```

#![deny(missing_docs)]

pub mod app;
pub mod compat;
pub mod config;
pub mod env;
pub mod error;
pub mod generate;
pub mod lock;
pub mod mise;
pub mod plan;
pub mod procfile;
pub mod provider;

pub use app::App;
pub use compat::ConfigSource;
pub use config::Config;
pub use env::Environment;
pub use error::{Error, Result};
pub use generate::{BuildContext, APP_DIR};
pub use lock::Lock;
pub use mise::{MisePackages, PackageRequest};
pub use plan::BuildPlan;
pub use procfile::Procfile;
pub use provider::{Provider, ProviderRegistry};

use indexmap::IndexMap;

/// Names of the steps autopack manages.
///
/// Providers must use these names for the install and build phases so that
/// `AUTOPACK_INSTALL_CMD` / `AUTOPACK_BUILD_CMD` and `autopack.json` overrides
/// land on the right step regardless of language.
pub mod steps {
    /// Installs mise and the language runtimes.
    pub const PACKAGES: &str = "packages";
    /// Installs project dependencies.
    pub const INSTALL: &str = "install";
    /// Compiles or bundles the app.
    pub const BUILD: &str = "build";
    /// The runtime base image.
    pub const RUNTIME: &str = "runtime";
}

/// The result of analysing an app.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Id of the provider that produced the plan.
    pub provider: String,
    /// The generated plan.
    pub plan: BuildPlan,
    /// Facts about the app, for `autopack info`.
    pub metadata: IndexMap<String, String>,
    /// Runtimes that will be installed, and where each version came from.
    pub packages: Vec<(String, PackageRequest)>,
}

/// Detect a provider for `app` and generate its build plan.
///
/// Configuration is read from `autopack.json` in the app root and from
/// `AUTOPACK_*` variables in `env`, with environment variables taking priority.
pub fn analyze(app: &App, env: &Environment, registry: &ProviderRegistry) -> Result<Analysis> {
    let loaded = Config::load_with_source(app, env)?;
    let config = loaded.config;
    let provider = registry.resolve(app, env, &config)?;

    let mut ctx = BuildContext::new(app, env, &config);
    if let Some(lock) = Lock::load(app.source())? {
        ctx.set_lock(lock);
    }
    ctx.add_metadata("provider", provider.id());

    // Every non-`web` Procfile process becomes a task. Doing it here rather
    // than in each provider means all of them get it, and a Procfile means the
    // same thing whatever language the app is written in.
    if let Some(procfile) = Procfile::load(app)? {
        for (name, command) in procfile.tasks() {
            ctx.add_task(name, command);
        }
    }
    if let Some(file) = loaded.source.file() {
        ctx.add_metadata(
            "config",
            if loaded.source.is_compat() {
                format!("{file} (compatibility mode)")
            } else {
                file.to_string()
            },
        );
    }
    // Anything a compatibility translation could not carry over is surfaced
    // rather than dropped: a silently-ignored setting shows up as a build that
    // behaves differently for no visible reason.
    for (index, note) in loaded.notes.iter().enumerate() {
        ctx.add_metadata(format!("configNote{}", index + 1), note);
        tracing::warn!(
            "{file}: {note}",
            file = loaded.source.file().unwrap_or("config")
        );
    }
    provider.plan(&mut ctx)?;
    let plan = ctx.generate()?;

    let packages = ctx
        .packages
        .iter()
        .map(|(name, request)| (name.to_string(), request.clone()))
        .collect();

    Ok(Analysis {
        provider: provider.id().to_string(),
        plan,
        metadata: ctx.metadata.clone(),
        packages,
    })
}
