# Examples

At least one minimal app per provider, plus a second, more realistic app for
the ecosystems where the framework changes the build.
`crates/autopack-dockerfile/tests/examples.rs` checks that every one analyses
and renders — and that no provider is left without an example — while the
commands below are the manual pass that checks they actually *run*.

The simple apps are deliberately dependency-light: they exercise the build
pipeline, not a framework's package graph. The framework apps exist because
"does Rails boot in production mode" is a different question from "does
bundler run".

| Example | Provider | Port | Notes |
|---|---|---|---|
| `node-express` | `node` | 3000 | plain Node HTTP server, no dependencies |
| `vite-spa` | `node` | 3000 | Vite build served by Caddy, SPA fallback |
| `bun-server` | `node` | 3000 | Bun runtime via the `packageManager` pin |
| `deno-api` | `deno` | 3000 | `deno task start` |
| `python-flask` | `python` | 8000 | pip + venv, gunicorn added automatically |
| `python-native` | `python` | 8000 | `psycopg2` — proves `libpq5` reaches the runtime image |
| `ruby-rack` | `ruby` | 3000 | bundler on the official Ruby image |
| `rails-app` | `ruby` | 3000 | Rails 7.1 API-only, boots in `RAILS_ENV=production` |
| `php-app` | `php` | 3000 | FrankenPHP, no Composer manifest |
| `php-composer` | `php` | 3000 | Slim 4 via Composer, PSR-4 autoloading, `public/` root |
| `java-maven` | `java` | 3000 | Maven via mise, runnable jar |
| `dotnet-api` | `dotnet` | 8080 | SDK image builds, ASP.NET image runs |
| `elixir-release` | `elixir` | 3000 | OTP release, no dependencies |
| `elixir-plug` | `elixir` | 3000 | Plug + Bandit from hex, OTP release |
| `gleam-cli` | `gleam` | — | CLI: prints and exits 0 |
| `go-api` | `go` | 3000 | static binary, no toolchain in the runtime image |
| `rust-api` | `rust` | 3000 | static binary, no toolchain in the runtime image |
| `haskell-api` | `haskell` | 3000 | Cabal build, binary-only runtime image |
| `cpp-cmake` | `cpp` | 3000 | CMake build |
| `procfile-app` | `procfile` | 3000 | no build; `deployAptPackages` in `autopack.json` |
| `static-site` | `static` | 3000 | no runtime installed at all |
| `swift-server` | `swift` | 3000 | SwiftPM, `-slim` runtime image |
| `dart-server` | `dart` | 3000 | `dart compile exe`, native binary |
| `crystal-server` | `crystal` | 3000 | shards, linker-resolved runtime libs |
| `zig-server` | `zig` | 3000 | `zig build`, binary-only runtime |
| `cobol-app` | `cobol` | — | GnuCOBOL; prints and exits 0 |

`shell` has no example because it never detects — it is selected explicitly
with `AUTOPACK_PROVIDER=shell` and driven entirely by configuration.

## Verify a build

```bash
cargo build

./target/debug/autopack build examples/go-api -t autopack-go-api:test
docker run --rm -d --name demo -p 18712:3000 autopack-go-api:test
curl -s http://127.0.0.1:18712/     # -> hello from autopack
docker rm -f demo
```

Use the port from the table above; `python-flask` and `dotnet-api` are the two
that do not listen on 3000.

For the SPA, check that deep links do not 404 — that is what the generated
Caddyfile's `try_files` rule is for:

```bash
./target/debug/autopack build examples/vite-spa -t autopack-vite-spa:test
docker run --rm -d --name demo -p 18715:3000 autopack-vite-spa:test
curl -so /dev/null -w '%{http_code}\n' http://127.0.0.1:18715/some/spa/route   # -> 200
docker rm -f demo
```

### Verify all of them

```bash
./scripts/verify-examples.sh          # every example
./scripts/verify-examples.sh rails-app php-composer   # or a subset
```

Most base images come from Docker Hub, so an unauthenticated machine can hit
the pull rate limit part-way through. `docker login` first if the script stops
with `429 Too Many Requests`.

## Verification status

All 28 examples pass the full conformance suite — 222 checks, 0 failures
(2026-08-01, Docker 29.6.1, darwin/arm64). Every image runs as uid 10001, exits
on SIGTERM, honours `$PORT`, leaks no build secret, and reuses its install layer
on a source-only edit. Reproduce with `./scripts/conformance.sh`.

`go-api` additionally carries an `autopack.lock`, so CI exercises the pinned
path: an exact runtime version and a digest-pinned base image.
