<p align="center">
  <img src="assets/logo-wide.svg" alt="upd logo" width="400">
</p>

# upd

A fast dependency updater for Python, Node.js, Rust, and Go projects,
written in Rust.

## Quick Start

```bash
# Run without installing (using uv)
uvx --from upd-cli upd

# Or with pipx
pipx run --spec upd-cli upd

# Preview changes without modifying files
uvx --from upd-cli upd -n
```

## Features

- **Multi-ecosystem**: Python, Node.js, Rust, and Go dependencies
- **Fast**: Parallel registry requests for all dependencies
- **Constraint-aware**: Respects version constraints like `>=2.0,<3`
- **Smart caching**: 24-hour version cache for faster subsequent runs
- **Update filters**: Filter by `--major`, `--minor`, or `--patch` updates
- **Interactive mode**: Approve updates individually with `-i`
- **Check mode**: Exit with code 1 if updates available (for CI/pre-commit)
- **Major warnings**: Highlights breaking changes with `(MAJOR)`
- **Format-preserving**: Keeps formatting, comments, and structure
- **Pre-release aware**: Updates pre-releases to newer pre-releases
- **Gitignore-aware**: Respects `.gitignore` when discovering files
- **Version alignment**: Align package versions across multiple files

## Installation

### From crates.io

```bash
cargo install upd

# or with cargo-binstall (faster, pre-built binary)
cargo binstall upd
```

### From PyPI

```bash
pip install upd-cli
# or with uv
uv pip install upd-cli
```

### From source

```bash
git clone https://github.com/rvben/upd
cd upd
cargo install --path .
```

## Usage

```bash
# Update all dependency files in current directory
upd

# Update specific files or directories
upd requirements.txt pyproject.toml

# Dry-run mode (preview changes without writing)
upd -n
upd --dry-run

# Verbose output
upd -v
upd --verbose

# Disable colored output
upd --no-color

# Disable caching (force fresh lookups)
upd --no-cache

# Filter by update type
upd --major      # Show only major (breaking) updates
upd --minor      # Show only minor updates
upd --patch      # Show only patch updates

# Combine filters
upd --major --minor  # Show major and minor updates only

# Interactive mode - approve updates one by one
upd -i
upd --interactive

# Filter by language/ecosystem
upd --lang python           # Update only Python dependencies
upd -l rust                 # Short form
upd --lang python --lang go # Update Python and Go only

# Version precision
upd --full-precision  # Output full versions (e.g., 3.1.5 instead of 3.1)

# Check mode - exit with code 1 if updates available (for CI/pre-commit)
upd --check
upd -c
upd --check --lang python  # Check only Python dependencies
```

### Commands

```bash
# Show version
upd version

# Check for upd updates
upd self-update

# Clear version cache
upd clean-cache

# Align versions across files (use highest version found)
upd align
upd align --check  # Exit 1 if misalignments found (for CI)
```

## Supported Files

### Python

- `requirements.txt`, `requirements-dev.txt`, `requirements-*.txt`
- `requirements.in`, `requirements-dev.in`, `requirements-*.in`
- `dev-requirements.txt`, `*-requirements.txt`, `*_requirements.txt`
- `pyproject.toml` (PEP 621 and Poetry formats)

### Node.js

- `package.json` (`dependencies` and `devDependencies`)

### Rust

- `Cargo.toml` (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`)

### Go

- `go.mod` (`require` blocks)

## Example Output

```text
pyproject.toml:12: Would update requests 2.28.0 → 2.31.0
pyproject.toml:13: Would update flask 2.2.0 → 3.0.0 (MAJOR)
Cargo.toml:8: Would update serde 1.0.180 → 1.0.200
Cargo.toml:9: Would update tokio 1.28.0 → 1.35.0

Would update 4 package(s) in 2 file(s), 15 up to date
```

Output includes clickable `file:line:` locations (recognized by VS Code, iTerm2, and modern terminals).

## Version Precision

By default, `upd` preserves version precision from the original file:

```text
# Original file has 2-component versions
flask>=2.0        →  flask>=3.1        (not 3.1.5)
django>=4         →  django>=6         (not 6.0.0)

# Original file has 3-component versions
requests>=2.0.0   →  requests>=2.32.5
```

Use `--full-precision` to always output full semver versions:

```text
upd --full-precision
flask>=2.0        →  flask>=3.1.5
django>=4         →  django>=6.0.0
requests>=2.0.0   →  requests>=2.32.5
```

## Version Alignment

In monorepos or projects with multiple dependency files, the same package might have different versions:

```text
# requirements.txt
requests==2.28.0

# requirements-dev.txt
requests==2.31.0

# services/api/requirements.txt
requests==2.25.0
```

Use `upd align` to update all occurrences to the highest version found:

```bash
upd align              # Align all packages to highest version
upd align --dry-run    # Preview changes
upd align --check      # Exit 1 if misalignments (for CI)
upd align --lang python # Align only Python packages
```

**Behavior:**

- Only aligns packages within the same ecosystem (Python with Python, etc.)
- Skips packages with upper bound constraints (e.g., `>=2.0,<3.0`) to avoid breaking them
- Ignores pre-release versions when finding the highest version

## Version Constraints

`upd` respects version constraints in your dependency files:

| Constraint | Behavior |
|------------|----------|
| `>=2.0,<3` | Updates within 2.x range only |
| `^2.0.0` | Updates within 2.x range (npm/Cargo) |
| `~2.0.0` | Updates within 2.0.x range (npm) |
| `>=2.0` | Updates to any version >= 2.0 |
| `==2.0.0` | No updates (pinned) |

## Caching

Version lookups are cached for 24 hours in:

- macOS: `~/Library/Caches/upd/versions.json`
- Linux: `~/.cache/upd/versions.json`
- Windows: `%LOCALAPPDATA%\upd\versions.json`

Use `upd clean-cache` to clear the cache, or `upd --no-cache` to bypass it.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `UV_INDEX_URL` | Custom PyPI index URL |
| `PIP_INDEX_URL` | Custom PyPI index URL (fallback) |
| `NPM_REGISTRY` | Custom npm registry URL |
| `GOPROXY` | Custom Go module proxy URL |
| `UPD_CACHE_DIR` | Custom cache directory |

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
