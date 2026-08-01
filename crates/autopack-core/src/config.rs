//! User configuration: `autopack.json` and `AUTOPACK_*` environment variables.
//!
//! Configuration is applied *after* providers have generated a plan, so a user
//! never has to reproduce the parts of the build they were happy with. The
//! merge is deliberately shallow and predictable: named fields replace, maps
//! extend, and command lists may splice the generated commands back in with the
//! `"..."` sentinel.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::env::Environment;
use crate::error::{Error, Result};
use crate::plan::{BuildPlan, Cache, Command, Layer, Step};
use crate::steps;

/// Default configuration file name, relative to the app root.
pub const DEFAULT_CONFIG_FILE: &str = "autopack.json";

/// Sentinel used inside a config `commands` array to keep generated commands.
pub const SPREAD: &str = "...";

/// Contents of `autopack.json`, plus anything derived from `AUTOPACK_*`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// Force a specific provider instead of running detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    /// Language runtimes to install, e.g. `{"node": "22", "python": "3.12"}`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub packages: IndexMap<String, String>,

    /// Debian packages installed in the build image.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apt_packages: Vec<String>,

    /// Debian packages installed in the runtime image.
    ///
    /// Separate from [`Config::apt_packages`] because a build dependency in
    /// the runtime image is dead weight, and a runtime dependency missing from
    /// it is a container that starts and immediately dies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deploy_apt_packages: Vec<String>,

    /// Extra cache definitions, merged into the plan's caches.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub caches: IndexMap<String, Cache>,

    /// Per-step overrides, keyed by step name.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub steps: IndexMap<String, StepPatch>,

    /// Runtime image overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<DeployPatch>,

    /// Additional secret names the build should receive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,

    /// Additional paths excluded from the build context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

/// Overrides for a single step. Absent fields leave the generated value alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepPatch {
    /// Replaces the step's inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<Layer>>,

    /// Replaces the step's commands. Include `"..."` to splice in the
    /// generated commands at that position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<Command>>,

    /// Replaces the step's secret allowlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<String>>,

    /// Merged into the step's assets.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub assets: IndexMap<String, String>,

    /// Merged into the step's variables.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, String>,

    /// Replaces the step's cache list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caches: Option<Vec<String>>,
}

/// Overrides for the runtime image.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeployPatch {
    /// Replaces the runtime base layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Layer>,

    /// Replaces the layers copied into the runtime image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<Layer>>,

    /// Replaces the container start command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_command: Option<String>,

    /// Merged into the runtime environment.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, String>,

    /// Appended to the runtime `PATH`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,

    /// Merged into the tasks the platform can run, keyed by process name.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub tasks: IndexMap<String, String>,
}

impl Config {
    /// Load configuration for `app`, layering `AUTOPACK_*` variables on top of
    /// the config file. Environment variables win on conflict.
    pub fn load(app: &App, env: &Environment) -> Result<Self> {
        Ok(Self::load_with_source(app, env)?.config)
    }

    /// Load configuration and report which file it came from.
    ///
    /// A `railpack.json` or `nixpacks.toml` is read when no `autopack.json`
    /// exists, so an application already configured for another builder keeps
    /// building the way its author intended. Set `AUTOPACK_COMPAT=off` to
    /// ignore those files.
    pub fn load_with_source(app: &App, env: &Environment) -> Result<crate::compat::Loaded> {
        // An explicit AUTOPACK_CONFIG_FILE bypasses discovery entirely.
        if let Some(file_name) = env.config("CONFIG_FILE") {
            let mut config = match app.read_file_opt(file_name)? {
                Some(contents) => {
                    serde_json::from_str(&contents).map_err(|e| Error::ParseFile {
                        path: file_name.into(),
                        message: e.to_string(),
                    })?
                }
                None => Config::default(),
            };
            config.apply_environment(env);
            return Ok(crate::compat::Loaded {
                config,
                source: crate::compat::ConfigSource::Autopack,
                notes: Vec::new(),
            });
        }

        let compat_enabled = !matches!(
            env.config("COMPAT").map(str::to_ascii_lowercase).as_deref(),
            Some("off" | "0" | "false" | "no")
        );

        let mut loaded = crate::compat::load(app, compat_enabled)?;
        loaded.config.apply_environment(env);
        Ok(loaded)
    }

    /// Fold `AUTOPACK_*` settings into this config.
    pub fn apply_environment(&mut self, env: &Environment) {
        if let Some(provider) = env.config("PROVIDER") {
            self.provider = Some(provider.to_string());
        }

        // AUTOPACK_PACKAGES="node@22 python@3.12"
        if let Some(packages) = env.config("PACKAGES") {
            for package in packages.split_whitespace() {
                let (name, version) = package.split_once('@').unwrap_or((package, "latest"));
                self.packages.insert(name.to_string(), version.to_string());
            }
        }

        if let Some(packages) = env.config("APT_PACKAGES") {
            self.apt_packages
                .extend(packages.split_whitespace().map(str::to_string));
        }

        if let Some(packages) = env.config("DEPLOY_APT_PACKAGES") {
            self.deploy_apt_packages
                .extend(packages.split_whitespace().map(str::to_string));
        }

        for (variable, step) in [("INSTALL_CMD", steps::INSTALL), ("BUILD_CMD", steps::BUILD)] {
            if let Some(cmd) = env.config(variable) {
                self.steps.entry(step.to_string()).or_default().commands =
                    Some(vec![Command::shell(cmd)]);
            }
        }

        if let Some(cmd) = env.config("RELEASE_CMD") {
            self.deploy
                .get_or_insert_with(DeployPatch::default)
                .tasks
                .insert(crate::procfile::RELEASE.to_string(), cmd.to_string());
        }

        if let Some(cmd) = env.config("START_CMD") {
            self.deploy
                .get_or_insert_with(DeployPatch::default)
                .start_command = Some(cmd.to_string());
        }
    }

    /// Apply this config to a generated plan.
    pub fn apply(&self, plan: &mut BuildPlan) {
        for (name, cache) in &self.caches {
            plan.caches.insert(name.clone(), cache.clone());
        }

        for name in &self.secrets {
            if !plan.secrets.contains(name) {
                plan.secrets.push(name.clone());
            }
        }

        for path in &self.exclude {
            if !plan.exclude.contains(path) {
                plan.exclude.push(path.clone());
            }
        }

        for (name, patch) in &self.steps {
            match plan.steps.iter_mut().find(|step| &step.name == name) {
                Some(step) => patch.apply(step),
                None => {
                    let mut step = Step::new(name);
                    patch.apply(&mut step);
                    plan.steps.push(step);
                }
            }
        }

        if let Some(deploy) = &self.deploy {
            deploy.apply(plan);
        }
    }
}

impl StepPatch {
    fn apply(&self, step: &mut Step) {
        if let Some(inputs) = &self.inputs {
            step.inputs = inputs.clone();
        }

        if let Some(commands) = &self.commands {
            step.commands = splice(commands, &step.commands);
        }

        if let Some(secrets) = &self.secrets {
            step.secrets = secrets.clone();
        }

        if let Some(caches) = &self.caches {
            step.caches = caches.clone();
        }

        for (key, value) in &self.assets {
            step.assets.insert(key.clone(), value.clone());
        }

        for (key, value) in &self.variables {
            step.variables.insert(key.clone(), value.clone());
        }
    }
}

impl DeployPatch {
    fn apply(&self, plan: &mut BuildPlan) {
        if let Some(base) = &self.base {
            plan.deploy.base = base.clone();
        }
        if let Some(inputs) = &self.inputs {
            plan.deploy.inputs = inputs.clone();
        }
        if let Some(start) = &self.start_command {
            plan.deploy.start_command = Some(start.clone());
        }
        for (key, value) in &self.variables {
            plan.deploy.variables.insert(key.clone(), value.clone());
        }
        for path in &self.paths {
            plan.deploy.add_path(path.clone());
        }
        for (name, command) in &self.tasks {
            plan.deploy.add_task(name.clone(), command.clone());
        }
    }
}

/// Replace `override_commands`' `"..."` markers with `generated`.
fn splice(override_commands: &[Command], generated: &[Command]) -> Vec<Command> {
    let mut result = Vec::with_capacity(override_commands.len() + generated.len());
    for command in override_commands {
        if is_spread(command) {
            result.extend(generated.iter().cloned());
        } else {
            result.push(command.clone());
        }
    }
    result
}

fn is_spread(command: &Command) -> bool {
    matches!(command, Command::Exec(exec) if exec.cmd == SPREAD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Deploy;

    fn plan_with_build() -> BuildPlan {
        let mut plan = BuildPlan::new();
        let mut build = Step::new(steps::BUILD);
        build.add_command(Command::shell("npm run build"));
        plan.add_step(build);
        plan.deploy = Deploy {
            base: Layer::step(steps::BUILD),
            start_command: Some("npm start".into()),
            ..Default::default()
        };
        plan
    }

    #[test]
    fn commands_replace_by_default() {
        let config: Config =
            serde_json::from_str(r#"{"steps":{"build":{"commands":["make"]}}}"#).unwrap();
        let mut plan = plan_with_build();
        config.apply(&mut plan);
        assert_eq!(
            plan.step("build").unwrap().commands,
            vec![Command::exec("make")]
        );
    }

    #[test]
    fn spread_keeps_generated_commands() {
        let config: Config =
            serde_json::from_str(r#"{"steps":{"build":{"commands":["...","make docs"]}}}"#)
                .unwrap();
        let mut plan = plan_with_build();
        config.apply(&mut plan);
        let commands = &plan.step("build").unwrap().commands;
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].display_name(), "npm run build");
        assert_eq!(commands[1], Command::exec("make docs"));
    }

    #[test]
    fn env_overrides_the_start_command() {
        let mut config = Config::default();
        config.apply_environment(&Environment::from_pairs([(
            "AUTOPACK_START_CMD",
            "./bin/server",
        )]));
        let mut plan = plan_with_build();
        config.apply(&mut plan);
        assert_eq!(plan.deploy.start_command.as_deref(), Some("./bin/server"));
    }

    #[test]
    fn env_packages_parse_name_and_version() {
        let mut config = Config::default();
        config.apply_environment(&Environment::from_pairs([(
            "AUTOPACK_PACKAGES",
            "node@22 python@3.12 go",
        )]));
        assert_eq!(config.packages["node"], "22");
        assert_eq!(config.packages["python"], "3.12");
        assert_eq!(config.packages["go"], "latest");
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        let err = serde_json::from_str::<Config>(r#"{"provder":"node"}"#).unwrap_err();
        assert!(err.to_string().contains("provder"), "{err}");
    }
}
