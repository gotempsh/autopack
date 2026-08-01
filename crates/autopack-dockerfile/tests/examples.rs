//! End-to-end checks over the example apps in `examples/`.
//!
//! These assert the whole pipeline — detect, plan, lower — produces a coherent
//! Dockerfile for each ecosystem autopack claims to support. They do not run
//! Docker; `examples/README.md` documents the manual build-and-run pass.

use std::path::PathBuf;

use autopack_core::{analyze, App, Environment};
use autopack_dockerfile::{to_dockerfile, to_dockerignore};

/// Every example, the provider that must claim it, and a fragment its start
/// command must contain.
const EXAMPLES: &[(&str, &str, &str)] = &[
    ("node-express", "node", "node server.js"),
    ("vite-spa", "node", "caddy run"),
    ("bun-server", "node", "bun run server.ts"),
    ("deno-api", "deno", "deno task start"),
    ("python-flask", "python", "gunicorn app:app"),
    ("python-native", "python", "gunicorn app:app"),
    ("ruby-rack", "ruby", "rackup"),
    ("ruby-native", "ruby", "rackup"),
    ("rails-app", "ruby", "rails server"),
    ("php-app", "php", "frankenphp run"),
    ("php-composer", "php", "frankenphp run"),
    ("php-extensions", "php", "frankenphp run"),
    ("java-maven", "java", "java -jar"),
    ("scala-app", "scala", "java -jar"),
    ("clojure-app", "clojure", "java -jar"),
    ("dotnet-api", "dotnet", "dotnet /app/out/Api.dll"),
    ("elixir-release", "elixir", "/app/release/bin/hello start"),
    ("elixir-plug", "elixir", "/app/release/bin/demo start"),
    ("gleam-cli", "gleam", "erlang-shipment/entrypoint.sh"),
    ("go-api", "go", "/app/bin/app"),
    ("rust-api", "rust", "/app/bin/app"),
    ("lunatic-app", "lunatic", "lunatic run"),
    ("haskell-api", "haskell", "/app/bin/haskell-api"),
    ("swift-server", "swift", "/app/bin/app"),
    ("dart-server", "dart", "/app/bin/app"),
    ("zig-server", "zig", "/app/bin/app"),
    ("crystal-server", "crystal", "/app/bin/app"),
    ("cobol-app", "cobol", "/app/bin/app"),
    ("cpp-cmake", "cpp", "/app/bin/app"),
    ("procfile-app", "procfile", "http.server"),
    ("static-site", "static", "caddy run"),
];

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .canonicalize()
        .expect("examples directory should exist")
}

fn analyse(example: &str) -> autopack_core::Analysis {
    let app = App::new(examples_dir().join(example)).expect("example should be a directory");
    analyze(&app, &Environment::new(), &autopack_providers::registry())
        .unwrap_or_else(|error| panic!("analysing {example} failed: {error}"))
}

#[test]
fn every_example_produces_a_buildable_dockerfile() {
    for (example, provider, start_fragment) in EXAMPLES {
        let analysis = analyse(example);
        assert_eq!(&analysis.provider, provider, "{example}");

        let start = analysis
            .plan
            .deploy
            .start_command
            .as_deref()
            .unwrap_or_default();
        assert!(
            start.contains(start_fragment),
            "{example}: start command `{start}` should contain `{start_fragment}`"
        );

        let dockerfile = to_dockerfile(&analysis.plan)
            .unwrap_or_else(|error| panic!("{example}: rendering failed: {error}"));

        assert!(dockerfile.starts_with("# syntax="), "{example}");
        assert!(dockerfile.contains("CMD ["), "{example}");
        assert!(
            !dockerfile.contains("FROM  "),
            "{example}: empty FROM reference"
        );
        assert!(
            to_dockerignore(&analysis.plan).contains(".git"),
            "{example}: .git should be excluded from the build context"
        );
    }
}

#[test]
fn there_is_an_example_for_every_detecting_provider() {
    // `shell` is configuration-only and never detects, so it has no example.
    let covered: Vec<&str> = EXAMPLES.iter().map(|(_, provider, _)| *provider).collect();
    for provider in autopack_providers::registry().ids() {
        if provider == "shell" {
            continue;
        }
        assert!(
            covered.contains(&provider),
            "provider `{provider}` has no example app"
        );
    }
}

#[test]
fn plans_round_trip_through_json() {
    for (example, _, _) in EXAMPLES {
        let analysis = analyse(example);
        let json = analysis.plan.to_json().unwrap();
        let parsed = autopack_core::BuildPlan::from_json(&json)
            .unwrap_or_else(|error| panic!("{example}: reparsing failed: {error}"));
        assert_eq!(parsed, analysis.plan, "{example}");
    }
}

#[test]
fn compiled_languages_leave_the_toolchain_behind() {
    // A Go runtime image that still carries mise would be ~400MB of nothing.
    for example in [
        "go-api",
        "rust-api",
        "cpp-cmake",
        "elixir-release",
        "dotnet-api",
    ] {
        let analysis = analyse(example);
        let dockerfile = to_dockerfile(&analysis.plan).unwrap();
        let runtime = dockerfile
            .split("# ---- runtime image ----")
            .nth(1)
            .expect("a runtime section");
        assert!(!runtime.contains("/mise"), "{example}: {runtime}");
    }
}

#[test]
fn no_runtime_image_copies_a_directory_its_build_never_created() {
    // Copying `/mise` out of a packages step that only ran apt fails the build
    // with a checksum error that says nothing about mise.
    for (example, _, _) in EXAMPLES {
        let analysis = analyse(example);
        let installs_runtimes = !analysis.packages.is_empty();
        let runtime = analysis
            .plan
            .step("runtime")
            .unwrap_or_else(|| panic!("{example}: no runtime step"));

        let copies_mise = runtime.inputs.iter().any(|input| {
            input
                .filter
                .include
                .iter()
                .any(|path| path.contains("mise"))
        });

        assert!(
            !copies_mise || installs_runtimes,
            "{example}: runtime copies /mise but no runtime was installed"
        );
    }
}
