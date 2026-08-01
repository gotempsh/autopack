# nixpacks vs railpack vs autopack

A measured comparison on a third-party corpus. Every number here comes from a
script in `scripts/`, run against applications this repository did not write.

## Read this first

**I wrote autopack.** A self-comparison is worth distrusting by default, so the
method is built to be checkable rather than persuasive:

- The corpus is [`temps-examples`](https://github.com/gotempsh/temps-examples)
  (`examples/starters`) — 13 applications written by someone else, **for
  nixpacks**. Several still contain
  `nixpacks.toml` files. If the corpus is biased, it is biased *against*
  autopack.
- Every failure below is reported with the failing tool's own error text, and
  each one was investigated individually. Where the fault turned out to be the
  application's rather than the builder's, it is excluded from the score and
  documented in [App-side failures](#app-side-failures).
- Both comparison scripts are in this repo. Re-run them.

**Caveats that materially limit these results:**

| Limitation | Consequence |
|---|---|
| 13 applications | Small sample. Treat gaps of one or two as noise. |
| `darwin/arm64`, Docker 29.6.1 | At least one nixpacks failure is arm64-specific. No amd64 run. |
| railpack built from source (`version dev`), nixpacks 1.40.0 | railpack is pre-release; expect it to improve. |
| Corpus skews to web services | Nothing here tests batch jobs, workers, or monorepos. |

## Method

```bash
# Outcomes: builds, serves, size, uid, SIGTERM
./scripts/compare-builders.sh path/to/temps-examples/examples/starters > results.tsv

# Cold-cache build times (caches cleared before every single build)
./scripts/compare-timing.sh path/to/temps-examples/examples/starters
```

Each application is built, then run with `PORT` injected, then probed on
3000/8000/8080, then stopped with `docker stop -t 12` to time SIGTERM handling.
`uid` comes from `id -u` inside the image.

Timings are measured separately, with each builder's cache cleared before every
build. Base images are *not* pruned — pulling Debian is a property of the
machine, not the builder. The timings in the outcome sweep are meaningless
(whichever builder ran last inherits warm layers) and are not reported.

## Results

| | nixpacks | railpack | autopack |
|---|---|---|---|
| Built | 8 / 13 | 9 / 13 | **13 / 13** |
| Built *and* served traffic | 6 | 7 | **12** |
| Mean image size | 351 MB | 127 MB | **108 MB** |
| Runs as non-root | 0 / 8 | 0 / 9 | **13 / 13** |
| Ignored SIGTERM (killed after 10s) | 1 | 2 | **0** |

Cold-cache build time, seconds, on the four apps all three can build:

| App | nixpacks | railpack | autopack |
|---|---|---|---|
| nodejs/express | 35 | 23 | **21** |
| python/fastapi | 42 | 24 | **20** |
| vite/react | 67 | 21 | **19** |
| php/vanilla | 43 | 14 | **6** |

Per-application detail:

| App | nixpacks | railpack | autopack |
|---|---|---|---|
| go/net-http | ✗ toolchain | ✓ 39MB | ✓ 33MB |
| go/gin | ✗ toolchain | ✗ go.sum | ✓ 34MB |
| nodejs/express | ✓ 257MB | ✓ 138MB | ✓ 133MB |
| nodejs/fastify | ✓ 258MB | ✓ 139MB | ✓ 134MB |
| bun/bun-server | ✓ 256MB, no serve | ✓ 175MB | ✓ 170MB |
| deno | ✗ not detected | ✗ not detected | ✓ 118MB |
| python/flask | ✓ 377MB | ✓ 115MB, no serve | ✓ 118MB |
| python/fastapi | ✓ 386MB | ✓ 124MB | ✓ 128MB |
| python/django | ✓ 486MB, no serve | ✓ 129MB, no serve | ✓ 131MB |
| rust/actix | ✗ build failed | ✗ manifest parse | ✓ 33MB |
| php/vanilla | ✓ 391MB, slow stop | ✓ 234MB, slow stop | ✓ 192MB |
| java/spring-boot | ✗ not detected | ✗ not detected | ✓ builds¹ |
| vite/react | ✓ 397MB | ✓ 53MB, slow stop | ✓ 47MB |

¹ Builds a correct fat jar; the application itself refuses to start. See below.

## What the differences actually are

**Non-root is the clearest gap.** Every nixpacks and railpack image in this run
runs as uid 0. Every autopack image runs as uid 10001. This is a deliberate
autopack default rather than a bug in the others — but for a self-hosted
platform it is the difference between a container escape being contained and
being a host compromise.

**SIGTERM handling is the most under-discussed.** `php/vanilla` under both
nixpacks and railpack, and `vite/react` under railpack, took the full 12-second
grace period and then died to `SIGKILL`. The cause is generic: the start
command runs under a shell that becomes PID 1, and the kernel discards
default-disposition signals sent to PID 1. **autopack had exactly this bug**
until a conformance check caught it; it is fixed with `tini` plus `exec`. Any
platform running these images is hard-killing in-flight requests on every
deploy.

**Image size** is mostly Nix. nixpacks averages 351 MB against 108–127 MB for
the two mise-based builders, because the Nix store lands in the runtime image.

**Runtime version resolution** explains both Go failures. `go.mod` declares
`go 1.24`; the Nix package set ships an older Go, so the toolchain
auto-download fires and fails on arm64. mise installs the requested version
directly. This is architectural, not a bug — Nix pins for reproducibility,
which is a genuine advantage autopack does not match (see below).

**Entry-point detection** explains the rest. railpack ran
`gunicorn ... main:app` against an app whose module is `app.py`
(`ModuleNotFoundError: No module named 'main'`). autopack reads the file. Both
tools missed the bare `main.ts` Deno app and the `build.gradle`-without-wrapper
Spring app.

## Where autopack loses

Publishing only the wins would make this document useless.

- **One provider short of nixpacks.** Since this run autopack added Swift,
  Dart, Crystal, Zig, COBOL, Scala, Clojure and Lunatic — 16 to 24 providers
  against nixpacks' 23. Only **Scheme (Haunt)** is missing, and it is a
  deliberate omission: Haunt is not packaged for Debian, so nixpacks can
  support it only because Nix packages it. Building it from source would mean
  compiling Guile plus three unpackaged Guile libraries. Use the `shell`
  provider if you need it.
- **No native BuildKit LLB backend.** railpack emits LLB through its own
  frontend and can execute independent steps concurrently. autopack lowers to a
  linear Dockerfile. This is now known to be *feasible* in Rust without a Go
  frontend ([docs/llb-backend.md](docs/llb-backend.md)) but it is not built.
- **The version lock is new and lightly exercised.** `autopack lock` pins exact
  runtime versions and base image digests, which closes the reproducibility gap
  Nix held — but it has days of use behind it, against years for Nix pinning.
- **Least battle-tested by far.** nixpacks has run in production at Railway for
  years. autopack is days old, and this comparison is 13 applications on one
  machine, measured by its own author.
- **Timing gaps are small and noisy.** Three of the four cold-build
  measurements are within a few seconds. Only `php/vanilla` (6s vs 14s vs 43s)
  is a clear separation, and one sample is not a benchmark.

## App-side failures

Four corpus applications cannot be built by *any* tool. They are excluded from
the scores above; each was checked individually rather than assumed:

| App | Problem |
|---|---|
| `nextjs/app-router` | No `app/layout.tsx`; `next build` errors "doesn't have a root layout" |
| `nodejs/nestjs` | `start` is `node dist/main` but there is no build script to produce `dist/` |
| `php/laravel` | Composer scripts call `artisan`; no `artisan` file, no `public/index.php` |
| `ruby/rails` | `Gemfile` pins `ruby "3.4.0"`; that patch's slim image is no longer published, so bundler exits 18 on a version mismatch |

Five more directories are not applications at all — `astro`, `nuxt` and
`sveltekit` have no `package.json`, `dotnet/web` has no `.csproj`, and
`elixir/phoenix` has no `mix.exs`.

`java/spring-boot` is the subtlest: `application.yaml` sets
`spring.main.web-application-type: reactive` while `build.gradle` declares only
servlet Tomcat, so Spring refuses to start. autopack's jar is correct — 31
libraries including `tomcat-embed-core`, no WebFlux. The build is fine; the app
is not.

**This is worth acting on independently of any builder choice.** Nine of 25
directories in a repository advertised as "minimal, working application[s] —
fork any example, connect it to your Temps instance, and deploy" are not
deployable.

## Reproducing

```bash
docker run --rm --privileged -d --name buildkit moby/buildkit:latest   # railpack
cargo build                                                            # autopack
./scripts/compare-builders.sh <corpus>
./scripts/compare-timing.sh <corpus>
```

Versions used: nixpacks 1.40.0 (Homebrew), railpack `dev` (source),
autopack 0.1.0, Docker 29.6.1, darwin/arm64, 2026-08-01.
