//! Reading other builders' configuration files.
//!
//! An application that was deployed with Nixpacks or Railpack carries a config
//! file describing what its author wanted. Ignoring it makes a migration
//! silently change how the app builds — the worst way to find out is in
//! production.
//!
//! So autopack reads them. `autopack.json` wins where it exists, because it is
//! the file someone wrote *for* autopack; the others are a fallback.

pub mod nixpacks;
pub mod railpack;

use crate::app::App;
use crate::config::{Config, DEFAULT_CONFIG_FILE};
use crate::error::{Error, Result};

/// Which file the configuration came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConfigSource {
    /// No configuration file present.
    #[default]
    None,
    /// `autopack.json`.
    Autopack,
    /// `railpack.json`, read in compatibility mode.
    Railpack,
    /// `nixpacks.toml`, read in compatibility mode.
    Nixpacks,
}

impl ConfigSource {
    /// The file name this source reads.
    pub fn file(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Autopack => Some(DEFAULT_CONFIG_FILE),
            Self::Railpack => Some(railpack::FILE),
            Self::Nixpacks => Some(nixpacks::FILE),
        }
    }

    /// True when the configuration came from another builder's file.
    pub fn is_compat(self) -> bool {
        matches!(self, Self::Railpack | Self::Nixpacks)
    }
}

/// A configuration file that was found, translated, and what was lost.
pub struct Loaded {
    /// The translated configuration.
    pub config: Config,
    /// Which file it came from.
    pub source: ConfigSource,
    /// Human-readable notes about anything that could not be translated.
    ///
    /// Empty for `autopack.json`, which needs no translation.
    pub notes: Vec<String>,
}

/// Start commands that will not survive being taken literally.
///
/// Compatibility mode honours whatever the file says — overriding a user's
/// explicit configuration would be worse. But two of these fail in ways that
/// are hard to diagnose from the symptom, so they are worth calling out:
///
/// * development servers are single-threaded, reload on file change, and in
///   Django's case refuse to serve static files with `DEBUG=False`;
/// * `dotnet run` needs the SDK, and the runtime image only carries the
///   ASP.NET runtime — so it fails at startup with "command not found".
const RISKY_START_COMMANDS: &[(&str, &str)] = &[
    (
        "dotnet run",
        "needs the .NET SDK, which is not in the runtime image; use `dotnet <assembly>.dll`",
    ),
    (
        "manage.py runserver",
        "is Django's development server and should not serve production traffic",
    ),
    (
        "artisan serve",
        "is Laravel's development server and should not serve production traffic",
    ),
    (
        "flask run",
        "is Flask's development server; use gunicorn or uvicorn",
    ),
    (
        "next dev",
        "is Next.js in development mode; use `next start` against a build",
    ),
    (
        "nodemon",
        "is a file watcher, not a production process manager",
    ),
];

/// Note anything about a translated start command that is likely to bite.
fn review_start_command(config: &Config, notes: &mut Vec<String>) {
    let Some(start) = config
        .deploy
        .as_ref()
        .and_then(|d| d.start_command.as_deref())
    else {
        return;
    };

    for (pattern, why) in RISKY_START_COMMANDS {
        if start.contains(pattern) {
            notes.push(format!(
                "start command `{start}` was taken from the file as-is, but `{pattern}` {why}."
            ));
        }
    }
}

/// Find and read the first configuration file that applies.
///
/// Precedence is `autopack.json`, then `railpack.json`, then `nixpacks.toml`.
/// A file written for autopack always wins: if someone has taken the trouble to
/// write one, a stale file from a previous builder must not override it.
pub fn load(app: &App, compat_enabled: bool) -> Result<Loaded> {
    if let Some(contents) = app.read_file_opt(DEFAULT_CONFIG_FILE)? {
        let config: Config = serde_json::from_str(&contents).map_err(|e| Error::ParseFile {
            path: DEFAULT_CONFIG_FILE.into(),
            message: e.to_string(),
        })?;
        return Ok(Loaded {
            config,
            source: ConfigSource::Autopack,
            notes: Vec::new(),
        });
    }

    if !compat_enabled {
        return Ok(Loaded {
            config: Config::default(),
            source: ConfigSource::None,
            notes: Vec::new(),
        });
    }

    if let Some(contents) = app.read_file_opt(railpack::FILE)? {
        let parsed: railpack::RailpackConfig =
            serde_json::from_str(&contents).map_err(|e| Error::ParseFile {
                path: railpack::FILE.into(),
                message: e.to_string(),
            })?;
        let config = parsed.to_config();
        let mut notes = Vec::new();
        review_start_command(&config, &mut notes);
        return Ok(Loaded {
            config,
            source: ConfigSource::Railpack,
            notes,
        });
    }

    if let Some(contents) = app.read_file_opt(nixpacks::FILE)? {
        let parsed: nixpacks::NixpacksConfig =
            toml::from_str(&contents).map_err(|e| Error::ParseFile {
                path: nixpacks::FILE.into(),
                message: e.to_string(),
            })?;
        let (config, unmapped) = parsed.to_config();

        let mut notes = Vec::new();
        if !unmapped.nix_packages.is_empty() {
            notes.push(format!(
                "nixPkgs with no mise equivalent were skipped: {}. \
                 Add them with `aptPackages` in autopack.json if they are needed.",
                unmapped.nix_packages.join(", ")
            ));
        }
        if !unmapped.phases.is_empty() {
            notes.push(format!(
                "phases other than setup/install/build were skipped: {}. \
                 autopack has no equivalent of a custom phase.",
                unmapped.phases.join(", ")
            ));
        }

        review_start_command(&config, &mut notes);
        return Ok(Loaded {
            config,
            source: ConfigSource::Nixpacks,
            notes,
        });
    }

    Ok(Loaded {
        config: Config::default(),
        source: ConfigSource::None,
        notes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn app_with(files: &[(&str, &str)]) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            fs::write(dir.path().join(name), contents).unwrap();
        }
        let app = App::new(dir.path()).unwrap();
        (dir, app)
    }

    #[test]
    fn autopack_json_wins_over_a_stale_nixpacks_file() {
        // A repository mid-migration has both. The file written for autopack
        // is the current intent.
        let (_dir, app) = app_with(&[
            ("autopack.json", r#"{"provider":"node"}"#),
            ("nixpacks.toml", "providers = [\"python\"]\n"),
        ]);
        let loaded = load(&app, true).unwrap();
        assert_eq!(loaded.source, ConfigSource::Autopack);
        assert_eq!(loaded.config.provider.as_deref(), Some("node"));
    }

    #[test]
    fn railpack_json_wins_over_nixpacks_toml() {
        let (_dir, app) = app_with(&[
            ("railpack.json", r#"{"provider":"node"}"#),
            ("nixpacks.toml", "providers = [\"python\"]\n"),
        ]);
        assert_eq!(load(&app, true).unwrap().source, ConfigSource::Railpack);
    }

    #[test]
    fn nixpacks_is_read_when_it_is_the_only_file() {
        let (_dir, app) = app_with(&[("nixpacks.toml", "[start]\ncmd = \"./server\"\n")]);
        let loaded = load(&app, true).unwrap();
        assert_eq!(loaded.source, ConfigSource::Nixpacks);
        assert_eq!(
            loaded.config.deploy.unwrap().start_command.as_deref(),
            Some("./server")
        );
    }

    #[test]
    fn compat_can_be_turned_off() {
        // A platform may want to ignore stale files from a previous builder
        // rather than have them quietly steer the build.
        let (_dir, app) = app_with(&[("nixpacks.toml", "[start]\ncmd = \"./server\"\n")]);
        let loaded = load(&app, false).unwrap();
        assert_eq!(loaded.source, ConfigSource::None);
        assert!(loaded.config.deploy.is_none());
    }

    #[test]
    fn untranslatable_settings_are_reported() {
        let (_dir, app) = app_with(&[(
            "nixpacks.toml",
            "[phases.setup]\nnixPkgs = [\"imagemagick\"]\n\n[phases.deploy]\ncmds = [\"./deploy.sh\"]\n",
        )]);
        let loaded = load(&app, true).unwrap();
        assert_eq!(loaded.notes.len(), 2);
        assert!(
            loaded.notes[0].contains("imagemagick"),
            "{:?}",
            loaded.notes
        );
    }

    #[test]
    fn a_development_start_command_is_flagged() {
        // temps-demo-apps/dotnet/web really does say this. Honouring it is
        // correct — but it produces an image that cannot start, and the
        // symptom ("command not found") does not point at the cause.
        let (_dir, app) = app_with(&[("nixpacks.toml", "[start]\ncmd = \"dotnet run\"\n")]);
        let loaded = load(&app, true).unwrap();
        assert_eq!(
            loaded
                .config
                .deploy
                .as_ref()
                .unwrap()
                .start_command
                .as_deref(),
            Some("dotnet run"),
            "the file must still be honoured"
        );
        assert!(
            loaded.notes.iter().any(|n| n.contains("SDK")),
            "{:?}",
            loaded.notes
        );
    }

    #[test]
    fn an_ordinary_start_command_is_not_flagged() {
        let (_dir, app) = app_with(&[("nixpacks.toml", "[start]\ncmd = \"node server.js\"\n")]);
        assert!(load(&app, true).unwrap().notes.is_empty());
    }

    #[test]
    fn no_config_at_all_is_not_an_error() {
        let (_dir, app) = app_with(&[("package.json", "{}")]);
        let loaded = load(&app, true).unwrap();
        assert_eq!(loaded.source, ConfigSource::None);
        assert!(loaded.notes.is_empty());
    }
}
