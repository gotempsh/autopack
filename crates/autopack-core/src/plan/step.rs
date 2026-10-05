//! Build steps.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::{Command, Layer};

/// One unit of work in the build graph.
///
/// A step starts from its first input, overlays the remaining inputs, then runs
/// its commands. Steps with no dependency on each other can be executed in
/// parallel by a backend that supports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Unique name within the plan. Referenced by [`Layer::step`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,

    /// Filesystem inputs. The first is the base; later inputs are overlaid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<Layer>,

    /// Commands run in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Command>,

    /// Secret names this step is allowed to read. `["*"]` means all secrets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,

    /// Build argument names this step declares as `ARG`s. `["*"]` means all.
    ///
    /// Empty unless a step opts in, unlike [`Step::secrets`]: a build
    /// argument's value is part of the cache key of every `RUN` after it, so
    /// declaring one in a dependency install would re-run the install whenever
    /// the value changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build_args: Vec<String>,

    /// Inline file contents addressed by [`super::FileCommand::name`].
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub assets: IndexMap<String, String>,

    /// Environment variables set for every command in the step.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, String>,

    /// Names of plan-level caches mounted for every command in the step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caches: Vec<String>,
}

impl Step {
    /// A step named `name` with access to every secret.
    ///
    /// Secrets default to `["*"]` to match the "just works" expectation of a
    /// zero-config builder; narrow them per step when a build should not see
    /// production credentials.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            secrets: vec!["*".to_string()],
            ..Default::default()
        }
    }

    /// True when this step reads every available secret.
    pub fn uses_all_secrets(&self) -> bool {
        self.secrets.iter().any(|s| s == "*")
    }

    /// Give the build step every build argument, unless it already says otherwise.
    ///
    /// The build step is where a framework reads `VITE_*` or `NEXT_PUBLIC_*`
    /// and inlines it, and it runs after dependencies are installed, so a
    /// changed value only re-runs the build. Other steps opt in explicitly.
    pub(crate) fn grant_default_build_args(&mut self) {
        if self.name == crate::steps::BUILD && self.build_args.is_empty() {
            self.build_args = vec!["*".to_string()];
        }
    }

    /// True when this step declares every build argument.
    pub fn uses_all_build_args(&self) -> bool {
        self.build_args.iter().any(|name| name == "*")
    }

    /// Append an input layer.
    pub fn add_input(&mut self, layer: Layer) -> &mut Self {
        self.inputs.push(layer);
        self
    }

    /// Append a command.
    pub fn add_command(&mut self, command: Command) -> &mut Self {
        self.commands.push(command);
        self
    }

    /// Append several commands.
    pub fn add_commands<I: IntoIterator<Item = Command>>(&mut self, commands: I) -> &mut Self {
        self.commands.extend(commands);
        self
    }

    /// Register an inline asset. Returns the key to use in a file command.
    pub fn add_asset(&mut self, name: impl Into<String>, contents: impl Into<String>) -> String {
        let name = name.into();
        self.assets.insert(name.clone(), contents.into());
        name
    }

    /// Set an environment variable for the step.
    pub fn add_variable(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.variables.insert(key.into(), value.into());
        self
    }

    /// Mount a plan-level cache in this step.
    pub fn add_cache(&mut self, name: impl Into<String>) -> &mut Self {
        let name = name.into();
        if !self.caches.contains(&name) {
            self.caches.push(name);
        }
        self
    }

    /// Step names this step depends on.
    pub fn dependencies(&self) -> Vec<&str> {
        self.inputs
            .iter()
            .filter_map(|input| input.step.as_deref())
            .collect()
    }
}
