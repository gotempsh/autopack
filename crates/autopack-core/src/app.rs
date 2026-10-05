//! Read-only view over the user's source directory.
//!
//! Providers only ever see the app through this type. Keeping file access
//! behind one struct means detection is cheap (the directory is walked once),
//! deterministic (results are sorted), and testable without touching disk
//! layout details in every provider.

#[cfg(test)]
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

/// A separate work budget bounds directory-heavy trees without charging
/// empty directories against the existing indexed-file limit.
const MAX_SCANNED_ENTRIES: usize = 200_000;

/// Acquire the source entry without following a replacement symlink. Its
/// parent is platform-owned, outside the untrusted repository. Once opened,
/// all source operations use this directory capability, not its old pathname.
fn open_source_directory(source: &Path) -> std::io::Result<cap_std::fs::Dir> {
    use cap_fs_ext::DirExt;
    let root = source.ancestors().last().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source has no filesystem root",
        )
    })?;
    let mut directory = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    for component in source.strip_prefix(root).unwrap().components() {
        directory = directory.open_dir_nofollow(component.as_os_str())?;
    }
    Ok(directory)
}

/// A source directory being analysed.
pub struct App {
    source: PathBuf,
    directory: cap_std::fs::Dir,
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
            .map_err(|source_error| Error::ReadFile {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        let directory = open_source_directory(&source).map_err(|source_error| Error::ReadFile {
            path: source.clone(),
            source: source_error,
        })?;
        Ok(Self {
            source,
            directory,
            files: OnceLock::new(),
        })
    }

    /// The directory being analysed.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Absolute path for a path relative to the app root.
    ///
    /// This only joins the paths: the result may be a symlink that leads out of
    /// the source tree. Read through [`App::read_file`], which refuses those.
    pub fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.source.join(relative)
    }

    /// True when `relative` exists inside the source tree and is a file.
    ///
    /// A symlink or `..` path that leads out of the tree counts as absent.
    pub fn has_file(&self, relative: impl AsRef<Path>) -> bool {
        self.directory
            .metadata(relative)
            .is_ok_and(|metadata| metadata.is_file())
    }

    /// True when `relative` exists inside the source tree and is a directory.
    ///
    /// A symlink or `..` path that leads out of the tree counts as absent.
    pub fn has_dir(&self, relative: impl AsRef<Path>) -> bool {
        self.directory
            .metadata(relative)
            .is_ok_and(|metadata| metadata.is_dir())
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
    ///
    /// Refuses absolute paths and symlink targets, or `..` traversal that
    /// leads out of the retained source directory.
    pub fn read_file(&self, relative: impl AsRef<Path>) -> Result<String> {
        let relative = relative.as_ref();
        let read_error = |source| Error::ReadFile {
            path: relative.to_path_buf(),
            source,
        };
        // Resolve and open within the retained directory in one confined
        // operation, including when the source pathname has been moved.
        self.directory.read_to_string(relative).map_err(read_error)
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

        let mut pending = vec![(PathBuf::new(), 0)];
        let mut scanned = 0;
        'walk: while let Some((directory, depth)) = pending.pop() {
            let path = if directory.as_os_str().is_empty() {
                Path::new(".")
            } else {
                &directory
            };
            let Ok(entries) = self.directory.read_dir(path) else {
                continue;
            };
            for entry in entries.filter_map(std::result::Result::ok) {
                scanned += 1;
                if scanned > MAX_SCANNED_ENTRIES {
                    tracing::warn!(limit = MAX_SCANNED_ENTRIES, "source indexing reached its traversal work bound; results may be incomplete");
                    break 'walk;
                }
                let name = entry.file_name();
                let relative = directory.join(&name);
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_file() {
                    if files.len() >= MAX_ENTRIES {
                        tracing::warn!(
                            limit = MAX_ENTRIES,
                            "source indexing reached its file bound"
                        );
                        break 'walk;
                    }
                    files.push(
                        relative
                            .components()
                            .map(|component| component.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/"),
                    );
                } else if kind.is_dir()
                    && depth + 1 < MAX_DEPTH
                    && !SKIP_DIRS.contains(&name.to_string_lossy().as_ref())
                {
                    pending.push((relative, depth + 1));
                }
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

    #[cfg(unix)]
    #[test]
    fn ancestor_entry_swapped_for_a_symlink_cannot_acquire_a_capability() {
        let parent = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::create_dir_all(parent.path().join("ancestor/source")).unwrap();
        fs::create_dir(external.path().join("source")).unwrap();
        let canonical = parent
            .path()
            .join("ancestor/source")
            .canonicalize()
            .unwrap();
        fs::rename(parent.path().join("ancestor"), parent.path().join("parked")).unwrap();
        std::os::unix::fs::symlink(external.path(), parent.path().join("ancestor")).unwrap();
        assert!(super::open_source_directory(&canonical).is_err());
    }

    #[test]
    fn empty_directories_do_not_consume_the_file_index_limit() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..=super::MAX_ENTRIES {
            fs::create_dir(root.path().join(format!("empty-{i}"))).unwrap();
        }
        fs::create_dir(root.path().join("project")).unwrap();
        fs::write(root.path().join("project/manifest.csproj"), "").unwrap();
        assert_eq!(
            App::new(root.path())
                .unwrap()
                .find_files("**/*.csproj")
                .unwrap(),
            ["project/manifest.csproj"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_entry_swapped_for_a_symlink_cannot_acquire_a_capability() {
        let parent = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let root = parent.path().join("source");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap();
        fs::rename(&root, parent.path().join("parked")).unwrap();
        std::os::unix::fs::symlink(external.path(), &root).unwrap();
        assert!(super::open_source_directory(&canonical).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_directory_swaps_never_read_outside_contents() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("config")).unwrap();
        fs::write(root.path().join("config/value"), "inside").unwrap();
        fs::write(outside.path().join("value"), "outside-secret").unwrap();
        let app = App::new(root.path()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let base = root.path().to_path_buf();
        let external = outside.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                fs::rename(base.join("config"), base.join("parked")).unwrap();
                std::os::unix::fs::symlink(&external, base.join("config")).unwrap();
                fs::remove_file(base.join("config")).unwrap();
                fs::rename(base.join("parked"), base.join("config")).unwrap();
            }
        });
        let mut leaked = false;
        for _ in 0..2000 {
            if let Ok(value) = app.read_file("config/value") {
                leaked |= value != "inside";
            }
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(!leaked, "read escaped the retained source directory");
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_source_path_does_not_replace_its_capability() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("source");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("value"), "original").unwrap();
        let app = App::new(&root).unwrap();
        fs::rename(&root, parent.path().join("retained")).unwrap();
        fs::create_dir(&root).unwrap();
        // The replacement deliberately has no matching file.
        assert_eq!(app.read_file("value").unwrap(), "original");
        assert!(app.has_file("value"));
        assert_eq!(app.files(), &["value".to_string()]);
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

    #[cfg(unix)]
    #[test]
    fn links_out_of_the_tree_are_never_read() {
        let dir = fixture(&[("README.md", "")]);
        let outside = fixture(&[("secret.json", r#"{"name":"SECRET"}"#)]);
        std::os::unix::fs::symlink(
            outside.path().join("secret.json"),
            dir.path().join("package.json"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("config")).unwrap();
        let app = App::new(dir.path()).unwrap();

        // A file link out of the tree.
        assert!(!app.has_file("package.json"));
        assert!(app.read_file("package.json").is_err());
        assert!(app.read_file_opt("package.json").unwrap().is_none());
        assert!(app
            .read_json_opt::<serde_json::Value>("package.json")
            .unwrap()
            .is_none());

        // A directory link out of the tree.
        assert!(!app.has_dir("config"));
        assert!(!app.has_file("config/secret.json"));
        assert!(app.read_file("config/secret.json").is_err());

        // A `..` path, with and without a link.
        let escape = format!(
            "../{}/secret.json",
            outside.path().file_name().unwrap().to_string_lossy()
        );
        assert!(!app.has_file(&escape));
        assert!(app.read_file(&escape).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_the_tree_still_resolve() {
        let dir = fixture(&[
            ("packages/web/package.json", r#"{"name":"web"}"#),
            ("packages/web/src/index.js", ""),
        ]);
        std::os::unix::fs::symlink("packages/web/package.json", dir.path().join("package.json"))
            .unwrap();
        std::os::unix::fs::symlink("packages/web/src", dir.path().join("src")).unwrap();
        let app = App::new(dir.path()).unwrap();

        assert!(app.has_file("package.json"));
        let value: serde_json::Value = app.read_json("package.json").unwrap();
        assert_eq!(value["name"], "web");
        assert!(app.has_dir("src"));
        assert!(app.has_file("src/index.js"));
    }
}
