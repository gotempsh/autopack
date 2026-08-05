//! Which JavaScript package manager the app uses, and how to drive it.

use autopack_core::{App, Lock};

use super::package_json::PackageJson;
use crate::support::normalize_version_range;

/// A JavaScript package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    /// npm.
    Npm,
    /// pnpm.
    Pnpm,
    /// Yarn 1.x ("classic").
    Yarn,
    /// Yarn 2+ ("berry"), which behaves differently enough to warrant its own case.
    YarnBerry,
    /// Bun.
    Bun,
}

/// Whether any script runs the app through Bun.
fn invokes_bun(package: &PackageJson) -> bool {
    package.scripts.values().any(|script| {
        let script = script.trim();
        script == "bun"
            || script.starts_with("bun ")
            || script.starts_with("bunx ")
            || script.contains("&& bun ")
    })
}

/// Major version of `version`, e.g. `10.12.0` -> `10`.
fn major_of(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

impl PackageManager {
    /// Work out the package manager from lockfiles and the `packageManager` field.
    ///
    /// Lockfiles win over the `packageManager` pin: the lockfile is what the
    /// install command actually consumes, and a stale pin is common.
    pub fn detect(app: &App, package: &PackageJson) -> Self {
        if app.has_any_file(["bun.lock", "bun.lockb"]) {
            return Self::Bun;
        }
        if app.has_file("pnpm-lock.yaml") || app.has_file("pnpm-workspace.yaml") {
            return Self::Pnpm;
        }
        if app.has_file("yarn.lock") {
            return if app.has_file(".yarnrc.yml") {
                Self::YarnBerry
            } else {
                Self::Yarn
            };
        }
        if app.has_any_file(["package-lock.json", "npm-shrinkwrap.json"]) {
            return Self::Npm;
        }

        // No lockfile and no pin: the scripts are the only remaining signal.
        // An app whose `start` is `bun run index.ts` is a Bun app even with a
        // bare package.json, and guessing npm here produces an image that
        // builds cleanly and then exits 127 on boot.
        if package.package_manager.is_none() && invokes_bun(package) {
            return Self::Bun;
        }

        match package.package_manager.as_deref() {
            Some(pinned) if pinned.starts_with("pnpm") => Self::Pnpm,
            Some(pinned) if pinned.starts_with("bun") => Self::Bun,
            Some(pinned) if pinned.starts_with("yarn") => {
                let major = pinned
                    .split_once('@')
                    .and_then(|(_, version)| version.split('.').next())
                    .and_then(|major| major.parse::<u32>().ok())
                    .unwrap_or(1);
                if major >= 2 {
                    Self::YarnBerry
                } else {
                    Self::Yarn
                }
            }
            _ => Self::Npm,
        }
    }

    /// Identifier used in metadata and cache names.
    pub fn id(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn | Self::YarnBerry => "yarn",
            Self::Bun => "bun",
        }
    }

    /// The version requested by the `packageManager` pin, if that pin actually
    /// names *this* manager.
    ///
    /// The name check matters because [`Self::detect`] lets a lockfile outvote
    /// the pin: a repo with `pnpm-lock.yaml` and `"packageManager": "npm@7.33.5"`
    /// is a pnpm app. Taking the version without the name installs pnpm at
    /// npm's version number — and pnpm 7 has no `ci` command, so it aliases the
    /// app's own `ci` script instead and the install silently does nothing.
    fn pinned_version(self, package: &PackageJson) -> Option<String> {
        let (name, version) = package.package_manager.as_deref()?.split_once('@')?;
        (name == self.id()).then(|| normalize_version_range(version))?
    }

    /// The version that will actually be installed.
    ///
    /// `autopack.lock` outranks the pin, mirroring the substitution
    /// [`autopack_core::BuildContext`] applies when it writes `mise.toml`.
    /// `None` means mise resolves `latest` at build time.
    ///
    /// Every decision that depends on the manager's version must go through
    /// here. Re-deriving it from `package.json` produces a plan that runs
    /// subcommands the installed binary does not have.
    pub fn resolved_version(self, package: &PackageJson, lock: Option<&Lock>) -> Option<String> {
        let tool = self.mise_tool(package)?.0;
        lock.and_then(|lock| lock.tool(tool))
            .map(String::from)
            .or_else(|| self.pinned_version(package))
    }

    /// Major version of [`Self::resolved_version`].
    fn resolved_major(self, package: &PackageJson, lock: Option<&Lock>) -> Option<u32> {
        self.resolved_version(package, lock)
            .as_deref()
            .and_then(major_of)
    }

    /// The mise tool that provides this package manager, if it is not bundled
    /// with Node itself. npm ships with Node, so it needs no extra tool.
    pub fn mise_tool(self, package: &PackageJson) -> Option<(&'static str, String)> {
        let pinned_version = self.pinned_version(package);

        match self {
            Self::Npm => None,
            Self::Pnpm => Some(("pnpm", pinned_version.unwrap_or_else(|| "latest".into()))),
            Self::Yarn => Some(("yarn", pinned_version.unwrap_or_else(|| "1".into()))),
            Self::YarnBerry => Some(("yarn", pinned_version.unwrap_or_else(|| "latest".into()))),
            Self::Bun => Some(("bun", pinned_version.unwrap_or_else(|| "latest".into()))),
        }
    }

    /// Whether the installed binary needs `libatomic1` on debian-slim.
    ///
    /// pnpm 11+ is distributed as `@pnpm/exe`, which dynamically links
    /// `libatomic.so.1`; older majors do not need it.
    ///
    /// An unresolvable version counts as needing it — mise resolves `latest`,
    /// which is 11+ — and erring that way is deliberate: over-installing a
    /// 10KB library costs nothing, omitting it is a loader failure at boot.
    pub fn needs_libatomic(self, package: &PackageJson, lock: Option<&Lock>) -> bool {
        self == Self::Pnpm
            && self
                .resolved_major(package, lock)
                .is_none_or(|major| major >= 11)
    }

    /// The install command, honouring whether a lockfile is present.
    ///
    /// Frozen-lockfile installs fail loudly when the lockfile is out of date,
    /// which is the correct behaviour for a reproducible build. Without a
    /// lockfile there is nothing to freeze, so a plain install is used.
    ///
    /// For pnpm the command also depends on the version that will be installed:
    /// `pnpm ci` only from 11, anything else keeps the frozen install.
    ///
    /// `CI=true` is set on the install/build steps separately — do not prefix
    /// it into this command string.
    pub fn install_command(
        self,
        app: &App,
        package: &PackageJson,
        lock: Option<&Lock>,
    ) -> &'static str {
        match self {
            Self::Npm => {
                if app.has_any_file(["package-lock.json", "npm-shrinkwrap.json"]) {
                    "npm ci"
                } else {
                    "npm install"
                }
            }
            Self::Pnpm => {
                if app.has_file("pnpm-lock.yaml") {
                    match self.resolved_major(package, lock) {
                        // `pnpm ci` landed in 11.0. Everything else — including
                        // a version we cannot resolve — takes the frozen
                        // install: every major supports it, and unlike `ci` on
                        // an older pnpm it can never be shadowed by a
                        // same-named script in the app's package.json.
                        Some(major) if major >= 11 => "pnpm ci",
                        _ => "pnpm install --frozen-lockfile",
                    }
                } else {
                    "pnpm install"
                }
            }
            Self::Yarn => {
                if app.has_file("yarn.lock") {
                    "yarn install --frozen-lockfile"
                } else {
                    "yarn install"
                }
            }
            Self::YarnBerry => {
                if app.has_file("yarn.lock") {
                    "yarn install --immutable"
                } else {
                    "yarn install"
                }
            }
            Self::Bun => {
                if app.has_any_file(["bun.lock", "bun.lockb"]) {
                    "bun install --frozen-lockfile"
                } else {
                    "bun install"
                }
            }
        }
    }

    /// Command that runs the npm script `script`.
    pub fn run_command(self, script: &str) -> String {
        match self {
            Self::Npm => format!("npm run {script}"),
            Self::Pnpm => format!("pnpm run {script}"),
            Self::Yarn | Self::YarnBerry => format!("yarn run {script}"),
            Self::Bun => format!("bun run {script}"),
        }
    }

    /// Directory to persist between builds, and the environment variables that
    /// point the package manager at it.
    pub fn cache(self) -> (&'static str, Vec<(&'static str, &'static str)>) {
        match self {
            Self::Npm => ("/cache/npm", vec![("npm_config_cache", "/cache/npm")]),
            Self::Pnpm => ("/cache/pnpm", vec![("npm_config_store_dir", "/cache/pnpm")]),
            Self::Yarn => ("/cache/yarn", vec![("YARN_CACHE_FOLDER", "/cache/yarn")]),
            Self::YarnBerry => (
                "/cache/yarn",
                vec![
                    ("YARN_ENABLE_GLOBAL_CACHE", "1"),
                    ("YARN_GLOBAL_FOLDER", "/cache/yarn"),
                ],
            ),
            Self::Bun => ("/cache/bun", vec![("BUN_INSTALL_CACHE_DIR", "/cache/bun")]),
        }
    }

    /// Lockfiles and config files that must be present for `install` to run.
    pub fn manifest_files(self) -> &'static [&'static str] {
        match self {
            Self::Npm => &[
                "package.json",
                "package-lock.json",
                "npm-shrinkwrap.json",
                ".npmrc",
            ],
            Self::Pnpm => &[
                "package.json",
                "pnpm-lock.yaml",
                "pnpm-workspace.yaml",
                ".npmrc",
            ],
            Self::Yarn => &["package.json", "yarn.lock", ".yarnrc", ".npmrc"],
            Self::YarnBerry => &["package.json", "yarn.lock", ".yarnrc.yml", ".npmrc"],
            Self::Bun => &["package.json", "bun.lock", "bun.lockb", ".npmrc"],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn app_with(files: &[&str]) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            fs::write(dir.path().join(file), "").unwrap();
        }
        let app = App::new(dir.path()).unwrap();
        (dir, app)
    }

    #[test]
    fn lockfiles_pick_the_manager() {
        let (_d, app) = app_with(&["package.json", "pnpm-lock.yaml"]);
        assert_eq!(
            PackageManager::detect(&app, &PackageJson::default()),
            PackageManager::Pnpm
        );
    }

    #[test]
    fn yarn_berry_is_distinguished_by_yarnrc_yml() {
        let (_d, app) = app_with(&["package.json", "yarn.lock", ".yarnrc.yml"]);
        assert_eq!(
            PackageManager::detect(&app, &PackageJson::default()),
            PackageManager::YarnBerry
        );
    }

    #[test]
    fn package_manager_field_is_the_fallback() {
        let (_d, app) = app_with(&["package.json"]);
        let package = PackageJson {
            package_manager: Some("yarn@4.3.1".into()),
            ..Default::default()
        };
        assert_eq!(
            PackageManager::detect(&app, &package),
            PackageManager::YarnBerry
        );
    }

    #[test]
    fn lockfiles_beat_the_package_manager_field() {
        let (_d, app) = app_with(&["package.json", "package-lock.json"]);
        let package = PackageJson {
            package_manager: Some("pnpm@9.0.0".into()),
            ..Default::default()
        };
        assert_eq!(PackageManager::detect(&app, &package), PackageManager::Npm);
    }

    #[test]
    fn a_bare_package_json_running_bun_is_a_bun_app() {
        // The bun-server starter in temps-examples is exactly this shape:
        // no lockfile, no packageManager pin, just `"start": "bun run index.ts"`.
        let (_d, app) = app_with(&["package.json"]);
        let package: PackageJson =
            serde_json::from_str(r#"{"scripts":{"start":"bun run index.ts"}}"#).unwrap();
        assert_eq!(PackageManager::detect(&app, &package), PackageManager::Bun);
    }

    #[test]
    fn a_lockfile_still_outranks_the_scripts() {
        let (_d, app) = app_with(&["package.json", "package-lock.json"]);
        let package: PackageJson =
            serde_json::from_str(r#"{"scripts":{"start":"bun run index.ts"}}"#).unwrap();
        assert_eq!(PackageManager::detect(&app, &package), PackageManager::Npm);
    }

    #[test]
    fn scripts_merely_mentioning_bun_do_not_count() {
        let (_d, app) = app_with(&["package.json"]);
        let package: PackageJson =
            serde_json::from_str(r#"{"scripts":{"start":"node bundle-builder.js"}}"#).unwrap();
        assert_eq!(PackageManager::detect(&app, &package), PackageManager::Npm);
    }

    #[test]
    fn install_command_depends_on_the_lockfile() {
        let (_d, with_lock) = app_with(&["package.json", "package-lock.json"]);
        let (_d2, without_lock) = app_with(&["package.json"]);
        let package = PackageJson::default();
        assert_eq!(
            PackageManager::Npm.install_command(&with_lock, &package, None),
            "npm ci"
        );
        assert_eq!(
            PackageManager::Npm.install_command(&without_lock, &package, None),
            "npm install"
        );
    }

    #[test]
    fn pnpm_10_uses_a_frozen_install() {
        // pnpm 10 has no `ci` subcommand; CI=true is set on the step instead.
        let (_d, app) = app_with(&["package.json", "pnpm-lock.yaml"]);
        for pin in [
            "pnpm@10.12.0",
            "pnpm@^10.15.0",
            "pnpm@>=10",
            "pnpm@v10.15.0",
        ] {
            let package = PackageJson {
                package_manager: Some(pin.into()),
                ..Default::default()
            };
            assert_eq!(
                PackageManager::Pnpm.install_command(&app, &package, None),
                "pnpm install --frozen-lockfile",
                "{pin}"
            );
        }
    }

    #[test]
    fn pnpm_11_and_later_use_pnpm_ci() {
        // `pnpm ci` landed in 11.0 and is the durable replacement for CI=true.
        let (_d, app) = app_with(&["package.json", "pnpm-lock.yaml"]);
        for pin in ["pnpm@11.0.0", "pnpm@11.12.0", "pnpm@12.0.0"] {
            let package = PackageJson {
                package_manager: Some(pin.into()),
                ..Default::default()
            };
            assert_eq!(
                PackageManager::Pnpm.install_command(&app, &package, None),
                "pnpm ci",
                "{pin}"
            );
        }
    }

    #[test]
    fn unpinned_pnpm_keeps_the_frozen_install() {
        // mise resolves "latest" at build time, so the major is unknowable
        // here. `pnpm install --frozen-lockfile` works on every major; `ci`
        // would break the moment "latest" is not 11+ (an offline mise cache,
        // an internal mirror still serving 10.x).
        let (_d, app) = app_with(&["package.json", "pnpm-lock.yaml"]);
        assert_eq!(
            PackageManager::Pnpm.install_command(&app, &PackageJson::default(), None),
            "pnpm install --frozen-lockfile"
        );
    }

    #[test]
    fn older_pnpm_keeps_frozen_lockfile() {
        let (_d, app) = app_with(&["package.json", "pnpm-lock.yaml"]);
        let package = PackageJson {
            package_manager: Some("pnpm@9.15.0".into()),
            ..Default::default()
        };
        assert_eq!(
            PackageManager::Pnpm.install_command(&app, &package, None),
            "pnpm install --frozen-lockfile"
        );
    }

    #[test]
    fn pnpm_without_a_lockfile_stays_a_plain_install() {
        let (_d, app) = app_with(&["package.json"]);
        let package = PackageJson {
            package_manager: Some("pnpm@11.12.0".into()),
            ..Default::default()
        };
        assert_eq!(
            PackageManager::Pnpm.install_command(&app, &package, None),
            "pnpm install"
        );
    }

    #[test]
    fn pinned_version_flows_into_the_mise_tool() {
        let package = PackageJson {
            package_manager: Some("pnpm@9.1.0".into()),
            ..Default::default()
        };
        let (tool, version) = PackageManager::Pnpm.mise_tool(&package).unwrap();
        assert_eq!(tool, "pnpm");
        assert_eq!(version, "9.1.0");
    }

    #[test]
    fn resolved_version_reads_ranges_and_prefers_the_lock() {
        for (pin, version) in [
            ("pnpm@10.12.0", Some("10.12.0")),
            ("pnpm@^10.15.0", Some("10.15.0")),
            ("pnpm@>=10", Some("10")),
            ("pnpm@v10.15.0", Some("10.15.0")),
            ("pnpm@10.x", Some("10")),
            ("pnpm@10", Some("10")),
        ] {
            let package = PackageJson {
                package_manager: Some(pin.into()),
                ..Default::default()
            };
            assert_eq!(
                PackageManager::Pnpm
                    .resolved_version(&package, None)
                    .as_deref(),
                version,
                "{pin}"
            );
        }

        // A pin naming a different manager is not this manager's version.
        // `detect` lets the lockfile win, so this repo is a pnpm app — but
        // 7.33.5 is npm's version and must not be installed as pnpm's.
        let mismatched = PackageJson {
            package_manager: Some("npm@7.33.5".into()),
            ..Default::default()
        };
        assert_eq!(
            PackageManager::Pnpm.resolved_version(&mismatched, None),
            None
        );
        assert_eq!(
            PackageManager::Pnpm.mise_tool(&mismatched),
            Some(("pnpm", "latest".to_string()))
        );

        // autopack.lock outranks the pin, matching what lands in mise.toml.
        let pinned_11 = PackageJson {
            package_manager: Some("pnpm@11.19.0".into()),
            ..Default::default()
        };
        let mut lock = Lock::new();
        lock.tools.insert("pnpm".into(), "10.15.0".into());
        assert_eq!(
            PackageManager::Pnpm
                .resolved_version(&pinned_11, Some(&lock))
                .as_deref(),
            Some("10.15.0")
        );

        assert_eq!(
            PackageManager::Pnpm.resolved_version(&PackageJson::default(), None),
            None
        );
    }

    #[test]
    fn install_command_follows_the_version_that_gets_installed() {
        let (_d, app) = app_with(&["package.json", "pnpm-lock.yaml"]);

        // A pin for a different manager must not select `pnpm ci`: mise would
        // install pnpm 7, where `ci` is not a command and pnpm aliases the
        // app's own `ci` script instead, silently skipping the install.
        let mismatched = PackageJson {
            package_manager: Some("npm@7.33.5".into()),
            ..Default::default()
        };
        assert_eq!(
            PackageManager::Pnpm.install_command(&app, &mismatched, None),
            "pnpm install --frozen-lockfile"
        );

        // A stale lock pinning pnpm 10 beats a package.json bumped to 11.
        let pinned_11 = PackageJson {
            package_manager: Some("pnpm@11.19.0".into()),
            ..Default::default()
        };
        let mut stale = Lock::new();
        stale.tools.insert("pnpm".into(), "10.15.0".into());
        assert_eq!(
            PackageManager::Pnpm.install_command(&app, &pinned_11, Some(&stale)),
            "pnpm install --frozen-lockfile"
        );
        assert!(!PackageManager::Pnpm.needs_libatomic(&pinned_11, Some(&stale)));

        // ...and a lock pinning 11 beats a package.json still on 10.
        let pinned_10 = PackageJson {
            package_manager: Some("pnpm@10.12.0".into()),
            ..Default::default()
        };
        let mut ahead = Lock::new();
        ahead.tools.insert("pnpm".into(), "11.19.0".into());
        assert_eq!(
            PackageManager::Pnpm.install_command(&app, &pinned_10, Some(&ahead)),
            "pnpm ci"
        );
        assert!(PackageManager::Pnpm.needs_libatomic(&pinned_10, Some(&ahead)));
    }

    #[test]
    fn needs_libatomic_follows_the_pnpm_major() {
        let v11 = PackageJson {
            package_manager: Some("pnpm@11.19.0".into()),
            ..Default::default()
        };
        let v9 = PackageJson {
            package_manager: Some("pnpm@9.15.0".into()),
            ..Default::default()
        };
        assert!(PackageManager::Pnpm.needs_libatomic(&v11, None));
        assert!(PackageManager::Pnpm.needs_libatomic(&PackageJson::default(), None));
        assert!(!PackageManager::Pnpm.needs_libatomic(&v9, None));
        assert!(!PackageManager::Npm.needs_libatomic(&v11, None));
    }
}
