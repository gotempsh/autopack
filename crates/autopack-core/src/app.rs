//! Read-only view over the user's source directory.
//!
//! Providers only ever see the app through this type. Keeping file access
//! behind one struct means detection is cheap (the directory is walked once),
//! deterministic (results are sorted), and testable without touching disk
//! layout details in every provider.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use globset::{Glob, GlobSetBuilder};
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

/// Directories never descended into while indexing a source tree.
///
/// These are either build output or dependency trees: walking them can add
/// hundreds of thousands of entries and changes no detection outcome.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "vendor",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".nuxt",
    ".turbo",
    ".gradle",
    ".terraform",
];

/// How deep the indexer descends. Deeper files are ignored by glob matching.
const MAX_DEPTH: usize = 8;

/// Upper bound on indexed paths, so a pathological repo cannot stall detection.
const MAX_ENTRIES: usize = 50_000;

/// A source directory being analysed.
pub struct App {
    source: PathBuf,
    /// Relative, `/`-separated paths of every indexed file, sorted.
    files: OnceLock<Vec<String>>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("source", &self.source).finish()
    }
}

impl App {
    /// Open `source` for analysis.
    ///
    /// Returns [`Error::InvalidSource`] when the path is missing or is a file.
    pub fn new(source: impl AsRef<Path>) -> Result<Self> {
        let source = source.as_ref();
        if !source.is_dir() {
            return Err(Error::InvalidSource(source.to_path_buf()));
        }
        let source = source
            .canonicalize()
            .unwrap_or_else(|_| source.to_path_buf());
        Ok(Self {
            source,
            files: OnceLock::new(),
        })
    }

    /// The directory being analysed.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Absolute path for a path relative to the app root.
    pub fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.source.join(relative)
    }

    /// True when `relative` exists and is a file.
    pub fn has_file(&self, relative: impl AsRef<Path>) -> bool {
        self.path(relative).is_file()
    }

    /// True when `relative` exists and is a directory.
    pub fn has_dir(&self, relative: impl AsRef<Path>) -> bool {
        self.path(relative).is_dir()
    }

    /// True when any of `candidates` exists as a file.
    pub fn has_any_file<I, S>(&self, candidates: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<Path>,
    {
        candidates.into_iter().any(|c| self.has_file(c))
    }

    /// Indexed file paths matching `pattern`, relative to the app root.
    ///
    /// Patterns are `globset` syntax (`**/*.csproj`, `src/*.go`). Paths inside
    /// dependency and build-output directories are not indexed.
    pub fn find_files(&self, pattern: &str) -> Result<Vec<String>> {
        let glob = Glob::new(pattern).map_err(|e| Error::InvalidGlob {
            pattern: pattern.to_string(),
            message: e.to_string(),
        })?;
        let set = GlobSetBuilder::new()
            .add(glob)
            .build()
            .map_err(|e| Error::InvalidGlob {
                pattern: pattern.to_string(),
                message: e.to_string(),
            })?;

        Ok(self
            .files()
            .iter()
            .filter(|path| set.is_match(path.as_str()))
            .cloned()
            .collect())
    }

    /// True when at least one indexed file matches `pattern`.
    pub fn has_match(&self, pattern: &str) -> bool {
        self.find_files(pattern)
            .map(|files| !files.is_empty())
            .unwrap_or(false)
    }

    /// Read `relative` as UTF-8.
    pub fn read_file(&self, relative: impl AsRef<Path>) -> Result<String> {
        let relative = relative.as_ref();
        fs::read_to_string(self.path(relative)).map_err(|source| Error::ReadFile {
            path: relative.to_path_buf(),
            source,
        })
    }

    /// Read `relative` as UTF-8, or `None` when it does not exist.
    ///
    /// Unreadable-but-present files still return an error: silently treating a
    /// permission error as "absent" would produce a confidently wrong plan.
    pub fn read_file_opt(&self, relative: impl AsRef<Path>) -> Result<Option<String>> {
        let relative = relative.as_ref();
        if !self.has_file(relative) {
            return Ok(None);
        }
        self.read_file(relative).map(Some)
    }

    /// Parse `relative` as JSON.
    pub fn read_json<T: DeserializeOwned>(&self, relative: impl AsRef<Path>) -> Result<T> {
        let relative = relative.as_ref();
        let contents = self.read_file(relative)?;
        serde_json::from_str(&contents).map_err(|e| Error::ParseFile {
            path: relative.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Parse `relative` as JSON, or `None` when it does not exist.
    pub fn read_json_opt<T: DeserializeOwned>(
        &self,
        relative: impl AsRef<Path>,
    ) -> Result<Option<T>> {
        if !self.has_file(&relative) {
            return Ok(None);
        }
        self.read_json(relative).map(Some)
    }

    /// Parse `relative` as TOML.
    pub fn read_toml<T: DeserializeOwned>(&self, relative: impl AsRef<Path>) -> Result<T> {
        let relative = relative.as_ref();
        let contents = self.read_file(relative)?;
        toml::from_str(&contents).map_err(|e| Error::ParseFile {
            path: relative.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Parse `relative` as TOML, or `None` when it does not exist.
    pub fn read_toml_opt<T: DeserializeOwned>(
        &self,
        relative: impl AsRef<Path>,
    ) -> Result<Option<T>> {
        if !self.has_file(&relative) {
            return Ok(None);
        }
        self.read_toml(relative).map(Some)
    }

    /// Every indexed file path, sorted. Walked lazily on first use.
    pub fn files(&self) -> &[String] {
        self.files.get_or_init(|| self.index())
    }

    fn index(&self) -> Vec<String> {
        let mut files = Vec::new();

        let walker = walkdir::WalkDir::new(&self.source)
            .max_depth(MAX_DEPTH)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                !(entry.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref()))
            });

        for entry in walker.filter_map(std::result::Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&self.source) else {
                continue;
            };
            let relative = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.push(relative);

            if files.len() >= MAX_ENTRIES {
                tracing::warn!(
                    limit = MAX_ENTRIES,
                    "source tree is very large; glob matching only sees the first {MAX_ENTRIES} files"
                );
                break;
            }
        }

        files.sort();
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, contents).unwrap();
        }
        dir
    }

    #[test]
    fn rejects_non_directories() {
        let dir = fixture(&[("a.txt", "hi")]);
        let err = App::new(dir.path().join("a.txt")).unwrap_err();
        assert!(matches!(err, Error::InvalidSource(_)));
    }

    #[test]
    fn globs_match_nested_files() {
        let dir = fixture(&[("src/main.go", ""), ("go.mod", ""), ("README.md", "")]);
        let app = App::new(dir.path()).unwrap();
        assert_eq!(app.find_files("**/*.go").unwrap(), vec!["src/main.go"]);
        assert!(app.has_match("go.mod"));
        assert!(!app.has_match("**/*.rs"));
    }

    #[test]
    fn dependency_directories_are_not_indexed() {
        let dir = fixture(&[
            ("package.json", "{}"),
            ("node_modules/left-pad/index.js", ""),
        ]);
        let app = App::new(dir.path()).unwrap();
        assert_eq!(app.files(), &["package.json".to_string()]);
        // Direct existence checks still work — only the index skips them.
        assert!(app.has_dir("node_modules"));
    }

    #[test]
    fn reads_structured_files() {
        let dir = fixture(&[("package.json", r#"{"name":"demo"}"#)]);
        let app = App::new(dir.path()).unwrap();
        let value: serde_json::Value = app.read_json("package.json").unwrap();
        assert_eq!(value["name"], "demo");
        assert!(app
            .read_json_opt::<serde_json::Value>("missing.json")
            .unwrap()
            .is_none());
    }

    #[test]
    fn malformed_json_reports_the_path() {
        let dir = fixture(&[("package.json", "{oops")]);
        let app = App::new(dir.path()).unwrap();
        let err = app
            .read_json::<serde_json::Value>("package.json")
            .unwrap_err();
        assert!(err.to_string().contains("package.json"), "{err}");
    }
}
