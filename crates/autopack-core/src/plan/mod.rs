//! The build plan: autopack's intermediate representation.
//!
//! Providers never emit a Dockerfile or BuildKit graph directly. They emit a
//! [`BuildPlan`] — a serialisable description of steps, layers, caches and the
//! resulting runtime image — which a backend then lowers. That split is what
//! lets the same analysis drive a Dockerfile, a BuildKit frontend, or a remote
//! builder without rewriting provider logic.

mod cache;
mod command;
mod deploy;
mod filter;
mod layer;
mod step;

pub use cache::{Cache, CacheType};
pub use command::{Command, CopyCommand, ExecCommand, FileCommand, PathCommand};
pub use deploy::{Deploy, RuntimeUser};
pub use filter::Filter;
pub use layer::{Layer, LayerSource};
pub use step::Step;

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A complete, backend-agnostic description of how to build and run an app.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPlan {
    /// Build steps, in the order providers created them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,

    /// Cache definitions referenced by name from steps.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub caches: IndexMap<String, Cache>,

    /// Secret names the build expects to be supplied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,

    /// The runtime image.
    pub deploy: Deploy,

    /// Paths never uploaded into the build context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,

    /// Token mixed into every cache mount id, isolating this app's caches
    /// from other projects built on the same worker. `None` shares them,
    /// which is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<String>,
}

impl BuildPlan {
    /// An empty plan.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a plan from JSON.
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// Render the plan as pretty-printed JSON.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Add a step, replacing any existing step with the same name.
    pub fn add_step(&mut self, step: Step) {
        match self.steps.iter_mut().find(|s| s.name == step.name) {
            Some(existing) => *existing = step,
            None => self.steps.push(step),
        }
    }

    /// Look up a step by name.
    pub fn step(&self, name: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.name == name)
    }

    /// Register a cache and return its name, for use in [`Step::add_cache`].
    pub fn add_cache(&mut self, name: impl Into<String>, cache: Cache) -> String {
        let name = name.into();
        self.caches.insert(name.clone(), cache);
        name
    }

    /// Drop empty layers and prune steps the deploy image cannot reach.
    ///
    /// Providers add steps optimistically (a build step is created before we
    /// know whether the app has a build command). Normalizing afterwards keeps
    /// the plan honest without every provider having to unwind its own work.
    pub fn normalize(&mut self) {
        for step in &mut self.steps {
            step.inputs.retain(|input| !input.is_empty());
        }
        self.deploy.inputs.retain(|input| !input.is_empty());

        let mut reachable: HashSet<String> = HashSet::new();
        let mut queue: Vec<String> = self
            .deploy
            .roots()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();

        while let Some(name) = queue.pop() {
            if !reachable.insert(name.clone()) {
                continue;
            }
            if let Some(step) = self.step(&name) {
                for dependency in step.dependencies() {
                    if !reachable.contains(dependency) {
                        queue.push(dependency.to_string());
                    }
                }
            }
        }

        if !reachable.is_empty() {
            self.steps.retain(|step| reachable.contains(&step.name));
        }

        let used_caches: HashSet<&String> =
            self.steps.iter().flat_map(|step| &step.caches).collect();
        self.caches
            .retain(|name, _| used_caches.contains(&name.to_string()));
    }

    /// Check the plan is executable: unique step names, no dangling references,
    /// no cycles, and every referenced cache exists.
    pub fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for step in &self.steps {
            if step.name.is_empty() {
                return Err(Error::InvalidPlan("a step has an empty name".into()));
            }
            if !seen.insert(step.name.as_str()) {
                return Err(Error::InvalidPlan(format!(
                    "duplicate step name `{}`",
                    step.name
                )));
            }
        }

        for step in &self.steps {
            for dependency in step.dependencies() {
                if !seen.contains(dependency) {
                    return Err(Error::InvalidPlan(format!(
                        "step `{}` depends on unknown step `{}`",
                        step.name, dependency
                    )));
                }
            }
            for cache in &step.caches {
                if !self.caches.contains_key(cache) {
                    return Err(Error::InvalidPlan(format!(
                        "step `{}` uses undefined cache `{}`",
                        step.name, cache
                    )));
                }
            }
        }

        for root in self.deploy.roots() {
            if !seen.contains(root) {
                return Err(Error::InvalidPlan(format!(
                    "deploy references unknown step `{root}`"
                )));
            }
        }

        // Cycle detection doubles as the topological sort backends need.
        self.execution_order()?;
        Ok(())
    }

    /// Steps in dependency order: every step appears after the steps it reads from.
    ///
    /// Returns [`Error::InvalidPlan`] when the graph contains a cycle.
    pub fn execution_order(&self) -> Result<Vec<&Step>> {
        let index: HashMap<&str, usize> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| (step.name.as_str(), i))
            .collect();

        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Unvisited,
            InProgress,
            Done,
        }

        let mut marks = vec![Mark::Unvisited; self.steps.len()];
        let mut order = Vec::with_capacity(self.steps.len());

        // Iterative DFS so a deeply nested plan cannot blow the stack.
        for start in 0..self.steps.len() {
            if marks[start] != Mark::Unvisited {
                continue;
            }
            let mut stack = vec![(start, false)];
            while let Some((current, children_done)) = stack.pop() {
                if children_done {
                    marks[current] = Mark::Done;
                    order.push(&self.steps[current]);
                    continue;
                }
                match marks[current] {
                    Mark::Done => continue,
                    Mark::InProgress => {
                        return Err(Error::InvalidPlan(format!(
                            "step `{}` is part of a dependency cycle",
                            self.steps[current].name
                        )))
                    }
                    Mark::Unvisited => {}
                }
                marks[current] = Mark::InProgress;
                stack.push((current, true));
                for dependency in self.steps[current].dependencies() {
                    if let Some(&next) = index.get(dependency) {
                        if marks[next] == Mark::Unvisited {
                            stack.push((next, false));
                        } else if marks[next] == Mark::InProgress {
                            return Err(Error::InvalidPlan(format!(
                                "step `{}` is part of a dependency cycle",
                                self.steps[next].name
                            )));
                        }
                    }
                }
            }
        }

        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_with_chain() -> BuildPlan {
        let mut plan = BuildPlan::new();

        let mut install = Step::new("install");
        install.add_input(Layer::image("debian:bookworm-slim"));

        let mut build = Step::new("build");
        build.add_input(Layer::step("install"));

        let mut orphan = Step::new("orphan");
        orphan.add_input(Layer::image("alpine"));

        plan.add_step(install);
        plan.add_step(build);
        plan.add_step(orphan);
        plan.deploy.base = Layer::step("build");
        plan
    }

    #[test]
    fn normalize_prunes_unreachable_steps() {
        let mut plan = plan_with_chain();
        plan.normalize();
        let names: Vec<_> = plan.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["install", "build"]);
    }

    #[test]
    fn execution_order_is_topological() {
        let plan = plan_with_chain();
        let order: Vec<_> = plan
            .execution_order()
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let install = order.iter().position(|n| *n == "install").unwrap();
        let build = order.iter().position(|n| *n == "build").unwrap();
        assert!(install < build, "{order:?}");
    }

    #[test]
    fn cycles_are_rejected() {
        let mut plan = BuildPlan::new();
        let mut a = Step::new("a");
        a.add_input(Layer::step("b"));
        let mut b = Step::new("b");
        b.add_input(Layer::step("a"));
        plan.add_step(a);
        plan.add_step(b);
        plan.deploy.base = Layer::step("a");

        let err = plan.validate().unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
    }

    #[test]
    fn dangling_step_references_are_rejected() {
        let mut plan = BuildPlan::new();
        let mut a = Step::new("a");
        a.add_input(Layer::step("missing"));
        plan.add_step(a);
        plan.deploy.base = Layer::step("a");

        let err = plan.validate().unwrap_err();
        assert!(err.to_string().contains("unknown step `missing`"), "{err}");
    }

    #[test]
    fn plans_round_trip_through_json() {
        let mut plan = plan_with_chain();
        plan.normalize();
        let json = plan.to_json().unwrap();
        assert_eq!(BuildPlan::from_json(&json).unwrap(), plan);
    }
}
