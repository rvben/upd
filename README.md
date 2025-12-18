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
- **Security auditing**: Check dependencies for known vulnerabilities via OSV
- **Config file support**: Ignore or pin packages via `.updrc.toml`
- **Private registries**: Authentication for PyPI, npm, Cargo, and Go

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

# Use a specific config file
upd --config /path/to/config.toml
upd --config .updrc.toml
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

# Check for security vulnerabilities
upd audit
upd audit --check  # Exit 1 if vulnerabilities found (for CI)
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

## Security Auditing

Check your dependencies for known security vulnerabilities using the [OSV (Open Source Vulnerabilities)](https://osv.dev/) database:

```bash
upd audit              # Scan all dependency files
upd audit --dry-run    # Same as audit (read-only operation)
upd audit --check      # Exit 1 if vulnerabilities found (for CI)
upd audit --lang python # Audit only Python packages
upd audit ./services   # Audit specific directory
```

**Example output:**

```text
Checking 42 unique package(s) for vulnerabilities...

⚠ Found 3 vulnerability/ies in 2 package(s):

  ● requests@2.19.0 (PyPI)
    ├── GHSA-j8r2-6x86-q33q [CVSS:3.1/AV:N/AC:H/PR:N/UI:R/S:C/C:H/I:N/A:N] Unintended leak of Proxy-Authorization header
    │   Fixed in: 2.31.0
    │   https://github.com/psf/requests/security/advisories/GHSA-j8r2-6x86-q33q

  ● flask@0.12.2 (PyPI)
    ├── GHSA-562c-5r94-xh97 [CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H] Denial of Service vulnerability
    │   Fixed in: 0.12.3
    │   https://nvd.nist.gov/vuln/detail/CVE-2018-1000656
    ├── GHSA-m2qf-hxjv-5gpq [CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N] Session cookie disclosure
    │   Fixed in: 2.3.2
    │   https://github.com/pallets/flask/security/advisories/GHSA-m2qf-hxjv-5gpq

Summary: 2 vulnerable package(s), 3 total vulnerability/ies
```

**Features:**

- Queries the OSV API (free, no API key required)
- Supports all ecosystems: PyPI, npm, crates.io, Go
- Deduplicates packages across files
- Shows severity scores (CVSS), descriptions, and fixed versions
- Batch queries for efficiency (up to 1000 packages per request)

**CI/CD Integration:**

```yaml
# GitHub Actions example
- name: Check for vulnerabilities
  run: upd audit --check
```

```yaml
# Pre-commit hook (.pre-commit-config.yaml)
repos:
  - repo: local
    hooks:
      - id: upd-audit
        name: security audit
        entry: upd audit --check
        language: system
        pass_filenames: false
        stages: [pre-push]
```

## Version Constraints

`upd` respects version constraints in your dependency files:

| Constraint | Behavior |
|------------|----------|
| `>=2.0,<3` | Updates within 2.x range only |
| `^2.0.0` | Updates within 2.x range (npm/Cargo) |
| `~2.0.0` | Updates within 2.0.x range (npm) |
| `>=2.0` | Updates to any version >= 2.0 |
| `==2.0.0` | No updates (pinned) |

## Configuration File

`upd` supports configuration files to customize update behavior on a per-project basis.

### File Discovery

`upd` searches for configuration files in the following order (first found wins):

1. `.updrc.toml` - Recommended, explicit config file
2. `upd.toml` - Alternative name
3. `.updrc` - Minimal name (TOML format)

The search starts from the target directory and walks up to parent directories, allowing you to place a config file at the repository root.

### Configuration Options

```toml
# .updrc.toml

# Packages to ignore during updates (never updated)
ignore = [
    "legacy-package",
    "internal-tool",
]

# Pin packages to specific versions (bypasses registry lookup)
[pin]
flask = "2.3.0"
django = "4.2.0"
```

### Options

| Option | Type | Description |
|--------|------|-------------|
| `ignore` | `string[]` | List of package names to skip during updates |
| `pin` | `table` | Map of package names to pinned versions |

### Example Configurations

**Ignore unstable packages:**

```toml
# .updrc.toml
ignore = [
    "experimental-api",
    "beta-feature",
]
```

**Pin critical dependencies:**

```toml
# .updrc.toml
[pin]
django = "4.2.0"      # LTS version
sqlalchemy = "2.0.0"  # Major version boundary
```

**Combined configuration:**

```toml
# .updrc.toml
ignore = ["internal-utils"]

[pin]
requests = "2.31.0"
flask = "3.0.0"
```

### Verbose Output

Use `--verbose` to see which packages are ignored or pinned:

```bash
upd --verbose
# Output:
# Using config from: .updrc.toml
#   Ignoring 2 package(s)
#   Pinning 3 package(s)
# pyproject.toml:12: Pinned flask 2.2.0 → 3.0.0 (pinned)
# pyproject.toml:13: Skipped internal-utils 1.0.0 (ignored)
```

## Caching

Version lookups are cached for 24 hours in:

- macOS: `~/Library/Caches/upd/versions.json`
- Linux: `~/.cache/upd/versions.json`
- Windows: `%LOCALAPPDATA%\upd\versions.json`

Use `upd clean-cache` to clear the cache, or `upd --no-cache` to bypass it.

## Private Repositories

`upd` supports private package registries for all ecosystems. Credentials are automatically detected from environment variables and configuration files.

### PyPI / Private Python Index

```bash
# Option 1: Environment variables
export UV_INDEX_URL=https://my-private-pypi.com/simple
export UV_INDEX_USERNAME=myuser
export UV_INDEX_PASSWORD=mypassword

# Option 2: PIP-style environment variables
export PIP_INDEX_URL=https://my-private-pypi.com/simple
export PIP_INDEX_USERNAME=myuser
export PIP_INDEX_PASSWORD=mypassword

# Option 3: ~/.netrc file
# machine my-private-pypi.com
# login myuser
# password mypassword

# Option 4: pip.conf / pip.ini
# ~/.config/pip/pip.conf (Linux/macOS)
# %APPDATA%\pip\pip.ini (Windows)
[global]
index-url = https://my-private-pypi.com/simple
extra-index-url = https://pypi.org/simple

# Option 5: Inline in requirements.txt (with credentials)
# --index-url https://user:pass@my-private-pypi.com/simple
# or just the URL (credentials from netrc):
# --index-url https://my-private-pypi.com/simple
```

**pip.conf locations** (searched in order):

1. `$PIP_CONFIG_FILE` environment variable
2. `$VIRTUAL_ENV/pip.conf` (if in a virtual environment)
3. `$XDG_CONFIG_HOME/pip/pip.conf` or `~/.config/pip/pip.conf`
4. `~/.pip/pip.conf`
5. `/etc/pip.conf` (system-wide)

**Inline index URLs**: When a `requirements.txt` file contains `--index-url` or `-i`,
`upd` automatically uses that index instead of the default PyPI. Credentials can be
embedded in the URL (`https://user:pass@host/simple`) or looked up from `~/.netrc`.

### npm / Private Registry

```bash
# Option 1: Environment variables
export NPM_REGISTRY=https://npm.mycompany.com
export NPM_TOKEN=your-auth-token

# Option 2: NODE_AUTH_TOKEN (GitHub Actions)
export NODE_AUTH_TOKEN=your-auth-token

# Option 3: ~/.npmrc file (global registry)
registry=https://npm.mycompany.com
//npm.mycompany.com/:_authToken=your-auth-token
# Or for environment variable reference:
//npm.mycompany.com/:_authToken=${NPM_TOKEN}

# Option 4: ~/.npmrc file (scoped registries)
@mycompany:registry=https://npm.mycompany.com
//npm.mycompany.com/:_authToken=your-auth-token
@another-scope:registry=https://another.registry.com
```

**Scoped registries**: Packages with scopes (e.g., `@mycompany/package`) will use the
registry configured for that scope in `.npmrc`. This allows mixing public and private
packages in the same project.

### Cargo / Private Registry

```bash
# Option 1: Environment variables
export CARGO_REGISTRY_TOKEN=your-token  # For crates.io default
export CARGO_REGISTRIES_MY_REGISTRY_TOKEN=your-token  # For named registry

# Option 2: ~/.cargo/credentials.toml
[registry]
token = "your-crates-io-token"

[registries.my-private-registry]
token = "your-private-token"

# Option 3: ~/.cargo/config.toml (registry URLs)
[registries.my-private-registry]
index = "https://my-registry.com/git/index"
# or sparse registry:
index = "sparse+https://my-registry.com/index/"
```

**Custom registries**: `upd` reads `~/.cargo/config.toml` to discover custom registry
URLs. Combine with `credentials.toml` for authenticated access.

### Go / Private Module Proxy

```bash
# Option 1: Environment variables
export GOPROXY=https://proxy.mycompany.com
export GOPROXY_USERNAME=myuser
export GOPROXY_PASSWORD=mypassword

# Option 2: Private module patterns
export GOPRIVATE=github.com/mycompany/*,gitlab.mycompany.com/*
export GONOPROXY=github.com/mycompany/*
export GONOSUMDB=github.com/mycompany/*

# Option 3: ~/.netrc file (commonly used with go modules)
# machine github.com
# login myuser
# password mytoken
```

**Private modules**: Set `GOPRIVATE` to specify module patterns that should bypass
the public proxy. `upd` respects these patterns and will attempt direct access
for matching modules.

Use `--verbose` to see when authenticated access is being used:

```bash
upd --verbose
# Output: Using authenticated PyPI access
# Output: Using authenticated npm access
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `UV_INDEX_URL` | Custom PyPI index URL |
| `PIP_INDEX_URL` | Custom PyPI index URL (fallback) |
| `PIP_CONFIG_FILE` | Path to pip configuration file |
| `UV_INDEX_USERNAME` | PyPI username (with UV_INDEX_URL) |
| `UV_INDEX_PASSWORD` | PyPI password (with UV_INDEX_URL) |
| `PIP_INDEX_USERNAME` | PyPI username (with PIP_INDEX_URL) |
| `PIP_INDEX_PASSWORD` | PyPI password (with PIP_INDEX_URL) |
| `NPM_REGISTRY` | Custom npm registry URL |
| `NPM_TOKEN` | npm authentication token |
| `NODE_AUTH_TOKEN` | npm token (GitHub Actions compatible) |
| `CARGO_REGISTRY_TOKEN` | crates.io authentication token |
| `CARGO_REGISTRIES_<NAME>_TOKEN` | Named registry token |
| `GOPROXY` | Custom Go module proxy URL |
| `GOPROXY_USERNAME` | Go proxy username |
| `GOPROXY_PASSWORD` | Go proxy password |
| `GOPRIVATE` | Comma-separated private module patterns |
| `GONOPROXY` | Modules to exclude from proxy |
| `GONOSUMDB` | Modules to exclude from checksum DB |
| `UPD_CACHE_DIR` | Custom cache directory |

## Pre-commit Integration

Add `upd` to your `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/rvben/upd
    rev: v0.0.17  # Use the latest version
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

Both hooks run on `pre-push` by default and trigger when dependency files change.

**Note:** Requires `upd` to be installed and available in PATH.

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
