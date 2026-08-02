# Inherit temps Tooling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port temps’ portable git hooks, cargo local/CI commands, changelog + dependency-scan workflows, Dependabot, issue/PR templates, and contributor docs into autopack without importing product-specific pipelines.

**Architecture:** Adapt-in-place copy from `/home/bruny/open-source/temps`: same hook/CI command surface, Autopack-only dependency-scan (cargo-audit), Autopack wording in templates/docs, existing conformance/starters workflows left intact aside from cargo-command alignment in `ci.yml`.

**Tech Stack:** Rust workspace (rust-version 1.82), pre-commit/prek, git-cliff, GitHub Actions, cargo-audit (rustsec/audit-check), Dependabot.

**Spec:** `docs/superpowers/specs/2026-08-02-inherit-temps-tooling-design.md`

## Global Constraints

- Canonical repo URLs and cliff issue links use `https://github.com/gotempsh/autopack` (from `Cargo.toml` `repository`).
- Do not import temps e2e, release/nightly, sandbox, skill-security, network-kernel, musl binary, web/bun audit, or Trivy workflows.
- Do not shrink or redesign the existing conformance matrix in `ci.yml` / `starters.yml`.
- Clippy bar: `--workspace --all-targets --all-features -- -D warnings`.
- No temps RUSTSEC ignore list initially in dependency-scan.
- Omit `scripts/changelog.sh`; omit CODE_OF_CONDUCT / dual-license.
- License in docs: MIT only.
- Source files to adapt live at `/home/bruny/open-source/temps/…`.

---

## File Structure

| Path | Responsibility |
|------|----------------|
| `.pre-commit-config.yaml` | Hook definitions (fmt, clippy, typos, conventional commits, changelog) |
| `_typos.toml` | Minimal typos config |
| `scripts/setup-hooks.sh` | Install prek/pre-commit + commit-msg hooks |
| `scripts/hooks/validate-changelog.py` | Guard generated CHANGELOG.md shape |
| `scripts/ci-local.sh` | Local mirror of CI fmt/check/clippy |
| `.github/workflows/ci.yml` | Align check job cargo commands |
| `cliff.toml` | git-cliff config (autopack issue links) |
| `.github/workflows/changelog.yml` | Conventional commit lint + PR preview |
| `.github/workflows/dependency-scan.yml` | cargo-audit only |
| `.github/dependabot.yml` | Weekly cargo updates |
| `.github/PULL_REQUEST_TEMPLATE.md` | PR checklist |
| `.github/ISSUE_TEMPLATE/*` | Bug/feature/config templates |
| `CONTRIBUTING.md` | Contributor guide |
| `README.md` | Contributing + Development pointers |

---

### Task 1: Git hooks scaffolding

**Files:**
- Create: `.pre-commit-config.yaml`
- Create: `_typos.toml`
- Create: `scripts/hooks/validate-changelog.py`
- Create: `scripts/setup-hooks.sh`

**Interfaces:**
- Consumes: temps `.pre-commit-config.yaml`, `_typos.toml` pattern, `scripts/hooks/validate-changelog.py`, `scripts/setup-hooks.sh`
- Produces: hooks runnable via `prek`/`pre-commit`; `setup-hooks.sh` executable

- [ ] **Step 1: Create `.pre-commit-config.yaml`**

```yaml
repos:
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v6.0.0
    hooks:
      - id: check-yaml
      - id: trailing-whitespace
      - id: end-of-file-fixer
  - repo: https://github.com/crate-ci/typos
    rev: v1.38.1
    hooks:
      - id: typos

  # Conventional Commits linter
  - repo: https://github.com/compilerla/conventional-pre-commit
    rev: v3.6.0
    hooks:
      - id: conventional-pre-commit
        stages: [commit-msg]

  - repo: local
    hooks:
      - id: changelog-format
        name: validate changelog format
        entry: python3 scripts/hooks/validate-changelog.py
        language: system
        files: ^CHANGELOG\.md$
        pass_filenames: false

      - id: cargo-fmt
        name: cargo fmt
        entry: cargo fmt --
        language: system
        types: [rust]
        pass_filenames: false

      - id: cargo-clippy
        name: cargo clippy
        language: system
        types: [rust]
        pass_filenames: false
        entry: cargo clippy --all-targets --all-features -- -D warnings
```

- [ ] **Step 2: Create minimal `_typos.toml`**

```toml
[files]
# Reserved for future exclude patterns (e.g. non-English READMEs).

[default.extend-words]
# Add project-specific false positives here as typos reports them.
```

- [ ] **Step 3: Create `scripts/hooks/validate-changelog.py`**

Copy `/home/bruny/open-source/temps/scripts/hooks/validate-changelog.py` verbatim, then change only the failure hint at the bottom from:

```python
        print("Note: CHANGELOG.md is generated — regenerate it with "
              "`scripts/changelog.sh` rather than editing by hand.")
```

to:

```python
        print("Note: CHANGELOG.md is generated — regenerate it with "
              "`git cliff` (see cliff.toml) rather than editing by hand.")
```

Make the file executable: `chmod +x scripts/hooks/validate-changelog.py`

- [ ] **Step 4: Create `scripts/setup-hooks.sh`**

Copy `/home/bruny/open-source/temps/scripts/setup-hooks.sh` verbatim to `scripts/setup-hooks.sh`. No logic changes required (tooling is project-agnostic). Make executable: `chmod +x scripts/setup-hooks.sh`

- [ ] **Step 5: Verify hook config parses**

Run: `python3 -c "import yaml; yaml.safe_load(open('.pre-commit-config.yaml'))"`  
(or `prek validate-config` / `pre-commit validate-config` if installed)

Expected: no exception / exit 0

- [ ] **Step 6: Commit**

```bash
git add .pre-commit-config.yaml _typos.toml scripts/hooks/validate-changelog.py scripts/setup-hooks.sh
git commit -m "$(cat <<'EOF'
ci: add pre-commit hooks and setup-hooks script from temps

EOF
)"
```

---

### Task 2: Local CI script + align `ci.yml` cargo commands

**Files:**
- Create: `scripts/ci-local.sh`
- Modify: `.github/workflows/ci.yml` (check job only)

**Interfaces:**
- Consumes: temps `scripts/ci-local.sh`; existing `ci.yml` conformance job untouched
- Produces: `ci-local.sh` commands identical to check job steps

- [ ] **Step 1: Create `scripts/ci-local.sh`**

Copy `/home/bruny/open-source/temps/scripts/ci-local.sh` to `scripts/ci-local.sh`, then change the header comment block to:

```bash
#!/usr/bin/env bash
# ci-local.sh — Run the same checks as the GitHub Actions PR check job.
#
# Mirrors .github/workflows/ci.yml job `check`: fmt, check, clippy.
# Run this BEFORE pushing to catch failures locally with identical commands.
#
# Usage:
#   scripts/ci-local.sh              # Run all checks (fmt, check, clippy)
#   scripts/ci-local.sh fmt          # Only formatting check
#   scripts/ci-local.sh check        # Only cargo check
#   scripts/ci-local.sh clippy       # Only clippy
#   scripts/ci-local.sh --fix        # Auto-fix fmt + clippy where possible
```

Also change the cwd comment from `temps/` to `autopack/`:

```bash
# Always run from the autopack crate root regardless of cwd.
```

Leave the fmt/check/clippy command bodies identical to temps (including clippy fingerprint bust and `--all-features`). Make executable: `chmod +x scripts/ci-local.sh`

- [ ] **Step 2: Update the `check` job in `.github/workflows/ci.yml`**

Replace the check job steps so they are:

```yaml
  check:
    name: Format, lint, test
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - uses: Swatinem/rust-cache@v2

      - name: Format
        run: cargo fmt --all -- --check

      - name: Check
        run: cargo check --workspace --all-targets

      - name: Clippy
        run: cargo clippy --workspace --all-targets --all-features -- -D warnings

      - name: Test
        run: cargo test --workspace
```

Do not modify the `conformance` job matrix or steps in this task.

- [ ] **Step 3: Run local CI script (fmt + check at minimum)**

Run: `./scripts/ci-local.sh fmt check`  
Expected: both steps pass (or fmt fails with actionable diffs — if so, run `./scripts/ci-local.sh fmt --fix` is wrong; use `./scripts/ci-local.sh --fix fmt` then re-run check).

If clippy is slow, still run once before commit: `./scripts/ci-local.sh clippy`  
Expected: pass, or fix warnings introduced by `--all-features` before committing.

- [ ] **Step 4: Commit**

```bash
git add scripts/ci-local.sh .github/workflows/ci.yml
git commit -m "$(cat <<'EOF'
ci: add ci-local.sh and align check job with temps cargo commands

EOF
)"
```

---

### Task 3: Changelog workflow + cliff.toml

**Files:**
- Create: `cliff.toml`
- Create: `.github/workflows/changelog.yml`

**Interfaces:**
- Consumes: temps `cliff.toml`, `.github/workflows/changelog.yml`
- Produces: PR workflow that lints conventional commits and posts cliff preview

- [ ] **Step 1: Create `cliff.toml`**

Copy `/home/bruny/open-source/temps/cliff.toml` to `cliff.toml`, changing only the issue-link preprocessor replace URL:

```toml
commit_preprocessors = [
  # Replace issue numbers
  { pattern = '\((\w+\s)?#([0-9]+)\)', replace = "([#${2}](https://github.com/gotempsh/autopack/issues/${2}))" },
]
```

Leave all other parsers/header/body identical to temps.

- [ ] **Step 2: Create `.github/workflows/changelog.yml`**

Copy `/home/bruny/open-source/temps/.github/workflows/changelog.yml` verbatim to `.github/workflows/changelog.yml`. No Autopack-specific logic changes (workflow is already generic aside from relying on root `cliff.toml`).

- [ ] **Step 3: Sanity-check cliff config**

If `git-cliff` is installed: `git cliff --config cliff.toml --unreleased --strip all | head`  
Expected: markdown preview or empty unreleased section without config parse errors.

If not installed: `grep -n 'gotempsh/autopack' cliff.toml`  
Expected: one match on the issues URL line; `grep temps/issues cliff.toml` must print nothing.

- [ ] **Step 4: Commit**

```bash
git add cliff.toml .github/workflows/changelog.yml
git commit -m "$(cat <<'EOF'
ci: add changelog workflow and git-cliff config

EOF
)"
```

---

### Task 4: Dependency scan + Dependabot

**Files:**
- Create: `.github/workflows/dependency-scan.yml`
- Create: `.github/dependabot.yml`

**Interfaces:**
- Consumes: temps cargo-audit job pins; temps dependabot schedule shape
- Produces: cargo-audit-only workflow; weekly cargo Dependabot

- [ ] **Step 1: Create `.github/workflows/dependency-scan.yml`**

```yaml
name: Dependency Scan

# Supply-chain scanning for autopack's Rust workspace.
# cargo-audit checks Cargo.lock against the RustSec Advisory Database.
# No web/bun or container image jobs — this repo has neither a web/ tree
# nor a published release image workflow yet.
on:
  pull_request:
    branches: [main]
    paths:
      - "**/Cargo.toml"
      - "**/Cargo.lock"
      - ".github/workflows/dependency-scan.yml"
  schedule:
    # Every Monday at 06:00 UTC.
    - cron: "0 6 * * 1"
  workflow_dispatch:

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: ${{ github.event_name == 'pull_request' }}

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1

jobs:
  cargo-audit:
    name: Rust Dependency Audit (cargo-audit)
    runs-on: ubuntu-latest
    permissions:
      contents: read
      checks: write
    steps:
      - name: Checkout code
        uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4 # stable
        with:
          toolchain: stable

      - name: Cache dependencies
        uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32 # v2
        with:
          key: dependency-scan-cargo-audit

      # Fails if any RUSTSEC advisory matches a resolved dependency.
      # Add `ignore:` only with a documented rationale (reachability /
      # upstream blocker), matching temps practice — none yet for autopack.
      - name: Run cargo-audit
        uses: rustsec/audit-check@69366f33c96575abad1ee0dba8212993eecbe998 # v2.0.0
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
```

- [ ] **Step 2: Create `.github/dependabot.yml`**

```yaml
version: 2

updates:
  - package-ecosystem: "cargo"
    directory: "/"
    schedule:
      interval: "weekly"
      day: "monday"
      time: "06:00"
      timezone: "America/New_York"
    open-pull-requests-limit: 10
    labels:
      - "dependencies"
      - "rust"
```

- [ ] **Step 3: Optional local audit smoke test**

If `cargo audit` is installed: `cargo audit`  
Expected: exit 0, or a list of advisories. Do **not** add ignores in this task unless an advisory blocks CI and you document the ignore in the workflow comment. Prefer fixing/upgrading deps when feasible.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/dependency-scan.yml .github/dependabot.yml
git commit -m "$(cat <<'EOF'
ci: add cargo-audit dependency scan and weekly Dependabot

EOF
)"
```

---

### Task 5: Issue and PR templates

**Files:**
- Create: `.github/PULL_REQUEST_TEMPLATE.md`
- Create: `.github/ISSUE_TEMPLATE/bug_report.yml`
- Create: `.github/ISSUE_TEMPLATE/feature_request.yml`
- Create: `.github/ISSUE_TEMPLATE/config.yml`

**Interfaces:**
- Consumes: temps templates structure
- Produces: Autopack-branded GitHub forms

- [ ] **Step 1: Create `.github/PULL_REQUEST_TEMPLATE.md`**

```markdown
## Description

<!-- A clear description of what this PR does and why. -->

## Type of change

- [ ] Bug fix (non-breaking change that fixes an issue)
- [ ] New feature (non-breaking change that adds functionality)
- [ ] Breaking change (fix or feature that would cause existing functionality to change)
- [ ] Documentation update

## Checklist

- [ ] I have written tests that cover the changes
- [ ] All new and existing tests pass (`cargo test --workspace`)
- [ ] `cargo check --workspace --all-targets` passes
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` is clean
- [ ] My commits follow the [Conventional Commits](https://www.conventionalcommits.org/) format
- [ ] I have **not** hand-edited `CHANGELOG.md` (it is generated from commit messages)
- [ ] I have updated documentation where necessary

## Related issues

<!-- Link related issues below. Use "Closes #123" to auto-close an issue when this PR is merged. -->
```

- [ ] **Step 2: Create `.github/ISSUE_TEMPLATE/bug_report.yml`**

```yaml
name: Bug Report
description: Report a bug in Autopack
labels: ["bug"]
body:
  - type: markdown
    attributes:
      value: |
        Thank you for taking the time to report a bug. Please fill out the information below to help us investigate.

  - type: textarea
    id: description
    attributes:
      label: Description
      description: A clear and concise description of the bug.
      placeholder: Describe the bug...
    validations:
      required: true

  - type: textarea
    id: steps-to-reproduce
    attributes:
      label: Steps to reproduce
      description: Detailed steps to reproduce the behavior.
      placeholder: |
        1. Run '...'
        2. Navigate to '...'
        3. See error
    validations:
      required: true

  - type: textarea
    id: expected-behavior
    attributes:
      label: Expected behavior
      description: What you expected to happen.
    validations:
      required: true

  - type: textarea
    id: actual-behavior
    attributes:
      label: Actual behavior
      description: What actually happened.
    validations:
      required: true

  - type: dropdown
    id: os
    attributes:
      label: Operating System
      options:
        - Linux
        - macOS
        - Windows
        - Other
    validations:
      required: true

  - type: input
    id: autopack-version
    attributes:
      label: Autopack version
      description: Output of `autopack --version`, or the git tag/commit you built from.
      placeholder: e.g. 0.1.0
    validations:
      required: true

  - type: input
    id: docker-version
    attributes:
      label: Docker / BuildKit version
      description: Output of `docker version` or `buildctl` if relevant to the bug.
      placeholder: e.g. Docker 27.0.0
    validations:
      required: false

  - type: textarea
    id: logs
    attributes:
      label: Relevant logs
      description: Paste any relevant log output here.
      render: shell
    validations:
      required: false

  - type: textarea
    id: additional-context
    attributes:
      label: Additional context
      description: Any other context about the problem (provider, example app, Dockerfile snippet).
    validations:
      required: false
```

- [ ] **Step 3: Create `.github/ISSUE_TEMPLATE/feature_request.yml`**

```yaml
name: Feature Request
description: Suggest a new feature or improvement for Autopack
labels: ["enhancement"]
body:
  - type: markdown
    attributes:
      value: |
        Thank you for suggesting a feature. Please describe the problem and your proposed solution.

  - type: textarea
    id: problem
    attributes:
      label: Problem description
      description: A clear description of the problem this feature would solve.
      placeholder: I find it difficult to...
    validations:
      required: true

  - type: textarea
    id: solution
    attributes:
      label: Proposed solution
      description: A clear description of what you would like to happen.
    validations:
      required: true

  - type: textarea
    id: alternatives
    attributes:
      label: Alternatives considered
      description: Any alternative solutions or features you have considered.
    validations:
      required: false

  - type: textarea
    id: additional-context
    attributes:
      label: Additional context
      description: Any other context, screenshots, or references related to this request.
    validations:
      required: false
```

- [ ] **Step 4: Create `.github/ISSUE_TEMPLATE/config.yml`**

```yaml
blank_issues_enabled: false
contact_links:
  - name: Discussions
    url: https://github.com/gotempsh/autopack/discussions
    about: Ask questions and discuss ideas with the community.
```

(No public docs URL yet — omit a Documentation contact link.)

- [ ] **Step 5: Commit**

```bash
git add .github/PULL_REQUEST_TEMPLATE.md .github/ISSUE_TEMPLATE/
git commit -m "$(cat <<'EOF'
docs: add Autopack issue and pull request templates

EOF
)"
```

---

### Task 6: CONTRIBUTING.md + README Development section

**Files:**
- Create: `CONTRIBUTING.md`
- Modify: `README.md` (replace `## Development` through end of that section; add `## Contributing` before it; keep `## Licence`)

**Interfaces:**
- Consumes: temps `CONTRIBUTING.md` section spine; autopack crate/layout facts from README
- Produces: contributor-facing docs matching the new hooks/CI

- [ ] **Step 1: Create `CONTRIBUTING.md`**

Write the full file:

```markdown
# Contributing to Autopack

Thank you for your interest in contributing to Autopack. Whether you are reporting a bug, suggesting a feature, improving documentation, or writing code, your contributions are welcome and appreciated.

## Ways to Contribute

- **Bug Reports**: Open an issue with a clear description, steps to reproduce, and expected vs. actual behavior.
- **Feature Requests**: Open an issue describing the use case and proposed solution.
- **Pull Requests**: Fix bugs, implement features, or improve documentation.
- **Discussions**: Join conversations in GitHub Issues and Discussions to help shape the project.

## Development Setup

### Prerequisites

- **Rust** 1.82 or later (`rustup` recommended; see `rust-version` in the workspace `Cargo.toml`)
- **rustfmt** and **clippy** (`rustup component add rustfmt clippy`)
- **Docker** / BuildKit — only required to run conformance builds against example apps
- **pre-commit** or **prek** — installed by `./scripts/setup-hooks.sh` if missing

### Clone and Build

```bash
git clone https://github.com/gotempsh/autopack.git
cd autopack

cargo build --release -p autopack-cli
```

### Pre-commit Hooks

Set up git hooks to enforce formatting, linting, and commit message conventions:

```bash
./scripts/setup-hooks.sh
```

Hooks run `cargo fmt`, `cargo clippy`, typos, and Conventional Commits checks on each commit.

### Local CI

Run the same fmt / check / clippy commands as the GitHub Actions `check` job:

```bash
./scripts/ci-local.sh           # fmt + check + clippy
./scripts/ci-local.sh --fix     # auto-fix fmt/clippy where possible
./scripts/ci-local.sh clippy    # clippy only
```

## Architecture Overview

Autopack is a Cargo workspace that analyses an application directory, builds a serialisable `BuildPlan`, and lowers that plan to a Dockerfile (or another backend later).

```
source dir → App → Provider → BuildContext → BuildPlan → backend → image
```

### Crates

- `autopack-core` — plan schema, source analysis, provider trait, config merge, lock
- `autopack-providers` — built-in language providers
- `autopack-dockerfile` — plan → BuildKit Dockerfile
- `autopack-cli` — the `autopack` binary

## Coding Standards

### Conventional Commits

All commit messages must follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>[optional scope]: <description>
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`

Examples:

```
feat(node): improve Next.js standalone pruning
fix(providers): handle missing start command for PHP
docs: update embedding guide
```

Use `!` after the type/scope for breaking changes (`feat(cli)!: …`). Choose a meaningful **scope** — it becomes the bold prefix in the changelog.

### Changelog (generated — do not edit)

`CHANGELOG.md` is a **generated artifact**, produced from Conventional Commits by [git-cliff](https://git-cliff.org) (config: [`cliff.toml`](./cliff.toml)). **Do not hand-edit it in a PR** — your commit messages *are* the changelog entries.

- A non-conventional commit is **dropped** from the changelog — another reason the commit format is enforced.
- The **Changelog** CI job posts a preview comment showing what your PR will add.
- Preview locally with `git cliff --unreleased --strip all` if you have git-cliff installed.
- Merge commits, release version bumps, and `chore(deps|pr|pull)` are excluded by design (see `commit_parsers` in `cliff.toml`).

### Rust

- Run `cargo check --workspace --all-targets` after every change.
- All new code should include tests. Tests must pass before submitting a PR.
- No clippy warnings on the workspace (`-D warnings`, including `--all-features`).
- Prefer clear error types and messages a host/UI can surface.

### Testing

```bash
# Unit + workspace tests
cargo test --workspace

# Run tests for a specific crate
cargo test -p autopack-core

# Conformance for one example (requires Docker / BuildKit)
AUTOPACK=./target/release/autopack ./scripts/conformance.sh node-express
```

## Pull Request Process

1. **Fork** the repository and create a branch from `main`.
2. **Name your branch** descriptively: `feat/add-zig-provider`, `fix/dockerfile-cache-mount`.
3. **Write your code** following the coding standards above.
4. **Add tests** for any new functionality.
5. **Commit** using Conventional Commits format.
6. **Push** your branch and open a Pull Request targeting `main`.
7. **Describe your changes** in the PR body: what changed, why, and how to test it.

Pre-commit hooks run automatically on each commit to check formatting (`cargo fmt`), linting (`cargo clippy`), and commit message format. If a hook fails, fix the issue and commit again.

### PR Checklist

- [ ] Code compiles (`cargo check --workspace --all-targets`)
- [ ] Clippy is clean (`cargo clippy --workspace --all-targets --all-features -- -D warnings`)
- [ ] Tests pass (`cargo test --workspace`)
- [ ] New functionality includes tests
- [ ] Commit messages follow Conventional Commits (they generate the changelog)
- [ ] PR description explains the change
- [ ] Do **not** edit `CHANGELOG.md` — it is generated from your commit messages

## Good First Issues

If you are new to the project, look for issues labeled [`good first issue`](https://github.com/gotempsh/autopack/labels/good%20first%20issue). These are scoped tasks that provide a good introduction to the codebase.

## License

Autopack is licensed under the [MIT License](LICENSE). By contributing, you agree that your contributions will be licensed under the same terms.
```

- [ ] **Step 2: Update `README.md` Development / Contributing**

Find the existing block:

```markdown
## Development

```bash
cargo test                                                  # unit + example tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Licence
```

Replace it with:

```markdown
## Contributing

We welcome contributions. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, architecture notes, commit format, and the PR process.

## Development

```bash
./scripts/setup-hooks.sh          # fmt, clippy, conventional commits
./scripts/ci-local.sh             # same checks as GitHub CI (fmt/check/clippy)
cargo test --workspace
```

## Licence
```

- [ ] **Step 3: Spot-check docs**

Run: `grep -n 'setup-hooks\|ci-local\|Conventional\|CHANGELOG' CONTRIBUTING.md README.md | head -40`  
Expected: both files mention hooks and ci-local; CONTRIBUTING mentions generated changelog; neither mentions TimescaleDB, Bun, wasm-pack, or `scripts/changelog.sh`.

- [ ] **Step 4: Commit**

```bash
git add CONTRIBUTING.md README.md
git commit -m "$(cat <<'EOF'
docs: add CONTRIBUTING.md and point README at hooks/ci-local

EOF
)"
```

---

### Task 7: End-to-end verification

**Files:**
- None new (verify only)

**Interfaces:**
- Consumes: all artifacts from Tasks 1–6
- Produces: confirmation against spec success criteria

- [ ] **Step 1: Confirm no product workflows were imported**

Run:

```bash
ls .github/workflows/
```

Expected files only among: `ci.yml`, `starters.yml`, `changelog.yml`, `dependency-scan.yml`  
Must **not** exist: `e2e-tests.yml`, `release.yml`, `nightly-release.yml`, `sandbox-images-beta.yml`, `skill-security.yml`, `network-kernel-tests.yml`

- [ ] **Step 2: Confirm ci-local matches ci.yml check commands**

Run:

```bash
grep -E 'cargo (fmt|check|clippy)' scripts/ci-local.sh .github/workflows/ci.yml
```

Expected: both use `fmt --all`, `check --workspace --all-targets`, `clippy --workspace --all-targets --all-features -- -D warnings`

- [ ] **Step 3: Run full local CI**

Run: `./scripts/ci-local.sh`  
Expected: exit 0 (fix any remaining fmt/clippy issues first)

- [ ] **Step 4: Final commit only if Step 3 required fixes**

If fixes were needed:

```bash
git add -u
git commit -m "$(cat <<'EOF'
chore: satisfy fmt/clippy after temps tooling inheritance

EOF
)"
```

---

## Spec coverage checklist (self-review)

| Spec requirement | Task |
|------------------|------|
| `.pre-commit-config.yaml` + setup-hooks + validate-changelog + `_typos.toml` | Task 1 |
| `ci-local.sh` + ci.yml cargo alignment | Task 2 |
| `cliff.toml` + `changelog.yml` | Task 3 |
| `dependency-scan.yml` cargo-audit only | Task 4 |
| Weekly Dependabot | Task 4 |
| Issue/PR templates | Task 5 |
| `CONTRIBUTING.md` + README Contributing/Development | Task 6 |
| No product pipelines | Task 7 + Global Constraints |
| Success criteria verification | Task 7 |

## Placeholder / consistency notes

- Action SHA pins match temps for new workflows; existing `ci.yml` keeps `@v4` / `@stable` / `@v2` tags unless a later task intentionally unifies pins.
- Autopack checklist uses `--workspace` (not temps’ `--lib`) to match this repo’s test surface.
- `validate-changelog.py` mentions `git cliff`, not `scripts/changelog.sh`.
