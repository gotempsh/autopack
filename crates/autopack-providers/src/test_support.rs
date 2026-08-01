//! Fixture helpers shared by provider tests.

use std::fs;

use autopack_core::{analyze, Analysis, App, Environment, Result};

/// Write `files` into a temporary directory and open it as an [`App`].
///
/// The [`tempfile::TempDir`] is returned so the caller keeps it alive — dropping
/// it deletes the fixture out from under the test.
pub fn write_app(files: &[(&str, &str)]) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }
    let app = App::new(dir.path()).unwrap();
    (dir, app)
}

/// Analyse `app` with the built-in registry and an empty environment.
pub fn plan_for(app: &App) -> Analysis {
    try_plan_for(app).expect("analysis should succeed")
}

/// Analyse `app`, returning the error instead of panicking.
pub fn try_plan_for(app: &App) -> Result<Analysis> {
    analyze(app, &Environment::new(), &crate::registry())
}

/// Analyse `app` with `env` applied.
pub fn plan_with_env(app: &App, env: &[(&str, &str)]) -> Result<Analysis> {
    analyze(
        app,
        &Environment::from_pairs(env.to_vec()),
        &crate::registry(),
    )
}
