<p align="center">
  <img src="assets/logo-wide.svg" alt="upd logo" width="400">
</p>

# upd

[![crates.io](https://img.shields.io/crates/v/upd.svg)](https://crates.io/crates/upd)
[![PyPI](https://img.shields.io/pypi/v/upd.svg)](https://pypi.org/project/upd/)
[![CI](https://github.com/rvben/upd/actions/workflows/ci.yml/badge.svg)](https://github.com/rvben/upd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/rvben/upd/blob/main/LICENSE)

A fast dependency updater for Python, Node.js, Rust, Go, Ruby, .NET, Terraform, GitHub Actions, pre-commit, and Mise projects, written in Rust.

## Quick Start

```bash
# Preview changes without modifying files (default)
uvx upd

# Apply updates
uvx upd --apply

# Or with pipx
pipx run upd --apply
```

## Features

- **Multi-ecosystem**: Python, Node.js, Rust, Go, Ruby, .NET, Terraform, GitHub Actions, pre-commit, Mise/asdf
- **Dry-run by default**: nothing is written without `--apply`
- **Fast**: parallel registry requests, with a 24-hour version cache
- **Constraint-aware**: respects `>=2.0,<3` (Python), `~> 7.1` (Ruby), and `^2.0.0` / `~2.0.0` (npm, Cargo)
- **Format-preserving**: keeps formatting, comments, and structure
- **Update filters**: `--only-bump`, `--max-bump`, `--package`, `--lang`, or approve one by one with `-i`
- **Major warnings**: breaking changes are flagged with `(MAJOR)`
- **Pre-release aware**: updates pre-releases to newer pre-releases
- **Cooldown**: hold back releases younger than N days, against supply-chain attacks
- **Security auditing**: OSV vulnerability scanning with auto-fix and SARIF output
- **Check mode**: exit 1 if updates are available (for CI and pre-commit)
- **Gitignore-aware**: honors `.gitignore` and prunes hidden directories, without missing the dotfiles it updates
- **Private registries**: authentication for PyPI, npm, Cargo, Go, and GitHub
- **Config file**: ignore or pin packages via `.updrc.toml`

## Installation

### From crates.io

```bash
cargo install upd

# or with cargo-binstall (faster, pre-built binary)
cargo binstall upd
```

### From PyPI

```bash
pip install upd
# or with uv
uv pip install upd
```

If you installed an earlier release under the old distribution name, migrate
once with `pip uninstall upd-cli && pip install upd`. The `upd-cli` command
remains available as a compatibility alias.

### From source

```bash
git clone https://github.com/rvben/upd
cd upd
cargo install --path .
```

## Usage

```bash
# Preview changes without modifying files (default when no --apply)
upd

# Apply updates to files
upd --apply

# Limit to specific files or directories
upd --apply requirements.txt pyproject.toml

# Approve updates one by one
upd -i

# Only the packages you name
upd -p requests,flask

# Cap the bump level (allow patch + minor, skip major). Updates above the
# ceiling are reported as held back, never as up to date, and do not
# change the exit code.
upd --max-bump minor

# Restrict to exactly one level (repeatable, comma-separated)
upd --only-bump major

# One ecosystem at a time: python, node, rust, go, ruby, dot-net,
# terraform, actions, pre-commit, mise, annotated
upd --lang python

# Exit 1 if anything is outdated (for CI and pre-commit)
upd --check

# Regenerate lockfiles after writing
upd --apply --lock

# Print the effective configuration and exit
upd --show-config
```

`upd --help` lists every flag; [Stability](https://github.com/rvben/upd/blob/main/docs/stability.md)
documents the ones that are contractual, and `upd schema` emits the whole
interface as JSON.

> **Dry-run by default**: `upd` without `--apply` only previews changes. Pass `--apply` to
> write updates. `--check`, `--dry-run`, and `--interactive` do not require `--apply`.
>
> **VCS-root scoping**: When no path argument is given, `upd` scans from the nearest `.git`
> ancestor directory rather than the current working directory. This prevents accidental
> rewrites when CWD is a subdirectory inside a repository.

### Commands

```bash
upd --version      # Print version
upd self-update    # Check for upd updates
upd clean-cache    # Clear the version cache
upd align          # Align versions across files (--check exits 1 on misalignment)
upd audit          # Scan for known vulnerabilities (exit 6 if found)
upd schema         # Machine-readable interface description
```

## Example Output

```text
.pre-commit-config.yaml:37: Would update pre-commit/pre-commit-hooks v4.6.0 → v6.0.0 (MAJOR)
.github/workflows/ci.yml:16: Would update actions/checkout v4 → v6 (MAJOR)
.github/workflows/ci.yml:18: Would update jdx/mise-action v2 → v4 (MAJOR)
.mise.toml:8: Would update rust 1.91.1 → 1.94.0
Cargo.toml:33: Would update clap 4.5.53 → 4.6.0
Cargo.toml:36: Would update tokio 1.48.0 → 1.50.0

Would update 6 package(s) (2 major, 3 minor, 1 patch) in 4 file(s), 8 up to date
```

Output includes clickable `file:line:` locations (recognized by VS Code, iTerm2, and modern terminals).

## Version Constraints

`upd` respects version constraints in your dependency files:

| Constraint | Behavior |
|------------|----------|
| `>=2.0,<3` | Updates within 2.x range only |
| `^2.0.0` | Updates within 2.x range (npm/Cargo); never crosses the major bound |
| `~2.0.0` | Updates within 2.0.x range (npm); `~2.0.0` (Cargo) stays within 2.0.x |
| `~> 7.1` | Updates within 7.x range (Ruby pessimistic) |
| `>=2.0` | Updates to any version >= 2.0 |
| `==2.0.0` | Updates the exact pin to the latest version (e.g. `==2.0.0` → `==3.1.5`). To freeze a package, use `[pin]` or `ignore` in `.updrc.toml`. |

For npm, comparator ranges such as `">=1.0.0 <2.0.0"` are rewritten with a
**bump strategy**: the lower bound moves to the highest version satisfying the
constraint, preserving the upper bound. Hyphen (`"1 - 2"`) and OR
(`"^1 || ^2"`) ranges are reported as warnings and left untouched rather than
rewritten wrongly.

## Version Precision

By default, `upd` preserves version precision from the original file:

```text
# Original file has 2-component versions
flask>=2.0        →  flask>=3.1        (not 3.1.5)
django>=4         →  django>=6         (not 6.0.0)

# Original file has 3-component versions
requests>=2.0.0   →  requests>=2.32.5

# GitHub Actions major-only tags
actions/checkout@v3  →  actions/checkout@v4  (not @v4.2.0)
```

Use `--full-precision` to always output full semver versions:

```text
upd --full-precision
flask>=2.0        →  flask>=3.1.5
django>=4         →  django>=6.0.0
requests>=2.0.0   →  requests>=2.32.5
```

## Version Alignment

In monorepos or projects with multiple dependency files, the same package might
have different versions:

```text
# requirements.txt
requests==2.28.0

# requirements-dev.txt
requests==2.31.0

# services/api/requirements.txt
requests==2.25.0
```

`upd align` updates every occurrence to the highest version found:

```bash
upd align              # Align all packages to highest version
upd align --dry-run    # Preview changes
upd align --check      # Exit 1 if misalignments (for CI)
upd align --lang python # Align only Python packages
```

It only aligns within one ecosystem, skips packages with upper bound
constraints (e.g. `>=2.0,<3.0`) to avoid breaking them, and ignores
pre-release versions when finding the highest version.

## Pre-commit Integration

Add `upd` to your `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/rvben/upd-pre-commit
    rev: v0.0.24
    hooks:
      - id: upd-check
        # Optional: only check specific ecosystems
        # args: ['--lang', 'python']
```

Available hooks:

| Hook ID | Description |
|---------|-------------|
| `upd-check` | Fail if any dependencies are outdated |
| `upd-check-major` | Fail only on major (breaking) updates |

Both hooks run on `pre-push` by default. Uses `language: python` which installs `upd` from PyPI automatically, so no manual installation is needed.

## Documentation

Everything you look up rather than read lives in
[docs/](https://github.com/rvben/upd/tree/main/docs).

### Releases

Vership workflow, publication guarantees, automated integration pins, and safe
retry procedures.
→ [docs/releases.md](https://github.com/rvben/upd/blob/main/docs/releases.md)

### Supported files

Every file `upd` discovers, per ecosystem, plus annotated version pins in files
it does not otherwise understand.
→ [docs/ecosystems.md](https://github.com/rvben/upd/blob/main/docs/ecosystems.md)

### Security auditing

OSV vulnerability scanning, `--fix-audit`, SARIF output, and CI integration.
→ [docs/audit.md](https://github.com/rvben/upd/blob/main/docs/audit.md)

### Configuration file

`.updrc.toml` discovery order and every key it accepts.
→ [docs/configuration.md](https://github.com/rvben/upd/blob/main/docs/configuration.md)

### Cooldown (minimum release age)

Hold back versions published less than N days ago, per ecosystem.
→ [docs/configuration.md#cooldown-minimum-release-age](https://github.com/rvben/upd/blob/main/docs/configuration.md#cooldown-minimum-release-age)

### Caching

Where the 24-hour version cache lives and how to clear or bypass it.
→ [docs/configuration.md#caching](https://github.com/rvben/upd/blob/main/docs/configuration.md#caching)

### Environment variables

Every variable `upd` reads, in one table.
→ [docs/configuration.md#environment-variables](https://github.com/rvben/upd/blob/main/docs/configuration.md#environment-variables)

### Private repositories

Credential detection for PyPI, npm, Cargo, Go, and GitHub, including private
indexes declared in `pyproject.toml`.
→ [docs/private-registries.md](https://github.com/rvben/upd/blob/main/docs/private-registries.md)

### GitHub pull requests

Run any supported dependency updates as one rolling GitHub PR, with immutable
Action SHA verification, validation, artifact reporting, and opt-in auto-merge.
→ [docs/github-actions.md](https://github.com/rvben/upd/blob/main/docs/github-actions.md)

### GitLab merge requests

Run scheduled dependency updates as one rolling GitLab MR, with validation,
lease-protected branch updates, and explicitly opt-in GitLab-native auto-merge.
→ [docs/gitlab.md](https://github.com/rvben/upd/blob/main/docs/gitlab.md)

### Stability

The stable CLI surface, exit codes, `--lock` commands, and output guarantees.
→ [docs/stability.md](https://github.com/rvben/upd/blob/main/docs/stability.md)

## Development

```bash
# Build
make build

# Run tests
make test

# Lint
make lint

# Format
make fmt

# All checks
make check
```

## License

MIT
