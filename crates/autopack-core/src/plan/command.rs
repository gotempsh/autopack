//! Commands that make up a build step.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

/// A single operation performed inside a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Command {
    /// Run a command.
    Exec(ExecCommand),
    /// Prepend a directory to `PATH` for every later command and for the deploy image.
    Path(PathCommand),
    /// Copy files, either from the step's own filesystem or from another image.
    Copy(CopyCommand),
    /// Materialise an asset registered on the step as a file.
    File(FileCommand),
}

/// Run a command in the build environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecCommand {
    /// The command line to run, e.g. `npm ci`.
    pub cmd: String,
    /// Label shown in build output instead of the raw command.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "customName"
    )]
    pub custom_name: Option<String>,
}

/// Add a directory to the global `PATH`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathCommand {
    /// Directory to prepend to `PATH`.
    pub path: String,
}

/// Copy files into the step's filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyCommand {
    /// Optional image to copy from. Defaults to the step's own inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Source path.
    pub src: String,
    /// Destination path.
    pub dest: String,
}

/// Write an asset to disk.
///
/// The content lives in the step's `assets` map under [`FileCommand::name`],
/// which keeps large blobs out of the command list and lets the backend decide
/// how to transfer them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCommand {
    /// Absolute path of the file to write.
    pub path: String,
    /// Key of the asset in the step's `assets` map.
    pub name: String,
    /// Unix permission bits, e.g. `0o644`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Label shown in build output.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "customName"
    )]
    pub custom_name: Option<String>,
}

impl Command {
    /// Run `cmd` verbatim.
    pub fn exec(cmd: impl Into<String>) -> Self {
        Command::Exec(ExecCommand {
            cmd: cmd.into(),
            custom_name: None,
        })
    }

    /// Run `cmd` through `sh -c`, displaying the unwrapped command in build output.
    pub fn shell(cmd: impl Into<String>) -> Self {
        let cmd = cmd.into();
        Command::Exec(ExecCommand {
            cmd: format!("sh -c '{}'", cmd.replace('\'', r"'\''")),
            custom_name: Some(cmd),
        })
    }

    /// Prepend `path` to the global `PATH`.
    pub fn path(path: impl Into<String>) -> Self {
        Command::Path(PathCommand { path: path.into() })
    }

    /// Copy `src` to `dest` within the step.
    pub fn copy(src: impl Into<String>, dest: impl Into<String>) -> Self {
        Command::Copy(CopyCommand {
            image: None,
            src: src.into(),
            dest: dest.into(),
        })
    }

    /// Copy `src` to `dest` from another image.
    pub fn copy_from(
        image: impl Into<String>,
        src: impl Into<String>,
        dest: impl Into<String>,
    ) -> Self {
        Command::Copy(CopyCommand {
            image: Some(image.into()),
            src: src.into(),
            dest: dest.into(),
        })
    }

    /// Write the asset `name` to `path`.
    pub fn file(path: impl Into<String>, name: impl Into<String>) -> Self {
        Command::File(FileCommand {
            path: path.into(),
            name: name.into(),
            mode: None,
            custom_name: None,
        })
    }

    /// The label to show in build output for this command.
    pub fn display_name(&self) -> String {
        match self {
            Command::Exec(c) => c.custom_name.clone().unwrap_or_else(|| c.cmd.clone()),
            Command::Path(c) => format!("PATH += {}", c.path),
            Command::Copy(c) => match &c.image {
                Some(image) => format!("copy {}:{} -> {}", image, c.src, c.dest),
                None => format!("copy {} -> {}", c.src, c.dest),
            },
            Command::File(c) => c
                .custom_name
                .clone()
                .unwrap_or_else(|| format!("create {}", c.path)),
        }
    }
}

impl<'de> Deserialize<'de> for Command {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Commands are an untagged union, and `path`/`name` overlap between the
        // file and path variants. Dispatch on the key set explicitly so a typo
        // produces "unknown field" instead of "data did not match any variant".
        let value = serde_json::Value::deserialize(deserializer)?;

        if let Some(cmd) = value.as_str() {
            return Ok(Command::exec(cmd));
        }

        let object = value
            .as_object()
            .ok_or_else(|| de::Error::custom("a command must be a string or an object"))?;

        let variant = if object.contains_key("cmd") {
            serde_json::from_value(value).map(Command::Exec)
        } else if object.contains_key("src") || object.contains_key("dest") {
            serde_json::from_value(value).map(Command::Copy)
        } else if object.contains_key("name") {
            serde_json::from_value(value).map(Command::File)
        } else if object.contains_key("path") {
            serde_json::from_value(value).map(Command::Path)
        } else {
            return Err(de::Error::custom(
                "unrecognised command: expected one of `cmd`, `src`/`dest`, `name`, or `path`",
            ));
        };

        variant.map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_strings_parse_as_exec() {
        let cmd: Command = serde_json::from_str(r#""npm ci""#).unwrap();
        assert_eq!(cmd, Command::exec("npm ci"));
    }

    #[test]
    fn file_wins_over_path_when_name_is_present() {
        let cmd: Command =
            serde_json::from_str(r#"{"path":"/etc/caddy/Caddyfile","name":"Caddyfile"}"#).unwrap();
        assert!(matches!(cmd, Command::File(_)));
    }

    #[test]
    fn path_command_without_name_stays_a_path() {
        let cmd: Command = serde_json::from_str(r#"{"path":"/mise/shims"}"#).unwrap();
        assert_eq!(cmd, Command::path("/mise/shims"));
    }

    #[test]
    fn shell_quoting_survives_embedded_quotes() {
        let cmd = Command::shell("echo 'hi'");
        let Command::Exec(exec) = &cmd else {
            panic!("expected exec");
        };
        assert_eq!(exec.cmd, r"sh -c 'echo '\''hi'\'''");
        assert_eq!(exec.custom_name.as_deref(), Some("echo 'hi'"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = serde_json::from_str::<Command>(r#"{"cmd":"ls","typo":1}"#).unwrap_err();
        assert!(err.to_string().contains("typo"), "{err}");
    }
}
