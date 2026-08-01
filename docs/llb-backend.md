# A native LLB backend in Rust

**Question:** BuildKit is Go, and railpack's LLB frontend is Go. Can autopack
have a native LLB backend without leaving Rust?

**Answer: yes, and it does not require writing a frontend at all.** This is
verified with a working proof of concept, not reasoned about.

## The misconception worth clearing up first

Railpack ships a *gateway frontend*: a container image
(`ghcr.io/railwayapp/railpack:frontend`) that BuildKit invokes over gRPC, which
returns an LLB graph. That is one way to reach LLB, and it is the one that
forces a Go dependency and a published image.

It is not the only way. **BuildKit's `buildctl` reads a marshalled LLB
definition straight from stdin** when no frontend is specified. Any language
that can emit the protobuf can drive BuildKit directly:

```
your-program | buildctl build
```

No frontend image, no gateway protocol, no Go.

## Proof of concept

A Rust binary emitting LLB, solved by BuildKit:

```rust
use buildkit_llb::prelude::*;

fn main() {
    let base = Source::image("debian:bookworm-slim");
    let command = Command::run("/bin/sh")
        .args(&["-c", "echo hello-from-rust-llb > /out.txt && cat /out.txt"])
        .mount(Mount::Layer(OutputIdx(0), base.output(), "/"));

    Terminal::with(command.output(0))
        .write_definition(std::io::stdout())
        .unwrap();
}
```

```console
$ ./llbprobe > /tmp/llb.pb          # 546 bytes of protobuf
$ docker exec -i buildkit buildctl build --no-cache < /tmp/llb.pb
#1 docker-image://docker.io/library/debian:bookworm-slim
#1 DONE 1.9s

#2 /bin/sh -c echo hello-from-rust-llb > /out.txt && cat /out.txt
#2 0.044 hello-from-rust-llb
#2 DONE 0.2s
```

BuildKit pulled the image, executed the step, and produced the output — driven
entirely by a Rust-generated graph.

## What this would buy

The Dockerfile backend loses two things to the linear stage graph:

1. **Parallelism.** `BuildPlan` already models dependencies precisely, so
   independent steps *could* run concurrently. A Dockerfile serialises them
   into whatever order the stages are written.
2. **Filter fidelity.** `Layer.exclude` currently lowers to
   `RUN rm -rf …` after the copy, because `COPY --exclude` only exists on the
   labs frontend. LLB's `FileOp` expresses include/exclude directly.

The plan format was designed for this: `to_dockerfile()` is one lowering, and
an `to_llb()` would be another over the same `BuildPlan`.

## What makes it real work rather than a weekend

- **`buildkit-llb` 0.2.0 is unmaintained** — last released 2020. It still
  compiles on Rust 1.97 and the proof above uses it, but shipping on an
  abandoned crate for the core build path is a poor trade. The realistic route
  is generating the types with `prost` from BuildKit's own
  `solver/pb/ops.proto`, which is stable and versioned.
- **Cache mounts and secrets** are `ExecOp` mount variants
  (`CacheOpt`, `SecretOpt`). The plan already carries both, so this is
  mapping rather than design.
- **Exporting** an image means talking to the BuildKit `Solve` API with an
  exporter attribute, or shelling out to `buildctl --output`. The former needs
  a `tonic` gRPC client; the latter works today.
- **`buildctl` must be reachable.** The Dockerfile backend runs anywhere Docker
  does. An LLB backend needs a BuildKit endpoint — which is exactly the
  portability cost that made the Dockerfile the default in the first place.

## Recommendation

Keep the Dockerfile backend as the default and portable path. Add LLB as an
*opt-in* second backend (`autopack build --backend=llb`) behind the same
`BuildPlan`, using `prost`-generated types rather than `buildkit-llb`.

The proof of concept says the risk is in the mapping work, not in whether Rust
can do it at all.

## Reproducing

```bash
docker run --rm --privileged -d --name buildkit moby/buildkit:latest
cargo new llbprobe && cd llbprobe && cargo add buildkit-llb
# paste the program above into src/main.rs
cargo run | docker exec -i buildkit buildctl build --no-cache
```

---

# Writing our own LLB library

**Status: analysis only. The Dockerfile backend remains the only one shipped.**
This section exists to answer "could we own this layer?" before anyone commits
to it.

## Why our own rather than `buildkit-llb`

The proof of concept above uses `buildkit-llb` 0.2.0. It works, but:

- Last released **2020**. It predates `FileOp` reaching general use, and
  BuildKit has had five years of protocol additions since.
- It has no cache-mount or secret-mount support in its `Mount` enum, which are
  two of the four things autopack's plan actually needs.
- Basing the core build path on an abandoned crate means the first protocol
  change is an emergency.

The alternative is not "write a BuildKit client from scratch". LLB is **just a
protobuf message**. The whole surface we need is one `.proto` file that
BuildKit versions and maintains.

## What LLB actually is

A `Definition` is a flat list of serialised `Op` messages plus a digest map.
Each `Op` is one of four things, and autopack only needs three:

| Op | Purpose | autopack uses it for |
|---|---|---|
| `SourceOp` | pull an image, read the local context | `Layer::image`, `Layer::local` |
| `ExecOp` | run a command with mounts | every `Command::Exec` |
| `FileOp` | copy, mkdir, mkfile, rm | `Command::Copy`, `Command::File`, layer filters |
| `BuildOp` | nested builds | not needed |

Mounts on `ExecOp` carry the rest: `CacheOpt` for `RUN --mount=type=cache`,
`SecretOpt` for secrets, and plain layer mounts for step inputs.

That is the entire mapping. There is no hidden second protocol.

## Shape of an `autopack-llb` crate

```
crates/autopack-llb/
  build.rs          # prost-build over vendored ops.proto
  proto/ops.proto   # vendored from moby/buildkit solver/pb, pinned by commit
  src/
    op.rs           # thin builders over the generated types
    digest.rs       # content addressing — every op is keyed by its own digest
    marshal.rs      # Definition assembly + topological ordering
    lower.rs        # BuildPlan -> Definition
```

`prost-build` generates the message types; the hand-written part is `lower.rs`,
which is the same shape as the existing `to_dockerfile()`.

## Mapping, concretely

| `BuildPlan` | LLB |
|---|---|
| `Step` | a chain of `ExecOp`s, each taking the previous as input |
| first `Layer` of a step | `SourceOp` (`docker-image://…`) or the referenced step's output |
| other `Layer`s | `FileOp` copy actions with include/exclude — **no `rm -rf` workaround** |
| `Cache` | `ExecOp` mount, `MountType::Cache`, `CacheOpt { id, sharing }` |
| secret | `ExecOp` mount, `MountType::Secret`, `SecretOpt { id }` |
| `Command::File` + asset | `FileOp` `Mkfile` action |
| `Deploy` | the terminal op, plus an image config emitted alongside |

Two things fall out that the Dockerfile backend cannot do:

1. **Independent steps become independent DAG branches.** BuildKit schedules
   them concurrently on its own; the plan already carries the dependency
   information, and the Dockerfile is what flattens it.
2. **Exclusions become real.** `Layer::exclude` currently lowers to
   `RUN rm -rf …` *after* the copy, so the excluded bytes still enter a layer.
   `FileOp` copy takes exclude patterns directly.

## The part that is not free

- **Image config.** A Dockerfile gives you `ENV`/`CMD`/`USER` for free. With
  raw LLB you also have to construct the OCI image config JSON and hand it to
  the exporter. That is a second, separate format to get right — and it is
  where `USER`, `ENTRYPOINT` and the tini wiring live today.
- **Cache import/export.** `--cache-from`/`--cache-to` are exporter options on
  the Solve request, not part of the LLB graph.
- **Transport.** Piping to `buildctl` works today (proven above). Doing it
  in-process means a `tonic` gRPC client speaking BuildKit's `Control.Solve`,
  including the session protocol for local context and secrets — that session
  side is the genuinely fiddly part, not the graph.
- **A BuildKit endpoint becomes mandatory.** The Dockerfile backend runs
  anywhere Docker does. This is the portability cost, and it is why Dockerfile
  should stay the default rather than become the fallback.

## Rough shape of the work

| Piece | Effort | Risk |
|---|---|---|
| Vendor proto + prost codegen | small | low — the proto is stable and versioned |
| `SourceOp`/`ExecOp`/`FileOp` builders + digests | medium | low |
| `BuildPlan` → `Definition` lowering | medium | low — mirrors the existing lowering |
| OCI image config emission | medium | **medium** — a separate spec, easy to get subtly wrong |
| `buildctl` piping backend | small | low — already proven |
| In-process gRPC solve + session | large | **high** — the session protocol is the real work |

A useful first milestone is everything above the gRPC row: generate LLB, pipe
to `buildctl`, keep Dockerfile as default. That is genuinely incremental and
each step is independently testable against the conformance suite — the same
28 examples would have to pass on both backends, which is exactly the kind of
differential test that makes a second backend safe to trust.

## Recommendation

Own the library, not the frontend. Vendor `ops.proto`, generate with `prost`,
and lower the plan we already have. Do not write a gateway frontend — it is the
part that forces Go on railpack, and piping a definition to `buildctl` reaches
the same executor without it.
