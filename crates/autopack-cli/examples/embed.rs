//! Embedding autopack in another application.
//!
//! This is the whole library surface a host needs — a deployment platform, a
//! CI runner, an internal build service. Run it against any directory:
//!
//! ```console
//! cargo run --example embed -- ./examples/node-express
//! ```
//!
//! Nothing here shells out to the `autopack` binary. The binary is one caller
//! of this API, not a dependency.

use std::path::PathBuf;

use autopack_core::plan::BuildPlan;
use autopack_core::{analyze, App, Environment, Error, Lock, ProviderRegistry};
use autopack_dockerfile::{to_dockerfile, to_dockerignore};

fn main() {
    let path: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".".to_string())
        .into();

    match build_plan_for(&path) {
        Ok(()) => {}
        // Every failure is a typed `autopack_core::Error`, so a host can match
        // on the variant instead of parsing a message. `MissingStartCommand`
        // and `NoProviderDetected` in particular are things a UI wants to
        // present differently from an I/O failure.
        Err(Error::NoProviderDetected) => {
            eprintln!("nothing recognised this app — ask the user which provider to use");
            std::process::exit(1);
        }
        Err(Error::MissingStartCommand) => {
            eprintln!("built fine, but nothing knows how to start it — prompt for a start command");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

fn build_plan_for(path: &PathBuf) -> Result<(), Error> {
    // 1. Open the source directory. Indexing is lazy and happens once.
    let app = App::new(path)?;

    // 2. Build the environment. `from_process()` inherits the caller's
    //    variables; a platform usually wants to construct it explicitly
    //    instead, so a stray AUTOPACK_* on the build host cannot change a
    //    customer's build.
    let mut env = Environment::from_pairs([("NODE_ENV", "production")]);

    // Anything settable in autopack.json is settable here. This is how a
    // platform applies its own UI settings — a build command typed into a
    // form, a pinned runtime, a static output directory.
    env.set("AUTOPACK_PACKAGES", "node@22");

    // Secrets are declared by name only. Values never enter the plan, so a
    // plan is safe to log, cache, or show to a user.
    env.add_secret("DATABASE_URL");

    // 3. Choose the providers. `registry()` is the built-in set; a host can
    //    build its own to add providers or to restrict what is on offer.
    let registry: ProviderRegistry = autopack_providers::registry();

    // 4. Analyse. This is the whole pipeline: detect, plan, validate.
    let analysis = analyze(&app, &env, &registry)?;

    println!("provider:      {}", analysis.provider);
    println!("start command: {:?}", analysis.plan.deploy.start_command);

    // Metadata is what a UI shows: detected framework, package manager,
    // resolved versions and where each came from.
    for (key, value) in &analysis.metadata {
        println!("  {key:<16} {value}");
    }
    for (tool, request) in &analysis.packages {
        println!(
            "  runtime {tool:<8} {} (from {})",
            request.version, request.source
        );
    }

    // 5. Do something with the plan.
    //
    //    The plan is serialisable, so a platform can store it, diff it between
    //    deploys to show what changed, or hand it to a different backend.
    let json: String = analysis.plan.to_json()?;
    let reparsed: BuildPlan = BuildPlan::from_json(&json)?;
    assert_eq!(reparsed, analysis.plan);

    // Lower it. This is the only step that is backend-specific.
    let dockerfile = to_dockerfile(&analysis.plan)?;
    let dockerignore = to_dockerignore(&analysis.plan);

    println!("\nplan:       {} bytes", json.len());
    println!("dockerfile: {} lines", dockerfile.lines().count());
    println!("ignore:     {} entries", dockerignore.lines().count() - 1);

    // 6. Optional: reproducibility. If the app carries an autopack.lock,
    //    `analyze` already applied it — exact runtime versions and
    //    digest-pinned base images. This is just to show it was picked up.
    match Lock::load(app.source())? {
        Some(lock) => println!(
            "\nlocked: {} tools, {} images",
            lock.tools.len(),
            lock.images.len()
        ),
        None => println!("\nno autopack.lock — versions resolve at build time"),
    }

    Ok(())
}
