//! The runtime image description.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::Layer;

/// What the final image looks like and how the container starts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deploy {
    /// Base filesystem for the runtime image.
    #[serde(default)]
    pub base: Layer,

    /// Additional layers copied onto the base, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<Layer>,

    /// Command the container runs.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startCommand"
    )]
    pub start_command: Option<String>,

    /// Environment variables baked into the image.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, String>,

    /// Directories prepended to `PATH` in the runtime image.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,

    /// Whether to inherit the runtime base image's health probe.
    #[serde(default, skip_serializing_if = "Healthcheck::is_inherited")]
    pub healthcheck: Healthcheck,

    /// User the container runs as. `None` means root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<RuntimeUser>,

    /// Named one-off commands the platform runs against this image, keyed by
    /// process name — `release` before a deploy goes live, `worker` alongside
    /// it.
    ///
    /// They share the image, so nothing extra is built for them. Keeping them
    /// out of the start command is what stops a migration running once per
    /// replica per restart.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub tasks: IndexMap<String, String>,

    /// Environment variables the app cannot start without, which the
    /// platform has to create because no image can carry them: a framework
    /// secret (`SECRET_KEY_BASE`, `APP_KEY`) must be unique to the app and
    /// stable across deploys, and a public host is only known to the
    /// platform.
    ///
    /// A platform creates each one that the app's environment does not
    /// already define, once, and keeps it. Values the user sets always win.
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        rename = "generatedVariables"
    )]
    pub generated_variables: IndexMap<String, GeneratedVariable>,

    /// Places the app keeps data that has to outlive the container — a SQLite
    /// database, uploaded files — which a container's own filesystem loses on
    /// every redeploy.
    ///
    /// A platform that can mount persistent storage there should; one that
    /// cannot should tell the user before their data is lost, not after.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "persistentPaths"
    )]
    pub persistent_paths: Vec<PersistentPath>,
}

/// Policy for a health probe supplied by the runtime base image.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Healthcheck {
    /// Keep the base image's health probe, if one exists.
    #[default]
    Inherit,
    /// Remove the base image's probe; the host owns readiness checks.
    Disabled,
}

impl Healthcheck {
    fn is_inherited(&self) -> bool {
        matches!(self, Self::Inherit)
    }
}

/// One entry of [`Deploy::persistent_paths`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentPath {
    /// File or directory in the runtime image that holds the data.
    pub path: String,
    /// What is kept there and what happens without persistent storage,
    /// written for the person deploying the app.
    pub reason: String,
    /// Variables that move the data elsewhere when the environment sets any
    /// of them (`DATABASE_URL` pointing at a database server).
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "unlessSet")]
    pub unless_set: Vec<String>,
}

/// One entry of [`Deploy::generated_variables`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedVariable {
    /// How to produce the value.
    #[serde(flatten)]
    pub value: GeneratedValue,
    /// Variables that make this one unnecessary when the environment sets
    /// any of them: Rails reads its secret from encrypted credentials when it
    /// has `RAILS_MASTER_KEY`, and a generated `SECRET_KEY_BASE` would
    /// silently take precedence over that one.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "unlessSet")]
    pub unless_set: Vec<String>,
}

/// How a platform should produce a [`Deploy::generated_variables`] value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum GeneratedValue {
    /// `bytes` random bytes, hex-encoded (Rails `SECRET_KEY_BASE`, Symfony
    /// `APP_SECRET`).
    HexSecret {
        /// Random bytes before encoding.
        bytes: u32,
    },
    /// `bytes` random bytes, standard base64 with a `base64:` prefix — the
    /// format `php artisan key:generate` writes for Laravel's `APP_KEY`.
    PrefixedBase64Secret {
        /// Random bytes before encoding.
        bytes: u32,
    },
    /// The host name the app is publicly served on, without a scheme
    /// (Phoenix `PHX_HOST`).
    PublicHost,
    /// The URL the app is publicly served on, with its scheme (Laravel
    /// `APP_URL`).
    PublicUrl,
}

/// An unprivileged user created in the runtime image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeUser {
    /// Account name.
    pub name: String,
    /// Numeric uid. Used for `COPY --chown`, which cannot resolve names.
    pub uid: u32,
    /// Numeric gid.
    pub gid: u32,
    /// Home directory, which must be writable for tools that cache there.
    pub home: String,
}

impl Deploy {
    /// Names of every step the runtime image reads from.
    pub fn roots(&self) -> impl Iterator<Item = &str> {
        std::iter::once(&self.base)
            .chain(self.inputs.iter())
            .filter_map(|layer| layer.step.as_deref())
    }

    /// Set an environment variable on the runtime image.
    pub fn add_variable(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.variables.insert(key.into(), value.into());
        self
    }

    /// Register a task the platform can run against the image.
    pub fn add_task(&mut self, name: impl Into<String>, command: impl Into<String>) -> &mut Self {
        self.tasks.insert(name.into(), command.into());
        self
    }

    /// The conventional pre-deploy task, if one was declared.
    pub fn release_task(&self) -> Option<&str> {
        self.tasks.get(crate::procfile::RELEASE).map(String::as_str)
    }

    /// Prepend a directory to the runtime `PATH`, ignoring duplicates.
    pub fn add_path(&mut self, path: impl Into<String>) -> &mut Self {
        let path = path.into();
        if !self.paths.contains(&path) {
            self.paths.push(path);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_health_policy_preserves_existing_plans() {
        let deploy: Deploy = serde_json::from_str("{}").unwrap();
        assert_eq!(deploy.healthcheck, Healthcheck::Inherit);
        assert!(serde_json::to_value(&deploy)
            .unwrap()
            .get("healthcheck")
            .is_none());
        let disabled: Deploy = serde_json::from_str(r#"{"healthcheck":"disabled"}"#).unwrap();
        assert_eq!(disabled.healthcheck, Healthcheck::Disabled);
        assert_eq!(
            serde_json::to_value(&disabled).unwrap()["healthcheck"],
            "disabled"
        );
    }

    #[test]
    fn generated_variables_serialise_flat_with_optional_alternatives() {
        let variable = GeneratedVariable {
            value: GeneratedValue::HexSecret { bytes: 64 },
            unless_set: vec!["RAILS_MASTER_KEY".to_string()],
        };
        let json = serde_json::to_value(&variable).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "hexSecret", "bytes": 64, "unlessSet": ["RAILS_MASTER_KEY"]})
        );
        assert_eq!(
            serde_json::from_value::<GeneratedVariable>(json).unwrap(),
            variable
        );

        // Without alternatives the field is omitted, and absent reads back empty.
        let plain = serde_json::json!({"kind": "publicUrl"});
        let parsed: GeneratedVariable = serde_json::from_value(plain.clone()).unwrap();
        assert!(parsed.unless_set.is_empty());
        assert_eq!(serde_json::to_value(&parsed).unwrap(), plain);
    }
}
