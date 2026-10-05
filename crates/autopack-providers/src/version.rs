//! Version requirements as manifests write them, resolved against the
//! versions an official image actually publishes.
//!
//! Gemfiles, composer.json and mix.exs all state *ranges* (`~> 1.14`,
//! `^7.4 || ^8.0`, `'>= 3.2', '< 4.0'`). Reading only the lower bound picks the
//! oldest version the app tolerates, which is the one least likely to work: a
//! lockfile written on a newer interpreter, a dependency option the old
//! toolchain does not know, or an image tag that was never published. Picking
//! the newest published version inside the range matches what the developer
//! most likely ran.

use std::cmp::Ordering;

/// A version with an optional patch component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    /// Major component.
    pub major: u64,
    /// Minor component (0 when the manifest omitted it).
    pub minor: u64,
    /// Patch component, when the manifest wrote one.
    pub patch: Option<u64>,
}

impl Version {
    /// Parse `3`, `3.2`, `3.2.1`, ignoring a leading `v` and trailing
    /// qualifiers such as Ruby's `p20` or `-rc1`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches(['v', 'V']);
        let mut parts = text.split('.');
        let major = leading_number(parts.next()?)?;
        let minor = match parts.next() {
            Some(part) => leading_number(part)?,
            None => 0,
        };
        let patch = parts.next().and_then(leading_number);
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// Whether the text spelled out all three components (`3.1.2`).
    fn is_full(text: &str) -> bool {
        let text = text.trim().trim_start_matches(['v', 'V']);
        let parts: Vec<&str> = text.split('.').collect();
        parts.len() >= 3 && parts[..3].iter().all(|part| leading_number(part).is_some())
    }

    fn key(self, missing_patch: u64) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch.unwrap_or(missing_patch))
    }
}

fn leading_number(part: &str) -> Option<u64> {
    let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    NotMinor,
    NotPatch,
    NotMajor,
    Gt,
    Ge,
    Lt,
    Le,
    /// `~>` (Ruby, Elixir), `~` (Composer): `~> 3.2` is `>= 3.2, < 4.0`,
    /// `~> 3.2.1` is `>= 3.2.1, < 3.3`.
    Pessimistic,
    /// Composer `^`: `^8.1` is `>= 8.1, < 9.0`.
    Caret,
    /// Composer `8.2.*`, or a bare `8.2` in a range: any patch of that minor.
    Minor,
}

#[derive(Debug, Clone, Copy)]
struct Bound {
    op: Op,
    version: Version,
}

impl Bound {
    /// Whether the newest patch of `major.minor` satisfies this bound.
    ///
    /// Image tags like `ruby:3.2-slim` resolve to the newest patch, so that is
    /// the version a candidate actually stands for.
    fn admits_minor(&self, major: u64, minor: u64) -> bool {
        let candidate = (major, minor, u64::MAX);
        let low = self.version.key(0);
        match self.op {
            Op::NotPatch => true,
            Op::NotMinor => major != self.version.major || minor != self.version.minor,
            Op::NotMajor => major != self.version.major,
            Op::Eq => {
                self.version.patch.is_none()
                    && self.version.major == major
                    && self.version.minor == minor
            }
            Op::Gt => candidate > low,
            Op::Ge => candidate >= low,
            Op::Lt => candidate.cmp(&low) == Ordering::Less,
            Op::Le => candidate <= self.version.key(u64::MAX),
            Op::Minor => self.version.major == major && self.version.minor == minor,
            Op::Caret => candidate >= low && major == self.version.major,
            Op::Pessimistic => {
                candidate >= low
                    && if self.version.patch.is_some() {
                        major == self.version.major && minor == self.version.minor
                    } else {
                        major == self.version.major
                    }
            }
        }
    }
}

/// A requirement: any of several alternatives, each a set of bounds that must
/// all hold.
#[derive(Debug, Clone)]
pub struct Requirement {
    alternatives: Vec<Vec<Bound>>,
    exact: Option<String>,
}

impl Requirement {
    /// Parse a requirement in Ruby, Composer or Elixir syntax.
    ///
    /// Alternatives are separated by `||`, `|` or ` or `; bounds within one by
    /// commas, whitespace or ` and `. A bare full version (`3.1.2`) is an exact
    /// pin. Returns `None` when nothing in the text is a version.
    pub fn parse(text: &str) -> Option<Self> {
        let normalised = text
            .replace("||", "|")
            .replace(" or ", "|")
            .replace(" and ", ",");
        let mut alternatives = Vec::new();
        let mut exact = None;
        for alternative in normalised.split('|') {
            let alternative = expand_hyphen_range(alternative);
            let mut bounds = Vec::new();
            for token in split_bounds(&alternative) {
                if token == "*" || token == "x" {
                    // Anything goes.
                    bounds.push(Bound {
                        op: Op::Ge,
                        version: Version {
                            major: 0,
                            minor: 0,
                            patch: None,
                        },
                    });
                    continue;
                }
                let (op, rest) = split_operator(&token);
                let wildcard = rest.ends_with(".*") || rest.ends_with(".x");
                let rest = rest.trim_end_matches(".*").trim_end_matches(".x");
                let Some(version) = Version::parse(rest) else {
                    continue;
                };
                let op = match op {
                    Some(op) => op,
                    None if wildcard => Op::Minor,
                    None if Version::is_full(rest) => {
                        exact = Some(rest.trim().to_string());
                        Op::Eq
                    }
                    None => Op::Minor,
                };
                let op = if op == Op::NotMinor && Version::is_full(rest) {
                    Op::NotPatch
                } else if op == Op::NotMinor && !rest.contains('.') {
                    Op::NotMajor
                } else if op == Op::Eq && wildcard {
                    Op::Minor
                } else {
                    op
                };
                if op == Op::Eq && Version::is_full(rest) {
                    exact = Some(rest.trim().to_string());
                }
                bounds.push(Bound { op, version });
            }
            if !bounds.is_empty() {
                alternatives.push(bounds);
            }
        }
        if alternatives.is_empty() {
            return None;
        }
        // Only a single, sole exact bound is an exact pin.
        let single = alternatives.len() == 1 && alternatives[0].len() == 1;
        Some(Self {
            exact: if single { exact } else { None },
            alternatives,
        })
    }

    /// The exact `major.minor.patch` this requirement pins, if it pins one.
    pub fn exact(&self) -> Option<&str> {
        self.exact.as_deref()
    }

    /// Whether the newest patch of `candidate` (`"3.2"`) satisfies the
    /// requirement.
    pub fn admits(&self, candidate: &str) -> bool {
        let Some(version) = Version::parse(candidate) else {
            return false;
        };
        self.alternatives.iter().any(|bounds| {
            bounds
                .iter()
                .all(|bound| bound.admits_minor(version.major, version.minor))
        })
    }

    /// The newest of `candidates` (`major.minor` strings, any order) that the
    /// requirement admits.
    pub fn newest<'a>(&self, candidates: &[&'a str]) -> Option<&'a str> {
        candidates
            .iter()
            .copied()
            .filter(|candidate| self.admits(candidate))
            .max_by_key(|candidate| {
                Version::parse(candidate).map(|version| (version.major, version.minor))
            })
    }
}

/// Rewrite a Composer hyphen range (`8.1 - 8.3`) as the bounds it means
/// (`>=8.1, <=8.3`). A partial upper bound covers every patch of it, which
/// `<=` with a missing patch already does.
fn expand_hyphen_range(alternative: &str) -> String {
    match alternative.split_once(" - ") {
        Some((low, high)) => format!(">={},<={}", low.trim(), high.trim()),
        None => alternative.to_string(),
    }
}

/// Split one alternative into bound tokens, keeping operators attached to
/// their versions (`>= 3.2` is one token).
fn split_bounds(alternative: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut pending_operator = String::new();
    for raw in alternative.split([',', ' ', '\t']) {
        let raw = raw.trim().trim_matches(['"', '\'']);
        if raw.is_empty() {
            continue;
        }
        if raw
            .chars()
            .all(|c| matches!(c, '>' | '<' | '=' | '~' | '^' | '!'))
        {
            pending_operator.push_str(raw);
            continue;
        }
        tokens.push(format!("{pending_operator}{raw}"));
        pending_operator.clear();
    }
    tokens
}

fn split_operator(token: &str) -> (Option<Op>, &str) {
    for (prefix, op) in [
        ("!=", Op::NotMinor),
        ("~>", Op::Pessimistic),
        (">=", Op::Ge),
        ("<=", Op::Le),
        ("==", Op::Eq),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Eq),
        ("~", Op::Pessimistic),
        ("^", Op::Caret),
    ] {
        if let Some(rest) = token.strip_prefix(prefix) {
            return (Some(op), rest.trim());
        }
    }
    (None, token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluding_a_patch_keeps_later_patches_in_the_minor_available() {
        assert_eq!(
            Requirement::parse("^8.4 !=8.4.0")
                .unwrap()
                .newest(&["8.2", "8.3", "8.4"]),
            Some("8.4")
        );
    }

    #[test]
    fn composer_excluded_minor_and_major_are_not_selected() {
        assert_eq!(
            Requirement::parse("^8.2 !=8.4.*")
                .unwrap()
                .newest(&["8.2", "8.3", "8.4"]),
            Some("8.3")
        );
        assert!(!Requirement::parse(">=7 !=8.*").unwrap().admits("8.4"));
    }

    const MINORS: &[&str] = &["3.0", "3.1", "3.2", "3.3", "3.4", "4.0"];

    fn newest(text: &str) -> Option<&'static str> {
        Requirement::parse(text).and_then(|req| req.newest(MINORS))
    }

    #[test]
    fn ranges_resolve_to_the_newest_admitted_minor() {
        assert_eq!(newest(">= 3.2, < 4.0"), Some("3.4"));
        assert_eq!(newest(">= 3.2"), Some("4.0"));
        assert_eq!(newest("~> 3.2"), Some("3.4"));
        assert_eq!(newest("~> 3.2.0"), Some("3.2"));
        assert_eq!(newest("< 3.2.5"), Some("3.1"));
        assert_eq!(newest("*"), Some("4.0"));
    }

    #[test]
    fn composer_syntax() {
        let php = ["8.2", "8.3", "8.4"];
        let pick = |text: &str| Requirement::parse(text).and_then(|req| req.newest(&php));
        assert_eq!(pick("^7.4 || ^8.0"), Some("8.4"));
        assert_eq!(pick("^8.2|^8.3"), Some("8.4"));
        assert_eq!(pick(">=8.1 <8.4"), Some("8.3"));
        assert_eq!(pick("~8.2.0"), Some("8.2"));
        assert_eq!(pick("8.3.*"), Some("8.3"));
        assert_eq!(pick("^7.4"), None);
        assert_eq!(pick("8.1 - 8.3"), Some("8.3"));
        assert_eq!(pick("7.4 - 8.2.5 || ^8.4"), Some("8.4"));
        assert_eq!(pick("8.1 - 8.2"), Some("8.2"));
    }

    #[test]
    fn elixir_syntax() {
        let elixir = ["1.14", "1.15", "1.16", "1.17", "1.18"];
        let pick = |text: &str| Requirement::parse(text).and_then(|req| req.newest(&elixir));
        assert_eq!(pick("~> 1.14"), Some("1.18"));
        assert_eq!(pick("~> 1.15.0"), Some("1.15"));
        assert_eq!(pick("~> 1.15 or ~> 1.16"), Some("1.18"));
        assert_eq!(pick(">= 1.14.0 and < 1.17.0"), Some("1.16"));
    }

    #[test]
    fn a_bare_full_version_is_an_exact_pin() {
        let req = Requirement::parse("3.1.2").unwrap();
        assert_eq!(req.exact(), Some("3.1.2"));
        assert_eq!(
            Requirement::parse("= 3.1.2").unwrap().exact(),
            Some("3.1.2")
        );
        assert_eq!(Requirement::parse("3.1").unwrap().exact(), None);
        assert_eq!(Requirement::parse(">= 3.1.2").unwrap().exact(), None);
        assert_eq!(Requirement::parse("3.1.2 || 3.2.0").unwrap().exact(), None);
    }

    #[test]
    fn versions_ignore_qualifiers() {
        assert_eq!(
            Version::parse("3.4.9p0"),
            Some(Version {
                major: 3,
                minor: 4,
                patch: Some(9)
            })
        );
        assert_eq!(Version::parse("garbage"), None);
        assert!(Requirement::parse("no version here").is_none());
    }
}
