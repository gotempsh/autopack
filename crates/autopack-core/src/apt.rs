//! Shell snippets for apt that keep working on end-of-life Debian releases.
//!
//! Official language images pin the Debian release they were built on, so an
//! app that pins an old interpreter patch (`ruby '3.1.2'`) gets an image based
//! on a Debian release that has since left the regular mirrors. Its
//! `apt-get update` then fails with "does not have a Release file", or the
//! index refers to security packages that now 404. Once a release reaches end
//! of life Debian serves it from `archive.debian.org` instead, so pointing apt
//! there is what keeps those builds installable.

/// Debian releases that are no longer served by the regular mirrors.
///
/// Releases are added here once their long-term support ends.
pub const END_OF_LIFE_DEBIAN: &[&str] = &["jessie", "stretch", "buster", "bullseye"];

/// A shell command that refreshes the apt index, switching an end-of-life
/// Debian base image to `archive.debian.org` first.
///
/// The switch runs in a subshell so sourcing `/etc/os-release` cannot leak
/// variables such as `VERSION` into the rest of the command. Only the main
/// suite is configured: security suites of recently archived releases are not
/// always on the archive yet, and a missing suite would fail the whole update.
pub fn update_command() -> String {
    let releases = END_OF_LIFE_DEBIAN.join("|");
    format!(
        "(if [ -r /etc/os-release ]; then . /etc/os-release; \
           case \"${{VERSION_CODENAME:-}}\" in {releases}) \
             echo \"autopack: Debian $VERSION_CODENAME is end-of-life; installing packages from archive.debian.org\" >&2; \
             printf 'deb http://archive.debian.org/debian %s main\\n' \"$VERSION_CODENAME\" > /etc/apt/sources.list; \
             rm -f /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; \
             printf 'Acquire::Check-Valid-Until \"false\";\\n' > /etc/apt/apt.conf.d/99autopack-archive ;; \
           esac; \
         fi) && apt-get update"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_switches_only_end_of_life_releases_to_the_archive() {
        let command = update_command();
        assert!(command.ends_with("apt-get update"));
        assert!(command.contains("jessie|stretch|buster|bullseye)"));
        assert!(command.contains("archive.debian.org/debian %s main"));
        // Current releases fall through the `case` untouched.
        assert!(!command.contains("bookworm"));
        assert!(!command.contains("trixie"));
    }
}
