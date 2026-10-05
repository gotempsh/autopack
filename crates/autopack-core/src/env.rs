//! Environment variables visible to detection and plan generation.

use indexmap::IndexMap;

/// Prefix for variables that configure autopack itself rather than the app.
pub const CONFIG_PREFIX: &str = "AUTOPACK_";

/// Build-time environment.
///
/// Two distinct things live here:
///
/// * **variables** — values autopack may read (framework env vars, `NODE_ENV`,
///   and `AUTOPACK_*` configuration). These end up in the plan.
/// * **secrets** — names only. Values are supplied to the backend at build time
///   and never serialised into a plan, so a plan is safe to log or cache.
/// * **build args** — names only, like secrets, but declared as `ARG`s the
///   build step reads as ordinary environment variables. For values a
///   framework inlines into its output (`VITE_*`, `NEXT_PUBLIC_*`), not for
///   credentials: a build argument a `RUN` reads is recorded in the build
///   stage's image history.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    variables: IndexMap<String, String>,
    secrets: Vec<String>,
    build_args: Vec<String>,
}

impl Environment {
    /// An empty environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an environment from key/value pairs.
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut env = Self::new();
        for (key, value) in pairs {
            env.set(key, value);
        }
        env
    }

    /// Capture the current process environment.
    pub fn from_process() -> Self {
        Self::from_pairs(std::env::vars())
    }

    /// Set a variable.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.variables.insert(key.into(), value.into());
        self
    }

    /// Look up a variable.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.variables.get(key).map(String::as_str)
    }

    /// True when the variable is set to a truthy value (`1`, `true`, `yes`, `on`).
    pub fn is_enabled(&self, key: &str) -> bool {
        matches!(
            self.get(key)
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("1" | "true" | "yes" | "on")
        )
    }

    /// Read an `AUTOPACK_`-prefixed setting by its unprefixed name.
    ///
    /// `env.config("BUILD_CMD")` reads `AUTOPACK_BUILD_CMD`.
    pub fn config(&self, name: &str) -> Option<&str> {
        self.get(&format!("{CONFIG_PREFIX}{name}"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// True when `key` configures autopack rather than the app.
    pub fn is_config_variable(key: &str) -> bool {
        key.starts_with(CONFIG_PREFIX)
    }

    /// Variables belonging to the app, i.e. everything that is not `AUTOPACK_*`.
    pub fn app_variables(&self) -> impl Iterator<Item = (&str, &str)> {
        self.variables
            .iter()
            .filter(|(key, _)| !Self::is_config_variable(key))
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Declare a secret by name.
    pub fn add_secret(&mut self, name: impl Into<String>) -> &mut Self {
        let name = name.into();
        if !self.secrets.contains(&name) {
            self.secrets.push(name);
        }
        self
    }

    /// Declared secret names.
    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }

    /// Declare a build argument by name.
    pub fn add_build_arg(&mut self, name: impl Into<String>) -> &mut Self {
        let name = name.into();
        if !self.build_args.contains(&name) {
            self.build_args.push(name);
        }
        self
    }

    /// Declared build argument names.
    pub fn build_args(&self) -> &[String] {
        &self.build_args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_lookup_uses_the_prefix() {
        let env = Environment::from_pairs([("AUTOPACK_BUILD_CMD", "make")]);
        assert_eq!(env.config("BUILD_CMD"), Some("make"));
        assert_eq!(env.config("START_CMD"), None);
    }

    #[test]
    fn blank_config_values_read_as_unset() {
        let env = Environment::from_pairs([("AUTOPACK_BUILD_CMD", "   ")]);
        assert_eq!(env.config("BUILD_CMD"), None);
    }

    #[test]
    fn app_variables_exclude_autopack_settings() {
        let env =
            Environment::from_pairs([("AUTOPACK_PROVIDER", "node"), ("NODE_ENV", "production")]);
        let app: Vec<_> = env.app_variables().collect();
        assert_eq!(app, vec![("NODE_ENV", "production")]);
    }

    #[test]
    fn build_args_are_declared_once() {
        let mut env = Environment::new();
        env.add_build_arg("VITE_API_URL")
            .add_build_arg("VITE_API_URL");
        assert_eq!(env.build_args(), ["VITE_API_URL"]);
        // Declaring a name is not setting a value.
        assert_eq!(env.get("VITE_API_URL"), None);
    }
}
