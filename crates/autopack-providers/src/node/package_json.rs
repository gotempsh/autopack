//! `package.json` parsing, limited to the fields providers act on.

use indexmap::IndexMap;
use serde::Deserialize;

/// The parts of `package.json` autopack reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PackageJson {
    /// Package name.
    #[serde(default)]
    pub name: Option<String>,

    /// CommonJS entry point.
    #[serde(default)]
    pub main: Option<String>,

    /// `"module"` marks the package as ESM.
    #[serde(default, rename = "type")]
    pub module_type: Option<String>,

    /// npm scripts.
    #[serde(default)]
    pub scripts: IndexMap<String, String>,

    /// Runtime dependencies.
    #[serde(default)]
    pub dependencies: IndexMap<String, String>,

    /// Development dependencies.
    #[serde(default, rename = "devDependencies")]
    pub dev_dependencies: IndexMap<String, String>,

    /// Corepack-style `"pnpm@9.1.0"` pin.
    #[serde(default, rename = "packageManager")]
    pub package_manager: Option<String>,

    /// Engine constraints, notably `engines.node`.
    #[serde(default)]
    pub engines: IndexMap<String, String>,

    /// npm/yarn/bun workspace globs. Present as either an array or an object.
    #[serde(default)]
    pub workspaces: Option<serde_json::Value>,
}

impl PackageJson {
    /// True when `name` appears in dependencies or devDependencies.
    pub fn has_dependency(&self, name: &str) -> bool {
        self.dependencies.contains_key(name) || self.dev_dependencies.contains_key(name)
    }

    /// True when any of `names` appears in dependencies or devDependencies.
    pub fn has_any_dependency(&self, names: &[&str]) -> bool {
        names.iter().any(|name| self.has_dependency(name))
    }

    /// The body of an npm script, if defined and non-empty.
    pub fn script(&self, name: &str) -> Option<&str> {
        self.scripts
            .get(name)
            .map(String::as_str)
            .map(str::trim)
            .filter(|script| !script.is_empty())
    }

    /// True when this package declares workspaces.
    pub fn has_workspaces(&self) -> bool {
        match &self.workspaces {
            Some(serde_json::Value::Array(globs)) => !globs.is_empty(),
            Some(serde_json::Value::Object(map)) => !map.is_empty(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_unknown_fields() {
        let json = r#"{"name":"demo","unknown":{"a":1},"scripts":{"build":"vite build"}}"#;
        let package: PackageJson = serde_json::from_str(json).unwrap();
        assert_eq!(package.name.as_deref(), Some("demo"));
        assert_eq!(package.script("build"), Some("vite build"));
    }

    #[test]
    fn empty_scripts_read_as_absent() {
        let package: PackageJson = serde_json::from_str(r#"{"scripts":{"build":"  "}}"#).unwrap();
        assert_eq!(package.script("build"), None);
    }

    #[test]
    fn workspaces_detected_in_both_shapes() {
        let array: PackageJson = serde_json::from_str(r#"{"workspaces":["packages/*"]}"#).unwrap();
        let object: PackageJson =
            serde_json::from_str(r#"{"workspaces":{"packages":["apps/*"]}}"#).unwrap();
        let none: PackageJson = serde_json::from_str(r#"{"workspaces":[]}"#).unwrap();

        assert!(array.has_workspaces());
        assert!(object.has_workspaces());
        assert!(!none.has_workspaces());
    }
}
