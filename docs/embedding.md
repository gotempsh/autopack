# Embedding autopack

For a platform that already has its own build abstraction and wants autopack to
fill in one part of it, rather than take over.

The README covers the basic call. This covers the shape you end up with when
there is an existing system to fit into.

## The seam

Everything autopack does is a pure function of a directory plus an environment:

```
App + Environment + ProviderRegistry  ──analyze──▶  Analysis { provider, plan, metadata, packages }
                                       BuildPlan  ──lower──▶  Dockerfile
```

No global state, no network at plan time, no writes to the source tree. That
matters for a host: `analyze` can run in a request handler to show a user what
*would* happen, long before any build is scheduled.

## Worked example: a host whose interface is "give me a Dockerfile"

Many platforms already have a trait like this — a set of presets, each of which
knows how to produce a Dockerfile and some build arguments:

```rust
#[async_trait]
pub trait Preset {
    fn slug(&self) -> String;
    fn label(&self) -> String;
    async fn dockerfile(&self, config: DockerfileConfig<'_>) -> DockerfileWithArgs;
    fn default_port(&self) -> u16;
    fn static_output_dir(&self) -> Option<String>;
}
```

autopack drops in as one more implementation. The adapter is small because the
plan already carries everything the trait asks for:

```rust
use autopack_core::{analyze, App, Environment};
use autopack_dockerfile::to_dockerfile;

pub struct AutopackPreset;

#[async_trait]
impl Preset for AutopackPreset {
    fn slug(&self) -> String { "autopack".into() }
    fn label(&self) -> String { "Autopack (auto-detect)".into() }

    async fn dockerfile(&self, config: DockerfileConfig<'_>) -> DockerfileWithArgs {
        let app = App::new(config.local_path)?;

        // Map the host's own settings onto autopack's configuration surface.
        // Anything expressible in autopack.json is expressible here, so the
        // platform's UI keeps working unchanged.
        let mut env = Environment::new();
        for pair in config.build_vars.unwrap_or(&Vec::new()) {
            if let Some((key, value)) = pair.split_once('=') {
                env.set(key, value);
            }
        }
        if let Some(cmd) = config.install_command { env.set("AUTOPACK_INSTALL_CMD", cmd); }
        if let Some(cmd) = config.build_command   { env.set("AUTOPACK_BUILD_CMD", cmd); }
        if let Some(dir) = config.output_dir      { env.set("AUTOPACK_STATIC_DIR", dir); }

        let analysis = analyze(&app, &env, &autopack_providers::registry())?;

        // The host wanted a Dockerfile. That is exactly one lowering of the plan.
        DockerfileWithArgs::new(to_dockerfile(&analysis.plan)?)
    }

    fn default_port(&self) -> u16 { 3000 }

    fn static_output_dir(&self) -> Option<String> {
        // Available in metadata when the provider decided to serve statically.
        None
    }
}
```

Two things to notice, because they are the reason this is worth doing rather
than shelling out to a binary:

- **Nothing is written to the source tree.** Some builders emit a Dockerfile
  and supporting files into the checkout, which then has to be cleaned up and
  kept out of the user's repository. `to_dockerfile` returns a `String`.
- **`analyze` is cheap and side-effect free.** A host can call it to populate a
  "we detected Next.js, here is the start command we'll use" screen without
  building anything.

## Requirements the host must meet

- **BuildKit.** Generated Dockerfiles use `# syntax=docker/dockerfile:1.14`,
  cache mounts, secret mounts and heredocs. `DOCKER_BUILDKIT=1`, `docker
  buildx`, or a BuildKit daemon. A classic `docker build` will not parse them.
- **Secrets are passed at build time, by name.** `env.add_secret("DATABASE_URL")`
  puts the *name* in the plan; the host supplies the value with
  `--secret id=DATABASE_URL,env=DATABASE_URL`. Values never enter the plan, so
  plans are safe to log and cache.
- **The `.dockerignore` is separate.** `to_dockerignore(&plan)` returns it.
  BuildKit reads `<dockerfile-name>.dockerignore` from the Dockerfile's
  directory, so writing both to a temp dir and passing `-f` keeps the user's
  repository untouched.

## Reproducibility

If the app has an `autopack.lock`, `analyze` applies it automatically — exact
runtime versions and digest-pinned base images. A platform that wants
reproducible rebuilds should generate one at first deploy and store it
alongside the app:

```rust
use autopack_core::Lock;

if let Some(lock) = Lock::load(app.source())? {
    tracing::info!(tools = lock.tools.len(), images = lock.images.len(), "using locked versions");
}
```

Generating a lock requires resolving versions, which needs a container — see
`autopack lock` in the CLI for the implementation.

## Choosing which providers to offer

`autopack_providers::registry()` is a convenience. A host that wants to expose
a curated subset, or add its own, builds the registry itself:

```rust
let mut registry = ProviderRegistry::new();
registry.register(Box::new(autopack_providers::node::NodeProvider));
registry.register(Box::new(autopack_providers::python::PythonProvider));
registry.register(Box::new(MyInternalProvider));
```

Registration order is detection precedence: the first provider whose `detect`
returns true wins, so more specific providers go first.

## Testing an integration

The `BuildPlan` is `PartialEq` and serialisable, which makes assertions
straightforward without touching Docker:

```rust
let analysis = analyze(&app, &env, &registry)?;
assert_eq!(analysis.provider, "node");
assert_eq!(analysis.plan.deploy.start_command.as_deref(), Some("node server.js"));
```

For end-to-end confidence, `scripts/conformance.sh` in this repository is the
reference: it checks that a built image serves the expected body, exits on
SIGTERM, honours `$PORT`, runs unprivileged, ships no build secret, and reuses
its install layer on a source-only edit. Those are the properties worth
asserting about *any* builder, not just this one.
