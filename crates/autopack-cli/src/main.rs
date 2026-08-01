//! `autopack` — build container images from source, with no Dockerfile.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use autopack_core::{analyze, Analysis, App, Environment, Lock};
use autopack_dockerfile::{to_dockerfile, to_dockerignore};

#[derive(Parser)]
#[command(
    name = "autopack",
    version,
    about = "Build container images from source, with no Dockerfile",
    long_about = "autopack inspects a source directory, works out how to build and run it, \
                  and produces a container image.\n\n\
                  Configure it with an autopack.json file or AUTOPACK_* environment variables."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Increase log verbosity. Repeat for more detail.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Subcommand)]
enum Commands {
    /// Show what autopack detected: provider, runtimes, and start command.
    Info(CommonArgs),

    /// Print the build plan as JSON.
    Plan {
        #[command(flatten)]
        common: CommonArgs,

        /// Write to this file instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Print the generated Dockerfile.
    Dockerfile {
        #[command(flatten)]
        common: CommonArgs,

        /// Write to this file instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Build an image with Docker BuildKit.
    Build {
        #[command(flatten)]
        common: CommonArgs,

        /// Image tag, e.g. `registry.example.com/api:latest`.
        #[arg(short, long)]
        tag: Option<String>,

        /// Target platform, e.g. `linux/amd64`.
        #[arg(long)]
        platform: Option<String>,

        /// Push the image after building.
        #[arg(long)]
        push: bool,

        /// Ignore all build caches.
        #[arg(long)]
        no_cache: bool,

        /// Print the docker command instead of running it.
        #[arg(long)]
        dry_run: bool,
    },

    /// Resolve every fuzzy version to an exact one and write autopack.lock.
    ///
    /// Run this once, commit the result, and every later build of the same
    /// commit uses the same interpreter and the same base image bytes.
    Lock {
        #[command(flatten)]
        common: CommonArgs,

        /// Re-resolve everything instead of keeping existing pins.
        #[arg(long)]
        update: bool,
    },

    /// List the registered providers in detection order.
    Providers,
}

#[derive(Args)]
struct CommonArgs {
    /// Directory to analyse.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Name of a secret to expose to the build. Its value is read from the
    /// environment variable of the same name and never written to the plan.
    #[arg(long = "secret", value_name = "NAME")]
    secrets: Vec<String>,

    /// Extra `KEY=VALUE` build variable, repeatable.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
}

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    if let Err(error) = run(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("autopack={level}")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Info(common) => {
            let analysis = analyse(&common)?;
            print_info(&analysis);
            Ok(())
        }
        Commands::Plan { common, output } => {
            let analysis = analyse(&common)?;
            emit(output.as_deref(), &analysis.plan.to_json()?)
        }
        Commands::Dockerfile { common, output } => {
            let analysis = analyse(&common)?;
            emit(output.as_deref(), &to_dockerfile(&analysis.plan)?)
        }
        Commands::Build {
            common,
            tag,
            platform,
            push,
            no_cache,
            dry_run,
        } => {
            let analysis = analyse(&common)?;
            build(&common, &analysis, tag, platform, push, no_cache, dry_run)
        }
        Commands::Lock { common, update } => {
            let analysis = analyse(&common)?;
            write_lock(&common, &analysis, update)
        }
        Commands::Providers => {
            for id in autopack_providers::registry().ids() {
                println!("{id}");
            }
            Ok(())
        }
    }
}

/// Resolve fuzzy versions and image digests, then write `autopack.lock`.
///
/// Resolution deliberately happens inside a container rather than against a
/// local mise install: the answer must be the one the *build* would get, and a
/// developer's machine is not the build environment.
fn write_lock(common: &CommonArgs, analysis: &Analysis, update: bool) -> Result<()> {
    let path = common.path.join(autopack_core::lock::LOCK_FILE);
    let mut lock = if update {
        Lock::new()
    } else {
        Lock::load(&common.path)?.unwrap_or_else(Lock::new)
    };

    for (tool, request) in &analysis.packages {
        if !update && lock.tool(tool).is_some() {
            continue;
        }
        eprintln!("resolving {tool}@{}...", request.version);
        let exact = resolve_tool_version(tool, &request.version)?;
        lock.set_tool(tool, exact);
    }

    for image in plan_images(&analysis.plan) {
        if !update && lock.images.contains_key(&image) {
            continue;
        }
        eprintln!("resolving {image}...");
        match resolve_image_digest(&image) {
            Ok(digest) => lock.set_image(&image, digest),
            // A private or unreachable registry should not block locking the
            // rest; the image simply stays on its mutable tag.
            Err(error) => eprintln!("warning: could not pin {image}: {error}"),
        }
    }

    fs::write(&path, lock.to_json()?)
        .with_context(|| format!("cannot write `{}`", path.display()))?;
    eprintln!(
        "wrote {} ({} tools, {} images)",
        path.display(),
        lock.tools.len(),
        lock.images.len()
    );
    Ok(())
}

/// Every distinct image reference a plan pulls from.
fn plan_images(plan: &autopack_core::BuildPlan) -> Vec<String> {
    let mut images: Vec<String> = Vec::new();
    let layers = plan
        .steps
        .iter()
        .flat_map(|step| step.inputs.iter())
        .chain(std::iter::once(&plan.deploy.base))
        .chain(plan.deploy.inputs.iter());

    for layer in layers {
        if let Some(image) = &layer.image {
            if !images.contains(image) {
                images.push(image.clone());
            }
        }
    }
    images
}

/// Ask mise which exact version a specification resolves to.
fn resolve_tool_version(tool: &str, version: &str) -> Result<String> {
    let script = format!(
        // The slim base has no curl, and resolution must use the same mise
        // release the build will use — otherwise the lock records a version
        // the build would not have chosen.
        "set -e; apt-get update >/dev/null 2>&1; \
         apt-get install -y --no-install-recommends ca-certificates curl >/dev/null 2>&1; \
         curl -fsSL https://mise.run | MISE_VERSION={mise} sh >/dev/null 2>&1; \
         /usr/local/bin/mise latest {tool}@{version}",
        mise = autopack_core::mise::DEFAULT_MISE_VERSION,
    );

    let output = ProcessCommand::new("docker")
        .args([
            "run",
            "--rm",
            "-e",
            "MISE_INSTALL_PATH=/usr/local/bin/mise",
            autopack_core::generate::DEFAULT_BASE_IMAGE,
            "sh",
            "-c",
            &script,
        ])
        .output()
        .context("could not run `docker` to resolve versions")?;

    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || resolved.is_empty() {
        bail!(
            "mise could not resolve {tool}@{version}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(resolved)
}

/// The manifest digest a tag currently points at.
fn resolve_image_digest(image: &str) -> Result<String> {
    let output = ProcessCommand::new("docker")
        .args([
            "buildx",
            "imagetools",
            "inspect",
            "--format",
            "{{.Manifest.Digest}}",
            image,
        ])
        .output()
        .context("could not run `docker buildx imagetools`")?;

    let digest = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || !digest.starts_with("sha256:") {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(digest)
}

fn analyse(common: &CommonArgs) -> Result<Analysis> {
    let app = App::new(&common.path)
        .with_context(|| format!("cannot analyse `{}`", common.path.display()))?;

    let mut env = Environment::from_process();
    for pair in &common.env {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("--env expects KEY=VALUE, got `{pair}`"))?;
        env.set(key, value);
    }
    for secret in &common.secrets {
        env.add_secret(secret);
    }

    Ok(analyze(&app, &env, &autopack_providers::registry())?)
}

fn print_info(analysis: &Analysis) {
    println!("Provider:      {}", analysis.provider);

    if !analysis.packages.is_empty() {
        println!("Runtimes:");
        for (name, request) in &analysis.packages {
            println!(
                "  {name:<12} {:<12} (from {})",
                request.version, request.source
            );
        }
    }

    if !analysis.metadata.is_empty() {
        println!("Detected:");
        for (key, value) in &analysis.metadata {
            if key == "provider" {
                continue;
            }
            println!("  {key:<14} {value}");
        }
    }

    println!("Steps:");
    for step in &analysis.plan.steps {
        let commands = step
            .commands
            .iter()
            .map(|command| command.display_name())
            .collect::<Vec<_>>();
        println!("  {}", step.name);
        for command in commands {
            println!("    $ {command}");
        }
    }

    if let Some(start) = &analysis.plan.deploy.start_command {
        println!("Start command: {start}");
    }
}

fn emit(output: Option<&Path>, contents: &str) -> Result<()> {
    match output {
        Some(path) => {
            fs::write(path, contents)
                .with_context(|| format!("cannot write `{}`", path.display()))?;
            eprintln!("wrote {}", path.display());
        }
        None => print!("{contents}"),
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build(
    common: &CommonArgs,
    analysis: &Analysis,
    tag: Option<String>,
    platform: Option<String>,
    push: bool,
    no_cache: bool,
    dry_run: bool,
) -> Result<()> {
    let dockerfile = to_dockerfile(&analysis.plan)?;
    let dockerignore = to_dockerignore(&analysis.plan);

    // The Dockerfile lives outside the source tree so autopack never writes
    // into the user's repository. BuildKit reads `<dockerfile>.dockerignore`
    // from the same directory, which keeps the exclude list working too.
    let scratch = tempfile::tempdir().context("cannot create a temporary directory")?;
    let dockerfile_path = scratch.path().join("Dockerfile");
    fs::write(&dockerfile_path, &dockerfile).context("cannot write the generated Dockerfile")?;
    fs::write(
        scratch.path().join("Dockerfile.dockerignore"),
        &dockerignore,
    )
    .context("cannot write the generated .dockerignore")?;

    let context_path = common
        .path
        .canonicalize()
        .unwrap_or_else(|_| common.path.clone());

    let mut args: Vec<String> = vec![
        "buildx".into(),
        "build".into(),
        "--file".into(),
        dockerfile_path.display().to_string(),
    ];

    if let Some(tag) = &tag {
        args.push("--tag".into());
        args.push(tag.clone());
    }
    if let Some(platform) = &platform {
        args.push("--platform".into());
        args.push(platform.clone());
    }
    if no_cache {
        args.push("--no-cache".into());
    }
    if push {
        if tag.is_none() {
            bail!("--push needs an image tag; pass --tag <name>");
        }
        args.push("--push".into());
    } else if tag.is_some() {
        // Without an explicit output the image stays in the build cache and
        // `docker run` cannot see it, which is a confusing way to "succeed".
        args.push("--load".into());
    }

    for secret in &analysis.plan.secrets {
        if std::env::var_os(secret).is_none() {
            eprintln!("warning: secret `{secret}` was requested but is not set in the environment");
            continue;
        }
        args.push("--secret".into());
        args.push(format!("id={secret},env={secret}"));
    }

    args.push(context_path.display().to_string());

    if dry_run {
        // Keep the scratch directory: a printed command that points at a
        // deleted Dockerfile is worse than useless.
        let kept = scratch.keep();
        println!("docker {}", args.join(" "));
        println!("\n# Dockerfile written to {}", kept.display());
        println!("\n--- Dockerfile ---\n{dockerfile}");
        return Ok(());
    }

    let status = ProcessCommand::new("docker")
        .args(&args)
        .stdin(Stdio::null())
        .status()
        .context(
            "could not run `docker`. Install Docker, or use `autopack dockerfile` \
             and build with another tool",
        )?;

    if !status.success() {
        bail!("docker build failed with {status}");
    }

    if let Some(tag) = tag {
        eprintln!("built {tag}");
    }
    Ok(())
}
