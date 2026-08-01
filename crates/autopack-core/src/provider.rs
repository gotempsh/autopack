//! The provider interface and registry.

use crate::app::App;
use crate::config::Config;
use crate::env::Environment;
use crate::error::{Error, Result};
use crate::generate::BuildContext;

/// Language- or framework-specific build knowledge.
///
/// A provider does two things: decide whether it recognises an app, and, if so,
/// describe the build. Detection must be cheap and side-effect free — every
/// registered provider is asked, in registration order, until one says yes.
pub trait Provider: Send + Sync {
    /// Stable identifier, used by `AUTOPACK_PROVIDER` and `autopack.json`.
    fn id(&self) -> &'static str;

    /// Human readable name for build output.
    fn display_name(&self) -> &'static str {
        self.id()
    }

    /// Whether this provider recognises the app.
    fn detect(&self, app: &App, env: &Environment) -> Result<bool>;

    /// Fill in the build plan for an app this provider detected.
    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()>;
}

/// An ordered set of providers.
///
/// Order is significant: the first provider whose `detect` returns true wins,
/// so more specific providers must be registered before more general ones.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Box<dyn Provider>>,
}

impl ProviderRegistry {
    /// A registry with no providers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a provider.
    pub fn register(&mut self, provider: Box<dyn Provider>) -> &mut Self {
        self.providers.push(provider);
        self
    }

    /// Registered provider ids, in detection order.
    pub fn ids(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.id()).collect()
    }

    /// Look up a provider by id.
    pub fn get(&self, id: &str) -> Option<&dyn Provider> {
        self.providers
            .iter()
            .find(|p| p.id() == id)
            .map(AsRef::as_ref)
    }

    /// The first provider that recognises `app`.
    pub fn detect(&self, app: &App, env: &Environment) -> Result<Option<&dyn Provider>> {
        for provider in &self.providers {
            if provider.detect(app, env)? {
                tracing::debug!(provider = provider.id(), "provider matched");
                return Ok(Some(provider.as_ref()));
            }
        }
        Ok(None)
    }

    /// Resolve the provider to use: the configured one if set, else detection.
    pub fn resolve(&self, app: &App, env: &Environment, config: &Config) -> Result<&dyn Provider> {
        if let Some(name) = config.provider.as_deref() {
            return self.get(name).ok_or_else(|| Error::UnknownProvider {
                name: name.to_string(),
                available: self.ids().join(", "),
            });
        }

        self.detect(app, env)?.ok_or(Error::NoProviderDetected)
    }

    /// True when no providers are registered.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Command;
    use crate::steps;

    struct Always(&'static str);

    impl Provider for Always {
        fn id(&self) -> &'static str {
            self.0
        }
        fn detect(&self, _app: &App, _env: &Environment) -> Result<bool> {
            Ok(true)
        }
        fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
            ctx.step(steps::BUILD).add_command(Command::shell("true"));
            Ok(())
        }
    }

    struct Never;

    impl Provider for Never {
        fn id(&self) -> &'static str {
            "never"
        }
        fn detect(&self, _app: &App, _env: &Environment) -> Result<bool> {
            Ok(false)
        }
        fn plan(&self, _ctx: &mut BuildContext<'_>) -> Result<()> {
            Ok(())
        }
    }

    fn app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path()).unwrap();
        (dir, app)
    }

    #[test]
    fn first_match_wins() {
        let (_dir, app) = app();
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(Never));
        registry.register(Box::new(Always("first")));
        registry.register(Box::new(Always("second")));

        let provider = registry.detect(&app, &Environment::new()).unwrap().unwrap();
        assert_eq!(provider.id(), "first");
    }

    #[test]
    fn configured_provider_skips_detection() {
        let (_dir, app) = app();
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(Always("first")));
        registry.register(Box::new(Always("second")));

        let config = Config {
            provider: Some("second".into()),
            ..Default::default()
        };
        let provider = registry
            .resolve(&app, &Environment::new(), &config)
            .unwrap();
        assert_eq!(provider.id(), "second");
    }

    #[test]
    fn unknown_configured_provider_lists_alternatives() {
        let (_dir, app) = app();
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(Always("node")));

        let config = Config {
            provider: Some("nodejs".into()),
            ..Default::default()
        };
        let err = registry
            .resolve(&app, &Environment::new(), &config)
            .map(|p| p.id())
            .unwrap_err();
        assert!(err.to_string().contains("node"), "{err}");
    }

    #[test]
    fn no_match_is_an_actionable_error() {
        let (_dir, app) = app();
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(Never));

        let err = registry
            .resolve(&app, &Environment::new(), &Config::default())
            .map(|p| p.id())
            .unwrap_err();
        assert!(err.to_string().contains("AUTOPACK_PROVIDER"), "{err}");
    }
}
