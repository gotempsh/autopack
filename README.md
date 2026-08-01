# autopack

Build container images from source code. No Dockerfile.

autopack inspects a directory, works out what it is, and produces an OCI image
with sensible caching and a runtime image that carries only what the app needs
at run time. It is written in Rust, ships as a single binary, and is usable as a
library.

It is an alternative to [Railpack](https://railpack.com) and
[Nixpacks](https://nixpacks.com), with the same core idea — analyse, plan,
build — and three deliberate differences:

| | autopack | Railpack | Nixpacks |
|---|---|---|---|
| Language | Rust | Go | Rust |
| Build backend | Dockerfile → any BuildKit | custom BuildKit frontend | Dockerfile |
| Runtime resolution | mise | mise | Nix |
| Needs a custom frontend image | no | yes | no |
| Embeddable as a library | yes (`autopack-core`) | via Go packages | limited |

[COMPARISON.md](COMPARISON.md) has measured results for all three on a
third-party corpus — build success, image size, non-root, SIGTERM handling and
cold build times — including the cases where autopack loses.

The backend choice is the important one. Railpack emits BuildKit LLB through its
own frontend image, which buys finer-grained parallelism but means your builder
has to pull `ghcr.io/railwayapp/railpack:frontend` and speak the gateway
protocol. autopack lowers the same plan into a Dockerfile, so it runs unchanged
on `docker build`, `buildx`, `buildctl`, Kaniko, Depot, or any CI system you
already have. A native LLB backend is possible in Rust without a Go frontend — proven with a
working proof of concept in [docs/llb-backend.md](docs/llb-backend.md) — and
the plan format is already backend-agnostic.

## Install

```bash
cargo install --path crates/autopack-cli
```

## Use

```bash
autopack info .                       # what did it detect?
autopack plan .                       # the build plan, as JSON
autopack dockerfile .                 # the generated Dockerfile
autopack build . -t myapp:latest      # build it
autopack build . -t myapp:latest --dry-run   # show the docker command instead
```

```
$ autopack info examples/go-api
Provider:      go
Runtimes:
  go           1.23         (from go.mod)
Detected:
  goVersion      1.23
  mainPackage    .
Steps:
  packages
    $ apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git && rm -rf /var/lib/apt/lists/*
    $ curl -fsSL https://mise.run | MISE_VERSION=v2026.7.18 sh
    $ create /mise/config.toml
    $ mise install && mise reshim
    $ PATH += /mise/shims
  install
    $ go mod download
  build
    $ go build -ldflags='-s -w' -o /app/bin/app .
  runtime
    $ apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
Start command: /app/bin/app
```

Nothing is written into your repository: the generated Dockerfile and its
`.dockerignore` go to a temporary directory and are passed to Docker with `-f`.

## Supported languages

| Provider | Detected by | Handles |
|---|---|---|
| `node` | `package.json` | npm, pnpm, yarn (classic + berry), bun; Next, Nuxt, Remix, SvelteKit, Nest, Astro; Vite/CRA/Astro static builds served by Caddy |
| `deno` | `deno.json`, `deno.lock`, `main.ts` | `deno install`, `deno task build`/`start` |
| `python` | `requirements.txt`, `pyproject.toml`, `Pipfile`, `*.py` | uv, Poetry, Pipenv, pip; Django, FastAPI, Flask entry points |
| `ruby` | `Gemfile`, `config.ru`, `*.rb` | bundler, Rails (asset precompile), Rack |
| `php` | `composer.json`, `index.php` | Composer; Laravel, Symfony, WordPress; served by FrankenPHP |
| `java` | `pom.xml`, `build.gradle(.kts)`, wrappers | Maven and Gradle, wrapper-aware, runnable jar |
| `dotnet` | `*.csproj`, `*.fsproj`, `*.sln` | `dotnet publish`, SDK build image + ASP.NET runtime image |
| `elixir` | `mix.exs` | hex/rebar, Phoenix `assets.deploy`, OTP release |
| `gleam` | `gleam.toml` | `gleam export erlang-shipment` |
| `haskell` | `*.cabal`, `stack.yaml`, `package.yaml` | Cabal and Stack, hpack-aware, binary-only runtime image |
| `swift` | `Package.swift` | SwiftPM, runtime on the `-slim` Swift image |
| `dart` | `pubspec.yaml` | `dart compile exe`, native binary runtime |
| `crystal` | `shard.yml` | shards, linker-resolved runtime libraries |
| `zig` | `build.zig` | `zig build`, binary-only runtime image |
| `cobol` | `*.cbl`, `*.cob` | GnuCOBOL, fixed- and free-format detection |
| `scala` | `build.sbt` | sbt-assembly or sbt-native-packager, JRE runtime |
| `clojure` | `project.clj`, `deps.edn` | Leiningen uberjar or tools.deps, JRE runtime |
| `lunatic` | `Cargo.toml` naming `lunatic` | Rust → `wasm32-wasip1`, Lunatic runtime |
| `go` | `go.mod`, `*.go` | module downloads, `cmd/<name>` layouts, static binary runtime image |
| `rust` | `Cargo.toml` | cargo release builds, `rust-toolchain.toml`, static binary runtime image |
| `cpp` | `CMakeLists.txt`, `Makefile` + sources | CMake and Make builds |
| `procfile` | a `Procfile` with a `web:` line | no build; runs a prebuilt entry point |
| `static` | `index.html` in the root, `public/`, `dist/`, … | Caddy file server |
| `shell` | never — select it explicitly | fully configuration-driven escape hatch |

Every provider also reads a `Procfile`: a `web:` line always wins over the
inferred start command.

Versions come from what the project already declares — `.nvmrc`,
`package.json` engines, `.python-version`, `runtime.txt`, `go.mod`,
`rust-toolchain.toml`, `.ruby-version`, `composer.json`, `pom.xml`,
`<TargetFramework>`, `mix.exs`, `.tool-versions`.

Where a runtime installs cleanly as a binary — Node, Deno, Python, Go, Rust,
Java — it comes from [mise](https://mise.jdx.dev), so any version mise knows
about works without autopack shipping a package set. Where it does not, the
provider builds on the ecosystem's official image instead:

| Provider | Base image | Why not mise |
|---|---|---|
| `ruby` | `ruby:<version>-slim` | mise compiles Ruby from source: 5–10 minutes per cold build |
| `swift` | `swift:<version>` → `swift:<version>-slim` | Apple publishes a matched toolchain/runtime image pair |
| `dart` | `dart:<channel>` → `debian:bookworm-slim` | the Dart SDK is not packaged for mise |
| `crystal` | `crystallang/crystal:<version>` | no mise plugin; the binary needs the builder's libc |
| `php` | `dunglas/frankenphp:1-php<version>` | PHP extensions must be compiled into the interpreter |
| `dotnet` | `mcr.microsoft.com/dotnet/sdk` → `aspnet` | Microsoft publishes matched SDK and runtime images |
| `elixir` | `elixir:<version>` → `debian:bookworm-slim` | mise builds Erlang/OTP from source |
| `gleam` | `ghcr.io/gleam-lang/gleam:v<version>-erlang-slim` | the image pairs each Gleam release with a tested OTP |
| `haskell` | `haskell:<ghc>` → `debian:bookworm-slim` | GHC is a ~2GB install with no reliable mise plugin |
| `scala` | `sbtscala/scala-sbt` → `eclipse-temurin:21-jre` | sbt, the JDK and Scala are versioned as one tag |
| `clojure` | `clojure:<jdk>-lein` / `-tools-deps` → JRE | the official images carry each build tool |

`AUTOPACK_BASE_IMAGE` and `AUTOPACK_RUNTIME_BASE_IMAGE` override both. Base
images must be Debian- or Ubuntu-based, because autopack installs system
packages with apt.

## How it works

```
source dir ──▶ App ──▶ Provider ──▶ BuildContext ──▶ BuildPlan ──▶ backend ──▶ image
              (index)  (detect)     (describe)      (validate)    (lower)
```

The **build plan** is the interface. It is a JSON document of steps, layers,
caches, secrets and a runtime image description:

```json
{
  "steps": [
    {
      "name": "install",
      "inputs": [{ "step": "packages" }, { "local": true, "include": ["package.json", "package-lock.json"] }],
      "commands": [{ "cmd": "sh -c 'npm ci'", "customName": "npm ci" }],
      "caches": ["npm-store"]
    }
  ],
  "caches": { "npm-store": { "directory": "/cache/npm", "type": "shared" } },
  "deploy": {
    "base": { "step": "runtime" },
    "inputs": [{ "step": "build", "include": ["/app"] }],
    "startCommand": "node server.js"
  }
}
```

Each step becomes a build stage. The first input is the stage's base (`FROM`),
later inputs are copied in (`COPY --from=`), caches become
`RUN --mount=type=cache`, secrets become `RUN --mount=type=secret,env=`, and
generated files (a Caddyfile, a `mise.toml`) become heredoc `COPY`s.

Two properties fall out of that structure and matter in practice:

- **Dependency installs are isolated from source.** The install step only
  receives manifests and lockfiles, so editing application code does not
  reinstall `node_modules` or re-resolve a `requirements.txt`.
- **The runtime image is not the build image.** Go and Rust apps ship a runtime
  image with no toolchain and no mise at all; static sites ship a Caddy binary
  and the built assets, with no Node.

## Configuration

Everything autopack infers can be overridden, either with `autopack.json` in the
app root or with `AUTOPACK_*` environment variables. Environment variables win.

```jsonc
{
  "provider": "node",
  "packages": { "node": "22", "python": "3.12" },
  "aptPackages": ["libpq-dev"],
  "deployAptPackages": ["libpq5"],
  "caches": { "assets": { "directory": "/cache/assets", "type": "shared" } },
  "steps": {
    "build": {
      // "..." keeps the generated commands and appends to them.
      "commands": ["...", "npm run build:assets"]
    }
  },
  "deploy": {
    "startCommand": "node dist/server.js",
    "variables": { "LOG_LEVEL": "info" }
  },
  "secrets": ["DATABASE_URL"],
  "exclude": ["docs"]
}
```

| Variable | Effect |
|---|---|
| `AUTOPACK_PROVIDER` | Skip detection and use this provider |
| `AUTOPACK_PACKAGES` | Runtimes to install, e.g. `node@22 python@3.12` |
| `AUTOPACK_INSTALL_CMD` | Replace the install step's commands |
| `AUTOPACK_BUILD_CMD` | Replace the build step's commands |
| `AUTOPACK_START_CMD` | Replace the container start command |
| `AUTOPACK_APT_PACKAGES` | Extra Debian packages in the build image |
| `AUTOPACK_DEPLOY_APT_PACKAGES` | Extra Debian packages in the runtime image |
| `AUTOPACK_BASE_IMAGE` | Base image for build stages |
| `AUTOPACK_RUNTIME_BASE_IMAGE` | Base image for the runtime stage |
| `AUTOPACK_MISE_VERSION` | Pin the mise release |
| `AUTOPACK_USER` | Runtime account name, or `root` to run privileged |
| `AUTOPACK_STATIC_DIR` | Serve this directory as static files |
| `AUTOPACK_SPA` | Fall back to `index.html` for unknown paths |
| `AUTOPACK_PHP_ROOT` | Document root for the PHP provider |
| `AUTOPACK_CPP_BINARY` | Path a `Makefile` build produces |
| `AUTOPACK_DENO_VERSION`, `AUTOPACK_GLEAM_VERSION` | Pin those toolchains |
| `AUTOPACK_CONFIG_FILE` | Read config from this file instead of `autopack.json` |

Secrets are passed by name (`autopack build . --secret DATABASE_URL`), read from
the environment at build time, mounted with `--mount=type=secret`, and never
written into the plan or into an image layer.

## Use as a library

```rust
use autopack_core::{analyze, App, Environment};
use autopack_dockerfile::to_dockerfile;

let app = App::new("./my-app")?;
let analysis = analyze(&app, &Environment::from_process(), &autopack_providers::registry())?;

println!("provider: {}", analysis.provider);
println!("{}", to_dockerfile(&analysis.plan)?);
```

Crates:

- `autopack-core` — plan schema, source analysis, provider trait, config merge.
  No provider or backend dependencies.
- `autopack-providers` — the built-in language providers.
- `autopack-dockerfile` — plan → BuildKit Dockerfile.
- `autopack-cli` — the `autopack` binary.

Adding a provider means implementing one trait:

```rust
impl Provider for MyProvider {
    fn id(&self) -> &'static str { "elixir" }
    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.has_file("mix.exs"))
    }
    fn plan(&self, ctx: &mut BuildContext<'_>) -> Result<()> {
        ctx.packages.add("erlang", "27", "provider default");
        ctx.step(steps::BUILD).add_command(Command::shell("mix release"));
        ctx.set_start_command("/app/_build/prod/rel/app/bin/app start");
        Ok(())
    }
}
```

## When autopack cannot guess

A wrong guess that still builds is worse than a clear failure, so autopack
refuses rather than inventing:

```
error: could not work out how to start this app.
Set a start command with `AUTOPACK_START_CMD=...`, a `web:` line in a Procfile,
or {"deploy": {"startCommand": "..."}} in autopack.json
```

The same applies to Go repositories with several `cmd/` binaries, Cargo
workspace roots with no `[package]`, .NET solutions with no obvious web
project, and Makefile-only C++ builds — each names the alternatives it found
and the variable that resolves it.

When several ecosystems are present, the one that serves traffic wins: a
Laravel app with a Vite asset pipeline is `php`, a Rails app with
`package.json` is `ruby`, a Django app with a webpack build is `python`. Weak
signals never override a manifest — a lone `.py` helper script in a Node
repository does not make it a Python app.

## Reproducible builds

Without a lock, `node = "22"` means "whatever 22.x mise considers latest today"
and `debian:bookworm-slim` means "whatever that tag points at today". Two builds
of the same commit, months apart, can differ in the interpreter, the system
libraries, and every CVE in between.

```bash
autopack lock .        # resolve everything, write autopack.lock, commit it
autopack lock . --update   # deliberately move the pins
```

```json
{
  "version": 1,
  "tools":  { "go": "1.23.12" },
  "images": { "debian:bookworm-slim": "sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818" }
}
```

Every later build pins both halves — `mise.toml` gets `go = "1.23.12"`, and
every `FROM` becomes `image@sha256:…`. **The digest matters more than the
version**: a tag is mutable, so pinning only the runtime version still leaves
the base image free to move underneath you.

Resolution runs inside a container using the same pinned mise release the build
uses, not against whatever is installed locally — otherwise the lock records a
version the build would never have chosen.

This closes the one dimension where Nix-based builders were genuinely ahead: a
Nix pin *is* an exact closure, and until now autopack had no equivalent.

## Security defaults

**Containers run unprivileged.** autopack creates an `autopack` account
(uid 10001) in the runtime image, copies application files with
`COPY --chown`, and emits `USER 10001:10001`. Root in a container turns any
escape or careless bind-mount into host-level access, and it is the first
finding of every image scanner — so it is opt-*out*, not opt-in:

```bash
AUTOPACK_USER=root autopack build .      # only if the image truly needs it
```

Root is genuinely needed for binding a port below 1024 or for a base image
whose entrypoint expects it. Nothing in `examples/` does; all 21 run as
uid 10001. The conformance suite fails a build that runs as root, so this
cannot silently regress.

Build secrets are passed with `--mount=type=secret` and never written to a
layer — also asserted by the conformance suite, which greps every layer of the
saved image for a sentinel value.

## System libraries for native extensions

Most dependencies install a prebuilt wheel or binary and need nothing. A short
tail either compiles C at install time or `dlopen`s a shared library at run
time, and each half needs a *different* Debian package. Getting the split wrong
gives you the worst possible failure: **the build succeeds and the container
dies on first request** with `ImportError: libpq.so.5: cannot open shared
object file`.

autopack maps a declared dependency to both halves:

| Dependency | Build | Runtime |
|---|---|---|
| `psycopg2` (Python) | `libpq-dev`, `build-essential` | `libpq5` |
| `mysqlclient` (Python) | `default-libmysqlclient-dev` | `libmariadb3` |
| `pyodbc`, `pycairo`, … | `-dev` headers | the shared library |
| `pdf2image`, `pydub` | — | `poppler-utils`, `ffmpeg` |
| `canvas` (Node) | Cairo/Pango/JPEG `-dev` | `libcairo2`, `libpango-1.0-0`, … |
| `puppeteer` (Node) | — | the Chromium library closure |
| `node-gyp`, `bcrypt`, `better-sqlite3` | `build-essential`, `python3` | — |

Matching is on whole dependency names, so `psycopg2-binary` — which bundles its
own libpq — correctly pulls nothing.

This list is deliberately short, and it is a convenience rather than a
guarantee. Anything else goes in configuration:

```json
{ "aptPackages": ["libvips-dev"], "deployAptPackages": ["libvips42"] }
```

**How others solve it.** Railpack keeps a similarly small curated map, split the
same way (its own source comments say "we shouldn't handle all cases, but we
attempt"), plus an `aptPackages` escape hatch; it makes Playwright's browser
download opt-in behind an env var rather than guessing. Nixpacks exposes Nix
packages and apt packages through `nixpacks.toml`. Vercel takes the opposite
approach — a fixed build image with a broad preinstalled library set and no way
to install system packages, which is why `sharp` is special-cased by the
platform rather than by the user. autopack follows the Railpack model, because
a self-hosted platform cannot pre-bake every library into one image.

## Conformance

"It builds and returns 200" is the floor. `./scripts/conformance.sh` runs eight
checks per example, each one because it has caught a real defect:

| Check | Why it exists |
|---|---|
| builds | the floor |
| Dockerfile is deterministic | same input twice must give the same bytes |
| no build secret in any layer | `docker save \| grep` for a sentinel passed via `--secret` |
| runtime drops build tooling | a Go image carrying `go`, or any image carrying `/mise` after the provider dropped it, is a silent 100MB+ regression |
| serves expected content | asserts the body, not the status code |
| exits on SIGTERM | measured — see below |
| honours `$PORT` | run with `PORT=8137`; a hard-coded port silently never receives traffic |
| runs unprivileged | `id -u` inside the image must not be 0 |
| source edit reuses the install layer | the entire point of splitting install from build |

The suite found six real defects on its first run, three of them in shipped
providers:

- **SIGTERM was ignored by every image.** `CMD ["/bin/sh","-c",...]` left the
  shell as PID 1, where the kernel discards default-disposition signals, so
  `docker stop` waited the full grace period and then `SIGKILL`ed the app —
  every deploy killing in-flight requests. Fixed with `tini` as the entrypoint
  plus `exec` in the command.
- **PHP: `composer install` failed on every real project.** The FrankenPHP
  image has neither the zip extension nor `unzip`, so `--prefer-dist` dies on
  the first package. The `php-app` example missed it by having no
  `composer.json`.
- **Elixir: `MIX_HOME` pointed at a cache mount.** `mix local.hex` installed
  Hex into a directory that is not part of any layer, so the next step failed
  with "Could not find an SCM for dependency". The same class of bug as
  relocating `CARGO_HOME`.
- **Java shipped a JDK and Maven at runtime** (290MB). Now a JRE image: 113MB.

## Status

v0.1. All 21 apps in `examples/` pass the full conformance suite — 171 checks,
0 failures, every image unprivileged. 144 unit and integration tests, clippy
clean at `-D warnings`.
`examples/README.md` has the per-example detail and image sizes.

Known gaps, in rough priority order:

- **No native BuildKit LLB backend yet.** Steps that could run in parallel are
  serialised by the Dockerfile's linear stage graph. This is *feasible* in
  Rust — see below — but not implemented.
- **Interpreted-language images still carry mise.** Node (132MB), Deno
  (118MB), Python (118MB) and Bun (170MB) keep the whole mise install in the
  runtime image. Go, Rust, C++, Haskell, .NET, Java and Elixir drop their
  toolchain; the interpreted ones could switch to official slim runtime images
  the way Ruby and PHP already do.
- **No dev-dependency pruning** for Node. Correct, but larger than necessary.
- **Bun apps still get Node installed.** Bun is a drop-in replacement, but
  dropping Node would break any dependency whose install script shells out to
  `node`, so the safe default costs ~150MB.
- **PHP extensions are whatever FrankenPHP ships.** An app needing `ext-gd` or
  `ext-intl` has to add them itself.
- **No `HEALTHCHECK`.** Temps reads health config from `.temps.yaml`, but a
  generated image should still declare one for plain `docker run` users.

## Development

```bash
cargo test                                                  # unit + example tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Licence

MIT
