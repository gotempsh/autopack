//! # autopack-providers
//!
//! The built-in language and framework providers.
//!
//! ```no_run
//! use autopack_core::{analyze, App, Environment};
//!
//! let app = App::new(".")?;
//! let analysis = analyze(&app, &Environment::from_process(), &autopack_providers::registry())?;
//! println!("detected {}", analysis.provider);
//! # Ok::<(), autopack_core::Error>(())
//! ```

#![deny(missing_docs)]

pub mod clojure;
pub mod cobol;
pub mod cpp;
pub mod crystal;
pub mod dart;
pub mod deno;
pub mod dotnet;
pub mod elixir;
pub mod gleam;
pub mod golang;
pub mod haskell;
pub mod java;
pub mod lunatic;
pub mod native;
pub mod node;
pub mod php;
pub mod procfile;
pub mod python;
pub mod ruby;
pub mod rust;
pub mod scala;
pub mod shell;
pub mod staticfile;
pub mod support;
pub mod swift;
pub mod zig;

#[cfg(test)]
mod test_support;

use autopack_core::ProviderRegistry;

/// Every built-in provider, in detection order.
///
/// Order is precedence, and the ordering rule is "the ecosystem that runs the
/// server wins". Real repositories mix manifests constantly:
///
/// * A Laravel app has `composer.json` *and* `package.json` for its assets.
/// * A Rails app has a `Gemfile` *and* `package.json`.
/// * A Phoenix app has `mix.exs` *and* `package.json`.
/// * A Django app has `requirements.txt` *and* `package.json`.
/// * A Deno app may have a `package.json` for npm interop.
///
/// In every one of those, the JavaScript is a build-time asset pipeline, not
/// the thing that serves traffic — so those providers are registered ahead of
/// `node`. `procfile` and `static` are last-resort providers for repositories
/// with no manifest at all, and `shell` never detects.
pub fn registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();

    // Ecosystems that commonly carry a package.json for assets.
    registry.register(Box::new(deno::DenoProvider));
    registry.register(Box::new(php::PhpProvider));
    registry.register(Box::new(ruby::RubyProvider));
    registry.register(Box::new(elixir::ElixirProvider));
    registry.register(Box::new(gleam::GleamProvider));
    registry.register(Box::new(java::JavaProvider));
    registry.register(Box::new(scala::ScalaProvider));
    registry.register(Box::new(clojure::ClojureProvider));
    registry.register(Box::new(dotnet::DotnetProvider));
    registry.register(Box::new(haskell::HaskellProvider));
    registry.register(Box::new(swift::SwiftProvider));
    registry.register(Box::new(dart::DartProvider));
    registry.register(Box::new(crystal::CrystalProvider));
    registry.register(Box::new(zig::ZigProvider));
    registry.register(Box::new(python::PythonProvider));

    registry.register(Box::new(node::NodeProvider));
    registry.register(Box::new(golang::GoProvider));
    registry.register(Box::new(lunatic::LunaticProvider));
    registry.register(Box::new(rust::RustProvider));
    registry.register(Box::new(cpp::CppProvider));
    registry.register(Box::new(cobol::CobolProvider));

    // Last resorts, for repositories with no recognisable manifest.
    registry.register(Box::new(procfile::ProcfileProvider));
    registry.register(Box::new(staticfile::StaticProvider));

    // Configuration-only; never detects.
    registry.register(Box::new(shell::ShellProvider));

    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use autopack_core::{App, Environment};

    #[test]
    fn every_provider_has_a_unique_id() {
        let registry = registry();
        let mut ids = registry.ids();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate provider ids: {ids:?}");
    }

    #[test]
    fn covers_the_expected_ecosystems() {
        let mut ids = registry().ids();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "clojure", "cobol", "cpp", "crystal", "dart", "deno", "dotnet", "elixir", "gleam",
                "go", "haskell", "java", "lunatic", "node", "php", "procfile", "python", "ruby",
                "rust", "scala", "shell", "static", "swift", "zig",
            ]
        );
    }

    #[test]
    fn shell_never_detects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "").unwrap();
        let app = App::new(dir.path()).unwrap();

        let registry = registry();
        let provider = registry.detect(&app, &Environment::new()).unwrap().unwrap();
        assert_eq!(provider.id(), "static");
    }
}
