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
