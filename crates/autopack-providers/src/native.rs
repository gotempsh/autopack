//! System libraries required by packages with native extensions.
//!
//! Most dependency managers install prebuilt wheels or binaries and need
//! nothing from the system. A short tail does not: it either compiles C at
//! install time (needing `-dev` headers) or `dlopen`s a shared library at run
//! time (needing the runtime package). Getting this wrong produces the two
//! worst failure shapes a build tool has:
//!
//! * missing build headers — the install fails with a compiler error about a
//!   header nobody has heard of;
//! * missing runtime library — the *build succeeds* and the container dies on
//!   first request with `ImportError: libpq.so.5: cannot open shared object`.
//!
//! The second is why build and runtime packages are tracked separately rather
//! than dumped into one list.
//!
//! This map is deliberately short. It covers packages that are both common and
//! genuinely broken without help; anything else is the user's call via
//! `aptPackages` / `deployAptPackages`. Railpack takes the same position — its
//! own comment reads "we shouldn't handle all cases, but we attempt".

/// A package that needs system libraries, and which ones.
pub struct NativeDependency {
    /// Substring matched against the manifest, e.g. `psycopg2`.
    pub package: &'static str,
    /// Packages needed to compile it.
    pub build: &'static [&'static str],
    /// Packages the compiled artefact loads at run time.
    pub runtime: &'static [&'static str],
}

/// Chromium's shared library closure, needed by headless browsers.
pub(crate) const CHROMIUM_RUNTIME: &[&str] = &[
    "libnss3",
    "libnspr4",
    "libatk1.0-0",
    "libatk-bridge2.0-0",
    "libcups2",
    "libdrm2",
    "libxkbcommon0",
    "libxcomposite1",
    "libxdamage1",
    "libxfixes3",
    "libxrandr2",
    "libgbm1",
    "libasound2",
    "libpango-1.0-0",
    "libcairo2",
];

/// Node packages that need system libraries.
pub const NODE: &[NativeDependency] = &[
    // node-canvas compiles against Cairo and friends; there are no prebuilt
    // binaries for most platforms.
    NativeDependency {
        package: "canvas",
        build: &[
            "build-essential",
            "libcairo2-dev",
            "libpango1.0-dev",
            "libjpeg-dev",
            "libgif-dev",
            "librsvg2-dev",
            "python3",
        ],
        runtime: &[
            "libcairo2",
            "libpango-1.0-0",
            "libjpeg62-turbo",
            "libgif7",
            "librsvg2-2",
        ],
    },
    // node-gyp shells out to a C toolchain and python3.
    NativeDependency {
        package: "node-gyp",
        build: &["build-essential", "python3"],
        runtime: &[],
    },
    NativeDependency {
        package: "better-sqlite3",
        build: &["build-essential", "python3"],
        runtime: &[],
    },
    NativeDependency {
        package: "bcrypt",
        build: &["build-essential", "python3"],
        runtime: &[],
    },
    NativeDependency {
        package: "pg-native",
        build: &["build-essential", "libpq-dev"],
        runtime: &["libpq5"],
    },
];

/// Python packages that need system libraries.
pub const PYTHON: &[NativeDependency] = &[
    // `psycopg2-binary` bundles libpq; plain `psycopg2` links against the
    // system one and fails at import time without it.
    NativeDependency {
        package: "psycopg2",
        build: &["build-essential", "libpq-dev"],
        runtime: &["libpq5"],
    },
    NativeDependency {
        package: "psycopg",
        build: &["build-essential", "libpq-dev"],
        runtime: &["libpq5"],
    },
    NativeDependency {
        package: "mysqlclient",
        build: &[
            "build-essential",
            "default-libmysqlclient-dev",
            "pkg-config",
        ],
        runtime: &["libmariadb3"],
    },
    NativeDependency {
        package: "pycairo",
        build: &["build-essential", "libcairo2-dev", "pkg-config"],
        runtime: &["libcairo2"],
    },
    NativeDependency {
        package: "pyodbc",
        build: &["build-essential", "unixodbc-dev"],
        runtime: &["unixodbc"],
    },
    NativeDependency {
        package: "pdf2image",
        build: &[],
        runtime: &["poppler-utils"],
    },
    NativeDependency {
        package: "pydub",
        build: &[],
        runtime: &["ffmpeg"],
    },
    NativeDependency {
        package: "moviepy",
        build: &[],
        runtime: &["ffmpeg"],
    },
    NativeDependency {
        package: "weasyprint",
        build: &[],
        runtime: &["libpango-1.0-0", "libpangoft2-1.0-0", "libharfbuzz0b"],
    },
];

/// Ruby gems that need system libraries.
///
/// Gems are the worst offender of the three ecosystems: `bundle install`
/// compiles C for anything with a native extension, and the official
/// `ruby:*-slim` images ship no development headers at all.
pub const RUBY: &[NativeDependency] = &[
    NativeDependency {
        package: "pg",
        build: &["libpq-dev"],
        runtime: &["libpq5"],
    },
    NativeDependency {
        package: "mysql2",
        build: &["default-libmysqlclient-dev"],
        runtime: &["libmariadb3"],
    },
    NativeDependency {
        package: "sqlite3",
        build: &["libsqlite3-dev"],
        runtime: &["libsqlite3-0"],
    },
    NativeDependency {
        package: "ffi",
        build: &["libffi-dev"],
        runtime: &["libffi8"],
    },
    // Nokogiri vendors and statically links its own libxml2, so only the
    // source-build path needs headers and the runtime needs nothing.
    NativeDependency {
        package: "nokogiri",
        build: &["libxml2-dev", "libxslt1-dev"],
        runtime: &[],
    },
    NativeDependency {
        package: "rmagick",
        build: &["libmagickwand-dev"],
        runtime: &["imagemagick"],
    },
    NativeDependency {
        package: "charlock_holmes",
        build: &["libicu-dev"],
        runtime: &["libicu72"],
    },
    NativeDependency {
        package: "curb",
        build: &["libcurl4-openssl-dev"],
        runtime: &["libcurl4"],
    },
];

/// The build and runtime packages needed for everything `manifest` declares.
///
/// `manifest` is the raw text of the dependency files. Matching on text rather
/// than a parsed graph is crude but robust across the five Python and four
/// JavaScript manifest formats, and a false positive costs a few megabytes
/// while a false negative costs a production crash.
pub fn required_packages(manifest: &str, table: &[NativeDependency]) -> (Vec<String>, Vec<String>) {
    let haystack = manifest.to_ascii_lowercase();
    let mut build = Vec::new();
    let mut runtime = Vec::new();

    for dependency in table {
        if !mentions(&haystack, dependency.package) {
            continue;
        }
        for package in dependency.build {
            if !build.iter().any(|existing| existing == package) {
                build.push((*package).to_string());
            }
        }
        for package in dependency.runtime {
            if !runtime.iter().any(|existing| existing == package) {
                runtime.push((*package).to_string());
            }
        }
    }

    (build, runtime)
}

/// Whether `needle` appears as a whole dependency name.
///
/// A substring test would match `psycopg2` inside `psycopg2-binary`, which
/// bundles its own libpq and needs nothing — and would pull `libpq-dev` into
/// every build that uses the wheel.
pub(crate) fn mentions(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(index, _)| {
        let before = haystack[..index].chars().next_back();
        let after = haystack[index + needle.len()..].chars().next();

        let boundary_before = before.is_none_or(|c| !c.is_ascii_alphanumeric());
        // `-` and `_` continue a package name (`psycopg2-binary`), so they are
        // not a boundary; a version specifier or quote is.
        let boundary_after =
            after.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'));

        boundary_before && boundary_after
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psycopg2_pulls_libpq_at_both_stages() {
        let (build, runtime) = required_packages("flask==3.0\npsycopg2==2.9.9\n", PYTHON);
        assert!(build.contains(&"libpq-dev".to_string()));
        assert!(runtime.contains(&"libpq5".to_string()));
    }

    #[test]
    fn the_binary_wheel_needs_nothing() {
        // psycopg2-binary bundles libpq. Pulling libpq-dev for it would add a
        // compiler and headers to every build that already had a wheel.
        let (build, runtime) = required_packages("psycopg2-binary==2.9.9\n", PYTHON);
        assert!(build.is_empty(), "{build:?}");
        assert!(runtime.is_empty(), "{runtime:?}");
    }

    #[test]
    fn pyproject_style_declarations_match() {
        let manifest = r#"dependencies = ["psycopg2 (>=2.9)", "pdf2image"]"#;
        let (_, runtime) = required_packages(manifest, PYTHON);
        assert!(runtime.contains(&"libpq5".to_string()));
        assert!(runtime.contains(&"poppler-utils".to_string()));
    }

    #[test]
    fn node_canvas_needs_a_toolchain_and_runtime_libraries() {
        let (build, runtime) = required_packages(r#"{"dependencies":{"canvas":"^2.11"}}"#, NODE);
        assert!(build.contains(&"libcairo2-dev".to_string()));
        assert!(runtime.contains(&"libcairo2".to_string()));
        // The -dev package must not leak into the runtime image.
        assert!(!runtime.iter().any(|p| p.ends_with("-dev")), "{runtime:?}");
    }

    #[test]
    fn unrelated_manifests_add_nothing() {
        let (build, runtime) = required_packages(r#"{"dependencies":{"express":"^4"}}"#, NODE);
        assert!(build.is_empty());
        assert!(runtime.is_empty());
    }

    #[test]
    fn duplicate_requirements_are_merged() {
        let (build, _) = required_packages("bcrypt\nbetter-sqlite3\n", NODE);
        let toolchains = build.iter().filter(|p| *p == "build-essential").count();
        assert_eq!(toolchains, 1, "{build:?}");
    }
}
