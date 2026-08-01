//! Reading a `Procfile`.
//!
//! A Procfile declares named processes. `web` is the one that serves traffic
//! and becomes the container's start command; everything else describes work
//! the platform runs separately — a `release` task before traffic switches, a
//! `worker` alongside it.
//!
//! Discarding those is not harmless. The usual workaround is to fold a
//! migration into the start command:
//!
//! ```text
//! web: python manage.py migrate && gunicorn app.wsgi
//! ```
//!
//! which runs the migration in *every* replica on *every* restart, racing
//! them against each other. Keeping `release` separate is what lets a platform
//! run it once, before the new version takes traffic.

use indexmap::IndexMap;

use crate::app::App;
use crate::error::Result;

/// The process name that serves HTTP traffic.
pub const WEB: &str = "web";

/// The process name conventionally run once before a release goes live.
pub const RELEASE: &str = "release";

/// Every process declared in a `Procfile`, in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Procfile {
    processes: IndexMap<String, String>,
}

impl Procfile {
    /// Read the `Procfile` in `app`, if there is one.
    pub fn load(app: &App) -> Result<Option<Self>> {
        Ok(app.read_file_opt("Procfile")?.map(|c| Self::parse(&c)))
    }

    /// Parse Procfile contents.
    pub fn parse(contents: &str) -> Self {
        let mut processes = IndexMap::new();

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((name, command)) = line.split_once(':') else {
                continue;
            };

            let name = name.trim();
            let command = command.trim();
            // A process name is a bare word; anything else is a stray line
            // rather than a declaration.
            let valid_name = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');

            if valid_name && !command.is_empty() {
                processes.insert(name.to_ascii_lowercase(), command.to_string());
            }
        }

        Self { processes }
    }

    /// The `web` process, which becomes the container's start command.
    pub fn web(&self) -> Option<&str> {
        self.processes.get(WEB).map(String::as_str)
    }

    /// Every process that is not `web`.
    ///
    /// These are what a platform runs separately: `release` before a deploy
    /// goes live, `worker` and friends as their own containers.
    pub fn tasks(&self) -> impl Iterator<Item = (&str, &str)> {
        self.processes
            .iter()
            .filter(|(name, _)| name.as_str() != WEB)
            .map(|(name, command)| (name.as_str(), command.as_str()))
    }

    /// The `release` process, if declared.
    pub fn release(&self) -> Option<&str> {
        self.processes.get(RELEASE).map(String::as_str)
    }

    /// True when the file declared nothing usable.
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_web_from_everything_else() {
        let procfile = Procfile::parse(
            "web: gunicorn app:app\nrelease: python manage.py migrate\nworker: celery -A app worker\n",
        );

        assert_eq!(procfile.web(), Some("gunicorn app:app"));
        assert_eq!(procfile.release(), Some("python manage.py migrate"));
        assert_eq!(
            procfile.tasks().collect::<Vec<_>>(),
            vec![
                ("release", "python manage.py migrate"),
                ("worker", "celery -A app worker"),
            ]
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let procfile = Procfile::parse("# the web process\n\nweb: ./server\n");
        assert_eq!(procfile.web(), Some("./server"));
        assert_eq!(procfile.tasks().count(), 0);
    }

    #[test]
    fn commands_containing_colons_survive() {
        // `--bind 0.0.0.0:8000` must not be treated as a second separator.
        let procfile = Procfile::parse("web: gunicorn app:app --bind 0.0.0.0:8000\n");
        assert_eq!(procfile.web(), Some("gunicorn app:app --bind 0.0.0.0:8000"));
    }

    #[test]
    fn process_names_are_case_insensitive() {
        let procfile = Procfile::parse("Web: ./server\nRelease: ./migrate\n");
        assert_eq!(procfile.web(), Some("./server"));
        assert_eq!(procfile.release(), Some("./migrate"));
    }

    #[test]
    fn malformed_lines_are_ignored_rather_than_parsed_as_processes() {
        let procfile = Procfile::parse("this is not a declaration\nweb: ./server\n: empty\nx:\n");
        assert_eq!(procfile.web(), Some("./server"));
        assert_eq!(procfile.tasks().count(), 0);
    }

    #[test]
    fn a_procfile_with_only_tasks_has_no_web() {
        let procfile = Procfile::parse("worker: ./worker\n");
        assert_eq!(procfile.web(), None);
        assert_eq!(procfile.tasks().count(), 1);
    }
}
