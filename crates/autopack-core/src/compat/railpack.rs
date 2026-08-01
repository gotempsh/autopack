//! Reading `railpack.json`.
//!
//! autopack's plan schema was modelled on Railpack's, so this translation is
//! mostly a rename: the two configs describe the same shapes with two fields
//! spelled differently. That is deliberate — a repository already configured
//! for Railpack should not have to be re-specified to build here.

use indexmap::IndexMap;
use serde::Deserialize;

use crate::config::{Config, DeployPatch, StepPatch};
use crate::plan::{Cache, Command, Layer};

/// File name, relative to the app root.
pub const FILE: &str = "railpack.json";

/// The subset of `railpack.json` that has an equivalent here.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RailpackConfig {
    /// Forces a specific provider.
    #[serde(default)]
    pub provider: Option<String>,

    /// Railpack's name for build-time apt packages.
    #[serde(default)]
    pub build_apt_packages: Vec<String>,

    /// Language runtimes to install.
    #[serde(default)]
    pub packages: IndexMap<String, String>,

    /// Cache definitions, keyed by name.
    #[serde(default)]
    pub caches: IndexMap<String, Cache>,

    /// Per-step overrides, keyed by step name.
    #[serde(default)]
    pub steps: IndexMap<String, RailpackStep>,

    /// Runtime image configuration.
    #[serde(default)]
    pub deploy: Option<RailpackDeploy>,

    /// Secret names the build should receive.
    #[serde(default)]
    pub secrets: Vec<String>,

    /// Paths excluded from the build context.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// A step definition. The command grammar is shared, so it deserialises with
/// autopack's own [`Command`].
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RailpackStep {
    /// Filesystem inputs for this step.
    #[serde(default)]
    pub inputs: Option<Vec<Layer>>,
    /// Commands to run in this step.
    #[serde(default)]
    pub commands: Option<Vec<Command>>,
    /// Secret names this step may read.
    #[serde(default)]
    pub secrets: Option<Vec<String>>,
    /// Inline file contents addressed by a file command.
    #[serde(default)]
    pub assets: IndexMap<String, String>,
    /// Environment variables for this scope.
    #[serde(default)]
    pub variables: IndexMap<String, String>,
    /// Names of caches mounted in this step.
    #[serde(default)]
    pub caches: Option<Vec<String>>,
}

/// The `deploy` object.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RailpackDeploy {
    /// Railpack's name for runtime apt packages.
    #[serde(default)]
    pub apt_packages: Vec<String>,
    /// Base layer for the runtime image.
    #[serde(default)]
    pub base: Option<Layer>,
    /// Filesystem inputs for this step.
    #[serde(default)]
    pub inputs: Option<Vec<Layer>>,
    /// Command the container runs.
    #[serde(default)]
    pub start_command: Option<String>,
    /// Environment variables for this scope.
    #[serde(default)]
    pub variables: IndexMap<String, String>,
    /// Directories prepended to the runtime PATH.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl RailpackConfig {
    /// Translate into an autopack [`Config`].
    pub fn to_config(&self) -> Config {
        let mut config = Config {
            provider: self.provider.as_ref().map(|p| normalize_provider(p)),
            packages: self.packages.clone(),
            // The only real difference: Railpack splits build and runtime apt
            // packages across two differently-named fields.
            apt_packages: self.build_apt_packages.clone(),
            caches: self.caches.clone(),
            secrets: self.secrets.clone(),
            exclude: self.exclude.clone(),
            ..Default::default()
        };

        for (name, step) in &self.steps {
            config.steps.insert(
                name.clone(),
                StepPatch {
                    inputs: step.inputs.clone(),
                    commands: step.commands.clone(),
                    secrets: step.secrets.clone(),
                    assets: step.assets.clone(),
                    variables: step.variables.clone(),
                    caches: step.caches.clone(),
                },
            );
        }

        if let Some(deploy) = &self.deploy {
            config.deploy_apt_packages = deploy.apt_packages.clone();
            config.deploy = Some(DeployPatch {
                base: deploy.base.clone(),
                inputs: deploy.inputs.clone(),
                start_command: deploy.start_command.clone(),
                variables: deploy.variables.clone(),
                paths: deploy.paths.clone(),
                // Neither Railpack nor Nixpacks has a task concept; tasks come
                // from a Procfile or autopack.json.
                tasks: IndexMap::new(),
            });
        }

        config
    }
}

/// Railpack provider ids that differ from autopack's.
fn normalize_provider(provider: &str) -> String {
    match provider {
        "golang" => "go".to_string(),
        "staticfile" => "static".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Config {
        let parsed: RailpackConfig = serde_json::from_str(json).unwrap();
        parsed.to_config()
    }

    #[test]
    fn apt_package_fields_land_on_the_right_side() {
        // Getting this backwards puts build headers in the runtime image and
        // leaves the runtime library out of it.
        let config =
            parse(r#"{"buildAptPackages":["libpq-dev"],"deploy":{"aptPackages":["libpq5"]}}"#);
        assert_eq!(config.apt_packages, vec!["libpq-dev"]);
        assert_eq!(config.deploy_apt_packages, vec!["libpq5"]);
    }

    #[test]
    fn steps_and_commands_carry_across_unchanged() {
        let config = parse(r#"{"steps":{"build":{"commands":["npm run build"]}}}"#);
        assert_eq!(
            config.steps["build"].commands.as_ref().unwrap()[0].display_name(),
            "npm run build"
        );
    }

    #[test]
    fn deploy_settings_translate() {
        let config = parse(
            r#"{"deploy":{"startCommand":"node server.js","variables":{"NODE_ENV":"production"},"paths":["/app/bin"]}}"#,
        );
        let deploy = config.deploy.unwrap();
        assert_eq!(deploy.start_command.as_deref(), Some("node server.js"));
        assert_eq!(deploy.variables["NODE_ENV"], "production");
        assert_eq!(deploy.paths, vec!["/app/bin"]);
    }

    #[test]
    fn packages_caches_and_secrets_translate() {
        let config = parse(
            r#"{"packages":{"node":"22"},"caches":{"npm":{"directory":"/cache/npm"}},"secrets":["DATABASE_URL"]}"#,
        );
        assert_eq!(config.packages["node"], "22");
        assert_eq!(config.caches["npm"].directory, "/cache/npm");
        assert_eq!(config.secrets, vec!["DATABASE_URL"]);
    }

    #[test]
    fn provider_ids_are_normalised() {
        assert_eq!(
            parse(r#"{"provider":"golang"}"#).provider.as_deref(),
            Some("go")
        );
    }

    #[test]
    fn unknown_fields_do_not_break_the_read() {
        // Railpack can add fields; a repository configured for a newer version
        // should still build rather than fail to parse.
        let config = parse(r#"{"provider":"node","someFutureField":{"a":1}}"#);
        assert_eq!(config.provider.as_deref(), Some("node"));
    }
}
