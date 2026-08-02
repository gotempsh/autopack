# Inherit temps workflows, hooks, and cargo commands

**Date:** 2026-08-02  
**Status:** Approved for planning  
**Source:** `/home/bruny/open-source/temps`  
**Approach:** Adapt-in-place (portable tooling + shared GitHub meta; no product CI)

## Goal

Give autopack the same local quality gates and shared GitHub meta as temps — pre-commit/prek hooks, local CI mirror, conventional commits, changelog preview, cargo-audit, Dependabot, issue/PR templates, and contributor docs — without importing temps product pipelines (e2e, release/nightly, sandbox, skill-security, network-kernel, musl binary builds, web/bun audit, container scan).

## Non-goals

- Copying or adapting temps e2e, release, nightly-release, sandbox-images, skill-security, or network-kernel workflows
- Bun/web dependency audit or Trivy container scanning
- Dual-license or Code of Conduct files temps references but does not ship
- Generating a full historical `CHANGELOG.md` in this change (changelog is produced from commits at release time; PR workflow validates commits and posts a preview)
- Shared submodule / external reusable workflow package

## Architecture

```text
autopack/
├── .pre-commit-config.yaml
├── _typos.toml
├── cliff.toml
├── CONTRIBUTING.md
├── README.md                          # Development / Contributing pointer
├── .github/
│   ├── dependabot.yml
│   ├── PULL_REQUEST_TEMPLATE.md
│   ├── ISSUE_TEMPLATE/
│   │   ├── bug_report.yml
│   │   ├── feature_request.yml
│   │   └── config.yml
│   └── workflows/
│       ├── ci.yml                     # EDIT: align cargo commands
│       ├── starters.yml               # KEEP
│       ├── changelog.yml              # NEW
│       └── dependency-scan.yml        # NEW (cargo-audit only)
└── scripts/
    ├── setup-hooks.sh
    ├── ci-local.sh
    └── hooks/validate-changelog.py
```

Source of truth for behaviour remains the temps files of the same names, adapted for a Rust-only library/CLI workspace (`gotempsh/autopack`).

## Components

### 1. Git hooks

**`.pre-commit-config.yaml`** — same hook set as temps:

- `pre-commit-hooks`: `check-yaml`, `trailing-whitespace`, `end-of-file-fixer`
- `typos` (crate-ci/typos)
- `conventional-pre-commit` on `commit-msg` stage
- Local `changelog-format` → `python3 scripts/hooks/validate-changelog.py` when `CHANGELOG.md` changes
- Local `cargo fmt` (all Rust; `pass_filenames: false`)
- Local `cargo clippy --all-targets --all-features -- -D warnings`

**`scripts/setup-hooks.sh`** — prefer `prek`, else `pre-commit`; install both `pre-commit` and `commit-msg` hook types; optional first `run --all-files`.

**`_typos.toml`** — start minimal (e.g. exclude translated READMEs if any). Do not copy temps-only identifiers (`vertexes`, AIMD, trace IDs, etc.) unless they appear in autopack.

**`scripts/hooks/validate-changelog.py`** — copy from temps; guards generated `CHANGELOG.md` shape if present/edited. Does not require hand-maintaining the file in PRs.

### 2. Cargo commands / local CI

**`scripts/ci-local.sh`** — mirror the GitHub check job:

| Target | Command |
|--------|---------|
| `fmt` | `cargo fmt --all -- --check` (or apply with `--fix`) |
| `check` | `cargo check --workspace --all-targets` |
| `clippy` | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |

Include temps’ clippy fingerprint bust so local runs do not silently reuse stale lint cache. Usage: `scripts/ci-local.sh`, optional `fmt|check|clippy`, optional `--fix`.

**`.github/workflows/ci.yml` (edit)** — keep concurrency, conformance matrix, and starters untouched in spirit. Update the format/lint/test job to:

1. `cargo fmt --all -- --check`
2. `cargo check --workspace --all-targets`
3. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
4. `cargo test --workspace`

Bump checkout / action pins toward temps’ current major versions where low-risk and consistent with the new workflows; do not redesign the conformance job.

### 3. Changelog workflow

**`cliff.toml`** — temps config with issue links rewritten to `https://github.com/gotempsh/autopack/issues/…`.

**`.github/workflows/changelog.yml`** — on PRs to `main`:

1. Determine `merge-base..HEAD`
2. Lint Conventional Commits (same type set as temps; merge commits exempt)
3. Generate preview with `orhun/git-cliff-action` + `cliff.toml`
4. Post/update PR comment with marker `<!-- changelog-preview -->` when the head repo is not a fork

`CHANGELOG.md` is generated at release time from commits; contributors must not hand-edit it in PRs. Creating an initial empty/generated changelog file is optional and can wait for a release process.

### 4. Dependency scan

**`.github/workflows/dependency-scan.yml`** — Rust only:

- Triggers: PRs touching `**/Cargo.toml`, `**/Cargo.lock`, or the workflow file; weekly Monday cron; `workflow_dispatch`
- Job: `cargo-audit` via `rustsec/audit-check` (with Rust toolchain + rust-cache)
- No `web-audit`, no Trivy/`container-scan`
- No temps RUSTSEC ignore list initially; add ignores only with documented rationale if audit fails on transitive/unfixable advisories

### 5. Dependabot

**`.github/dependabot.yml`**:

- Ecosystem: `cargo`, directory `/`
- Schedule: weekly, Monday
- `open-pull-requests-limit: 10`
- Labels: `dependencies`, `rust`
- No temps-specific ignore rules (sqlx, sea-orm, aws-smithy, etc.)

### 6. Issue / PR templates

- **`PULL_REQUEST_TEMPLATE.md`** — Description, type of change, checklist (`cargo test`, `cargo check`, Conventional Commits, docs), related issues. Autopack wording; no temps dual-license notes.
- **`ISSUE_TEMPLATE/bug_report.yml`** / **`feature_request.yml`** — same fields as temps, product name Autopack.
- **`ISSUE_TEMPLATE/config.yml`** — `blank_issues_enabled: false`; contact links to Autopack GitHub Discussions (and docs only if a stable public docs URL exists; otherwise omit docs link).

### 7. Contributor documentation

**`CONTRIBUTING.md` (new)** — same section spine as temps, Autopack content:

1. Ways to Contribute  
2. Development Setup (Rust ≥ workspace `rust-version` 1.82, rustfmt/clippy; Docker only for conformance/examples — no Postgres/Bun/wasm)  
3. Clone and Build (`gotempsh/autopack`, `cargo build --release -p autopack-cli`)  
4. Pre-commit Hooks (`./scripts/setup-hooks.sh`)  
5. Local CI (`./scripts/ci-local.sh`, `--fix`)  
6. Architecture Overview (analyze → providers → `BuildPlan` → dockerfile backend; crates: `autopack-core`, `autopack-providers`, `autopack-dockerfile`, `autopack-cli`)  
7. Coding Standards (Conventional Commits; generated changelog; Rust check/clippy/fmt/tests; conformance via `scripts/conformance.sh`)  
8. Testing  
9. Pull Request Process + checklist  
10. Good First Issues (Autopack label URL)  
11. License (MIT only)

Omit temps-only: frontend, TimescaleDB, wasm-pack, Amazon Linux matrices, Sea-ORM three-layer handler rules, dual-license, missing CoC file. Omit `scripts/changelog.sh` for now; CONTRIBUTING documents that changelog preview comes from the Changelog CI job (and git-cliff locally if installed).

**`README.md` (edit)** — before Licence, replace Development with:

1. A short **Contributing** section: welcome + link to `CONTRIBUTING.md` (temps README pattern).  
2. A **Development** section with `setup-hooks.sh`, `ci-local.sh`, and `cargo test --workspace`.

Keep the existing Licence section unchanged.

## Data flow

```text
Developer commit
  → pre-commit: yaml/whitespace/typos/fmt/clippy
  → commit-msg: conventional commits
  → (if CHANGELOG.md staged) validate-changelog.py

Developer pre-push (optional local)
  → scripts/ci-local.sh  ≈  ci.yml check job

Pull request
  → ci.yml (fmt/check/clippy/test + conformance)
  → changelog.yml (lint commits + cliff preview comment)
  → dependency-scan.yml (if Cargo files changed → cargo-audit)
  → Dependabot: weekly cargo PRs independently
```

## Error handling

- Hook / `ci-local` failures: fix and retry; no `--no-verify` unless the contributor explicitly chooses to skip (discouraged in CONTRIBUTING).
- Changelog CI: fail the job on non-conventional commits; skip preview comment on forks (read-only token).
- cargo-audit: fail on matching advisories; introduce ignores only with comments explaining why.
- First `prek`/`pre-commit` run may rewrite formatting or flag typos across the tree — fix in the same implementation PR or a follow-up commit on the branch.

## Testing / verification

Success criteria:

1. `./scripts/setup-hooks.sh` installs hooks; bad commit message or unformatted Rust fails locally.  
2. `./scripts/ci-local.sh` matches the `ci.yml` check job commands.  
3. PRs trigger changelog validation/preview; Cargo file changes trigger cargo-audit.  
4. Dependabot weekly cargo config is present.  
5. Issue/PR templates show Autopack wording.  
6. `CONTRIBUTING.md` and README Development document hooks, `ci-local`, commits, and changelog rules.  
7. No temps product workflows are imported.

## Implementation notes

- Prefer copying temps file contents and adapting names/URLs/ignores over inventing parallel behaviour.  
- Pin GitHub Actions SHAs/versions consistently with temps where practical.  
- Autopack remote may differ (`brunyeestudio` fork vs `gotempsh/autopack`); docs and cliff links use canonical `gotempsh/autopack` per `Cargo.toml` `repository`.  
- Do not remove or shrink the existing conformance matrix as part of this work.
