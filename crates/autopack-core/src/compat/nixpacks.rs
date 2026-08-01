//! Reading `nixpacks.toml`.
//!
//! Repositories that were deployed with Nixpacks carry configuration that
//! still describes what the author wanted, and discarding it means a migration
//! silently changes how their app builds. This translates as much of it as has
//! a meaning here.
//!
//! The translation is partial by nature: `nixPkgs` names packages in the Nix
//! package set, which is not the same namespace as mise. Common runtimes map
//! cleanly; anything else is reported rather than dropped in silence.

use indexmap::IndexMap;
use serde::Deserialize;

use crate::config::{Config, DeployPatch, StepPatch};
use crate::plan::Command;
use crate::steps;

/// File name, relative to the app root.
pub const FILE: &str = "nixpacks.toml";

/// The subset of `nixpacks.toml` that has an equivalent here.
#[derive(Debug, Default, Deserialize)]
pub struct NixpacksConfig {
    /// Forces a provider. Nixpacks allows several; the first wins.
    #[serde(default)]
    pub providers: Vec<String>,

    /// Environment variables. Nixpacks exposes these at build *and* run time.
    #[serde(default)]
    pub variables: IndexMap<String, String>,

    /// Build phases, keyed by name. Nixpacks allows arbitrary phase names.
    #[serde(default)]
    pub phases: IndexMap<String, Phase>,

    /// The `[start]` table.
    #[serde(default)]
    pub start: Option<Start>,
}

/// One phase of a Nixpacks build.
#[derive(Debug, Default, Deserialize)]
pub struct Phase {
    /// Nix package names.
    #[serde(default, rename = "nixPkgs")]
    pub nix_pkgs: Vec<String>,

    /// Debian package names — these translate directly.
    #[serde(default, rename = "aptPkgs")]
    pub apt_pkgs: Vec<String>,

    /// Commands to run in this phase.
    #[serde(default)]
    pub cmds: Vec<String>,
}

/// The `[start]` table.
#[derive(Debug, Default, Deserialize)]
pub struct Start {
    /// The container's start command.
    #[serde(default)]
    pub cmd: Option<String>,
}

/// What a translation could not carry over, for reporting to the user.
#[derive(Debug, Default)]
pub struct Unmapped {
    /// Nix package names with no known mise equivalent.
    pub nix_packages: Vec<String>,
    /// Phases other than setup, install and build.
    pub phases: Vec<String>,
}

impl NixpacksConfig {
    /// Translate into an autopack [`Config`].
    ///
    /// Returns what could not be translated alongside it, so a caller can tell
    /// the user rather than let a silently-dropped setting surface as a
    /// mysterious build difference.
    pub fn to_config(&self) -> (Config, Unmapped) {
        let mut config = Config::default();
        let mut unmapped = Unmapped::default();

        if let Some(provider) = self.providers.first() {
            config.provider = Some(normalize_provider(provider));
        }

        for (name, phase) in &self.phases {
            config.apt_packages.extend(phase.apt_pkgs.iter().cloned());

            for package in &phase.nix_pkgs {
                match nix_to_mise(package) {
                    Some((tool, version)) => {
                        config.packages.insert(tool.to_string(), version);
                    }
                    None => unmapped.nix_packages.push(package.clone()),
                }
            }

            if phase.cmds.is_empty() {
                continue;
            }

            // Nixpacks phases are a free-form DAG; autopack has two named
            // command steps. `setup` is package installation, which is handled
            // above, so only install and build carry commands across.
            let step = match name.as_str() {
                "install" => steps::INSTALL,
                "build" => steps::BUILD,
                "setup" => continue,
                other => {
                    unmapped.phases.push(other.to_string());
                    continue;
                }
            };

            config.steps.insert(
                step.to_string(),
                StepPatch {
                    commands: Some(phase.cmds.iter().map(Command::shell).collect()),
                    ..Default::default()
                },
            );
        }

        let start = self.start.as_ref().and_then(|s| s.cmd.clone());
        if start.is_some() || !self.variables.is_empty() {
            config.deploy = Some(DeployPatch {
                start_command: start,
                // Nixpacks variables are set for the build and inherited by the
                // runtime, so the runtime is where they have to land here.
                variables: self.variables.clone(),
                ..Default::default()
            });
        }

        (config, unmapped)
    }
}

/// Map a Nix package name onto a mise tool and version.
///
/// Nix encodes the version in the attribute name (`nodejs_20`, `python311`,
/// `ruby_3_2`), so the version is recovered from the suffix where there is one.
fn nix_to_mise(package: &str) -> Option<(&'static str, String)> {
    let (base, version) = split_nix_version(package);

    let tool = match base.as_str() {
        "nodejs" | "nodejs-slim" | "node" => "node",
        "python" | "python3" => "python",
        "go" | "golang" => "go",
        "rustc" | "cargo" | "rust" | "rustup" => "rust",
        "openjdk" | "jdk" | "jre" | "temurin-bin" => "java",
        "ruby" => "ruby",
        "php" => "php",
        "elixir" => "elixir",
        "erlang" | "erlangR26" => "erlang",
        "deno" => "deno",
        "bun" => "bun",
        "dotnet-sdk" | "dotnet" => "dotnet",
        "swift" => "swift",
        "dart" => "dart",
        "crystal" => "crystal",
        "zig" => "zig",
        "ghc" | "haskell" => "haskell",
        // Package managers Nixpacks lists explicitly; mise has them too.
        "yarn" => "yarn",
        "pnpm" => "pnpm",
        "poetry" => "poetry",
        "uv" => "uv",
        _ => return None,
    };

    Some((tool, version.unwrap_or_else(|| "latest".to_string())))
}

/// `nodejs_20` -> (`nodejs`, `20`), `python311` -> (`python`, `3.11`).
fn split_nix_version(package: &str) -> (String, Option<String>) {
    // Underscore form: nodejs_20, ruby_3_2, go_1_21
    if let Some((base, rest)) = package.split_once('_') {
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return (base.to_string(), Some(rest.replace('_', ".")));
        }
    }

    // Concatenated form: python311, php82, erlangR26
    let digits: String = package
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    if digits.len() >= 2 {
        let base = package[..package.len() - digits.len()].to_string();
        // `python311` means 3.11, `php82` means 8.2 — the first digit is the
        // major version and the rest is the minor.
        let version = format!("{}.{}", &digits[..1], &digits[1..]);
        return (base, Some(version));
    }

    (package.to_string(), None)
}

/// Nixpacks provider ids that differ from autopack's.
fn normalize_provider(provider: &str) -> String {
    match provider {
        "csharp" | "fsharp" => "dotnet".to_string(),
        "golang" => "go".to_string(),
        "staticfile" => "static".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> (Config, Unmapped) {
        let parsed: NixpacksConfig = toml::from_str(toml_str).unwrap();
        parsed.to_config()
    }

    #[test]
    fn carries_the_start_command() {
        // The most common shape by far in real repositories.
        let (config, _) =
            parse("[start]\ncmd = \"gunicorn app:app --bind 0.0.0.0:${PORT:-5000}\"\n");
        assert_eq!(
            config.deploy.unwrap().start_command.as_deref(),
            Some("gunicorn app:app --bind 0.0.0.0:${PORT:-5000}")
        );
    }

    #[test]
    fn apt_packages_translate_directly() {
        let (config, unmapped) = parse(
            "[start]\ncmd = \"rails server\"\n\n[phases.setup]\naptPkgs = [\"libpq-dev\", \"pkg-config\"]\n",
        );
        assert_eq!(config.apt_packages, vec!["libpq-dev", "pkg-config"]);
        assert!(unmapped.nix_packages.is_empty());
    }

    #[test]
    fn nix_runtimes_map_onto_mise_with_their_versions() {
        let (config, _) = parse("[phases.setup]\nnixPkgs = [\"nodejs_20\", \"python311\"]\n");
        assert_eq!(config.packages["node"], "20");
        assert_eq!(config.packages["python"], "3.11");
    }

    #[test]
    fn unmappable_nix_packages_are_reported_not_dropped() {
        // `imagemagick` is a real Nix package with no mise equivalent. Silently
        // ignoring it would turn into a confusing runtime failure.
        let (_, unmapped) = parse("[phases.setup]\nnixPkgs = [\"imagemagick\", \"nodejs\"]\n");
        assert_eq!(unmapped.nix_packages, vec!["imagemagick"]);
    }

    #[test]
    fn install_and_build_phases_become_steps() {
        let (config, _) = parse(
            "[phases.install]\ncmds = [\"npm ci\"]\n\n[phases.build]\ncmds = [\"npm run build\"]\n",
        );
        assert_eq!(
            config.steps["install"].commands.as_ref().unwrap()[0].display_name(),
            "npm ci"
        );
        assert_eq!(
            config.steps["build"].commands.as_ref().unwrap()[0].display_name(),
            "npm run build"
        );
    }

    #[test]
    fn unknown_phases_are_reported() {
        let (_, unmapped) = parse("[phases.migrate]\ncmds = [\"rake db:migrate\"]\n");
        assert_eq!(unmapped.phases, vec!["migrate"]);
    }

    #[test]
    fn variables_reach_the_runtime() {
        let (config, _) = parse("[variables]\nNODE_ENV = \"production\"\n");
        assert_eq!(config.deploy.unwrap().variables["NODE_ENV"], "production");
    }

    #[test]
    fn provider_ids_are_normalised() {
        let (config, _) = parse("providers = [\"csharp\"]\n");
        assert_eq!(config.provider.as_deref(), Some("dotnet"));
    }

    #[test]
    fn version_suffixes_parse_both_ways() {
        assert_eq!(
            split_nix_version("nodejs_20"),
            ("nodejs".into(), Some("20".into()))
        );
        assert_eq!(
            split_nix_version("ruby_3_2"),
            ("ruby".into(), Some("3.2".into()))
        );
        assert_eq!(
            split_nix_version("php82"),
            ("php".into(), Some("8.2".into()))
        );
        assert_eq!(split_nix_version("go"), ("go".into(), None));
    }
}
