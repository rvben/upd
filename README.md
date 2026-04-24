<p align="center">
  <img src="assets/logo-wide.svg" alt="upd logo" width="400">
</p>

# upd

A fast dependency updater for Python, Node.js, Rust, Go, Ruby, .NET, Terraform, GitHub Actions, pre-commit, and Mise projects, written in Rust.

## Quick Start

```bash
# Preview changes without modifying files (default)
uvx --from upd-cli upd

# Apply updates
uvx --from upd-cli upd --apply

# Or with pipx
pipx run --spec upd-cli upd --apply
```

## Features

- **Multi-ecosystem**: Python, Node.js, Rust, Go, Ruby, .NET, Terraform, GitHub Actions, pre-commit, Mise/asdf
- **Fast**: Parallel registry requests for all dependencies
- **Constraint-aware**: Respects `>=2.0,<3` (Python), `~> 7.1` (Ruby), and `^2.0.0` / `~2.0.0` (npm, Cargo).
  For npm, comparator ranges such as `">=1.0.0 <2.0.0"` are rewritten with a **bump strategy**: the lower
  bound moves to the highest version satisfying the constraint, preserving the upper bound. Hyphen
  (`"1 - 2"`) and OR (`"^1 || ^2"`) ranges are reported as warnings and left untouched.
- **Smart caching**: 24-hour version cache for faster subsequent runs
- **Update filters**: Filter by bump level with `--only-bump <major|minor|patch>` (repeatable) or cap with `--max-bump`
- **Interactive mode**: Approve updates individually with `-i`
- **Check mode**: Exit with code 1 if updates available (for CI/pre-commit)
- **Major warnings**: Highlights breaking changes with `(MAJOR)`
- **Format-preserving**: Keeps formatting, comments, and structure
- **Pre-release aware**: Updates pre-releases to newer pre-releases
- **Gitignore-aware**: Respects `.gitignore` when discovering files
- **Version alignment**: Align package versions across multiple files
- **Security auditing**: Check dependencies for known vulnerabilities via OSV
- **Config file support**: Ignore or pin packages via `.updrc.toml`
- **Private registries**: Authentication for PyPI, npm, Cargo, Go, and GitHub

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
# Preview changes without modifying files (default when no --apply)
upd

# Apply updates to files
upd --apply

# Update specific files or directories (still dry-run without --apply)
upd requirements.txt pyproject.toml

# Apply updates to specific files
upd --apply requirements.txt pyproject.toml

# Dry-run mode (explicit; same as omitting --apply)
upd -n
upd --dry-run

# Verbose output
upd -v
upd --verbose

# Suppress decorative output (errors still shown)
upd --quiet
upd -q

# Disable colored output
upd --no-color

# Disable caching (force fresh lookups)
upd --no-cache

# Filter to only the named packages
upd --package requests
upd --package requests --package flask
upd --package requests,flask

# Filter by bump level (only exact levels)
upd --only-bump major      # Show only major (breaking) updates
upd --only-bump minor      # Show only minor updates
upd --only-bump patch      # Show only patch updates

# Combine filters (repeat --only-bump or comma-separate)
upd --only-bump major --only-bump minor
upd --only-bump major,minor

# Cap by bump level (include up to and including this level)
upd --max-bump minor       # Allow patch + minor, skip major
upd --max-bump patch       # Allow patch only

# Interactive mode - approve updates one by one
upd -i
upd --interactive

# Filter by language/ecosystem
upd --lang python           # Update only Python dependencies
upd -l rust                 # Short form
upd --lang python --lang go # Update Python and Go only
upd --lang actions          # Update only GitHub Actions
upd --lang pre-commit       # Update only pre-commit hooks
upd --lang ruby             # Update only Ruby gems
upd --lang dot-net          # Update only .NET NuGet packages
upd --lang terraform        # Update only Terraform providers/modules
upd --lang mise             # Update only Mise/asdf tools

# Version precision
upd --full-precision  # Output full versions (e.g., 3.1.5 instead of 3.1)

# Check mode - exit with code 1 if updates available (for CI/pre-commit)
upd --check
upd --check --lang python  # Check only Python dependencies

# Print effective configuration and exit
upd --show-config

# Use a specific config file
upd --config /path/to/config.toml
upd -c .updrc.toml         # Short form
```

> **Dry-run by default**: `upd` without `--apply` only previews changes. Pass `--apply` to
> write updates. `--check`, `--dry-run`, and `--interactive` do not require `--apply`.
>
> **VCS-root scoping**: When no path argument is given, `upd` scans from the nearest `.git`
> ancestor directory rather than the current working directory. This prevents accidental
> rewrites when CWD is a subdirectory inside a repository.

### Commands

```bash
# Show version
upd --version

# Check for upd updates
upd self-update

# Clear version cache
upd clean-cache

# Align versions across files (use highest version found)
upd align
upd align --check  # Exit 1 if misalignments found (for CI)

# Check for security vulnerabilities
upd audit
upd audit --check  # Exit 1 if vulnerabilities are found or the audit can't complete (for CI)

# Auto-fix vulnerable packages to minimum safe version, then write changes
upd audit --fix-audit --apply

# Run audit using only the local cache (no network; cache misses are errors)
upd audit --offline

# Emit SARIF 2.1.0 for GitHub Code Scanning upload
upd audit --format sarif > results.sarif
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

### Ruby

- `Gemfile` (gem declarations with version constraints)

### .NET / NuGet

- `.csproj` files (`PackageReference` elements)
- `Directory.Packages.props` and `Directory.Build.props` (`PackageVersion` elements)
- Supports both inline `Version` attributes and child `<Version>` elements
- Queries the NuGet v3 API (`api.nuget.org`)
- Skips range version constraints (`[1.0, 2.0)`)

### Terraform / OpenTofu

- `.tf` files (HCL format)
- Updates `required_providers` version constraints and `module` version declarations
- Queries the Terraform Registry API (`registry.terraform.io`)
- Skips local modules (`./`, `../`) and git sources
- Supports pessimistic constraints (`~> 5.0`)

### GitHub Actions

- `.github/workflows/*.yml` and `.github/workflows/*.yaml`
- Updates `uses:` version references (e.g., `actions/checkout@v3` → `actions/checkout@v4`)
- Skips SHA-pinned actions, branch refs, local actions, and Docker references
- Authenticates via `GITHUB_TOKEN` or `GH_TOKEN` for higher API rate limits

### Pre-commit

- `.pre-commit-config.yaml`
- Updates `rev:` fields for GitHub-hosted hook repositories
- Skips local hooks, meta hooks, and non-GitHub repositories

### Mise / asdf

- `.mise.toml` (`[tools]` section)
- `.tool-versions` (space-delimited format)
- Supports 24+ common dev tools: node, python, go, rust, zig, deno, bun, uv, ruff, terraform, kubectl, helm, and more
- Skips `latest` versions and `cargo:*` tools

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
upd audit --check      # Exit 1 if vulnerabilities are found or the audit can't complete
upd audit --lang python # Audit only Python packages
upd audit ./services   # Audit specific directory

# Auto-fix: bump each vulnerable package to the minimum safe version
# (max of fixed_version across all its vulnerabilities). Packages with
# no fixed_version are reported but left untouched.
upd audit --fix-audit --apply

# Offline mode: use only cached OSV responses; cache misses are errors
upd audit --offline

# SARIF 2.1.0 output for GitHub Code Scanning
upd audit --format sarif > results.sarif
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

**Supported ecosystems for auditing:** PyPI, npm, crates.io, Go, RubyGems, NuGet

**CI/CD Integration:**

```yaml
# GitHub Actions example — fail the build on vulnerabilities
- name: Check for vulnerabilities
  run: upd audit --check

# Upload SARIF results to GitHub Code Scanning
- name: Audit dependencies (SARIF)
  run: upd audit --format sarif > results.sarif
- name: Upload to Code Scanning
  uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: results.sarif
```

## Version Constraints

`upd` respects version constraints in your dependency files:

| Constraint | Behavior |
|------------|----------|
| `>=2.0,<3` | Updates within 2.x range only |
| `^2.0.0` | Updates within 2.x range (npm/Cargo) |
| `~2.0.0` | Updates within 2.0.x range (npm) |
| `~> 7.1` | Updates within 7.x range (Ruby pessimistic) |
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
    "actions/checkout",        # GitHub Actions use owner/repo
    "pre-commit/pre-commit-hooks",  # Pre-commit hooks too
]

# Pin packages to specific versions (bypasses registry lookup)
[pin]
flask = "2.3.0"
django = "4.2.0"
"actions/setup-node" = "v4"   # Pin GitHub Actions
"psf/black" = "24.0.0"        # Pin pre-commit hooks
```

### Options

| Option | Type | Description |
|--------|------|-------------|
| `ignore` | `string[]` | List of package names to skip during updates |
| `pin` | `table` | Map of package names to pinned versions |

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

## Cooldown (minimum release age)

Hold back updates to versions that have been public for less than N days.
Reduces exposure to supply-chain attacks that rely on freshly published
malicious versions being installed before detection. Modelled after
Renovate's `minimumReleaseAge` / Dependabot's `cooldown`.

Enable in `.updrc.toml`:

```toml
[cooldown]
default = "7d"           # applies to every ecosystem unless overridden

[cooldown.ecosystem]
npm = "14d"              # stricter for npm
pypi = "14d"
"crates.io" = "3d"
```

Duration syntax: `<integer><unit>` where unit is `s`, `m`, `h`, `d`, `w`.
A bare `0` disables cooldown.

Override from the CLI for one-off runs:

```text
upd --min-age 14d         # use 14 days regardless of config
upd --min-age 0           # disable cooldown entirely for this run
```

**How it works:** when the latest version is still inside the cooldown
window, `upd` updates to the newest version that *is* old enough. If nothing
newer is old enough yet, the package is held back. Output marks these
packages explicitly:

```text
requirements.txt: Updated requests 2.28.0 → 2.31.0
package.json: Held back lodash 4.17.20 → 4.17.21 (4.17.22 released 2d ago, cooldown 7d)
package.json: Skipped express (only newer version 4.19.0 released 1d ago, cooldown 7d)
```

**Supported ecosystems:** PyPI, npm, crates.io, Go modules, RubyGems,
GitHub releases (covers GitHub Actions, pre-commit, Mise). NuGet and
Terraform Registry do not expose per-version publish dates we can
consume today; cooldown is reported as unavailable for those files.

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

### GitHub (Actions & Pre-commit)

```bash
# Option 1: GITHUB_TOKEN (automatically available in GitHub Actions)
export GITHUB_TOKEN=ghp_your-token-here

# Option 2: GH_TOKEN (used by the gh CLI)
export GH_TOKEN=ghp_your-token-here
```

Without a token, the GitHub API rate limit is 60 requests/hour. With a token, it's 5,000 requests/hour.

Use `--verbose` to see when authenticated access is being used:

```bash
upd --verbose
# Output: Using authenticated PyPI access
# Output: Using authenticated npm access
# Output: Using authenticated GitHub access
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
| `GITHUB_TOKEN` | GitHub API token (for Actions and pre-commit) |
| `GH_TOKEN` | GitHub API token (gh CLI compatible) |
| `UPD_CACHE_DIR` | Custom cache directory |

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

Both hooks run on `pre-push` by default. Uses `language: python` which installs `upd-cli` from PyPI automatically — no manual installation needed.

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

## Stability

Starting with `0.1.0`, `upd` commits to the following public surfaces.
Anything listed here will not change in a backwards-incompatible way
without a major-version bump.

### Stable CLI

Global flags (accepted on every subcommand):

| Flag | Short | Purpose |
|------|-------|---------|
| `--apply` | | Write changes to files (omit for dry-run preview) |
| `--dry-run` | `-n` | Preview changes without writing (explicit form) |
| `--verbose` | `-v` | Verbose output |
| `--quiet` | `-q` | Suppress decorative output (errors still shown) |
| `--interactive` | `-i` | Approve each update individually |
| `--check` | | Exit 1 if updates/misalignments/vulnerabilities found |
| `--only-bump <major\|minor\|patch>` | | Restrict to exactly these bump levels (repeatable, comma-separated) |
| `--max-bump <major\|minor\|patch>` | | Include updates up to and including this level |
| `--package <NAME>` | | Restrict to named packages (repeatable, comma-separated) |
| `--lang <LANG>` | `-l` | Filter by ecosystem (repeatable) |
| `--full-precision` | | Output full versions |
| `--no-cache` | | Disable version cache |
| `--no-color` | | Disable colored output |
| `--lock` | | Regenerate lockfiles after updates |
| `--config <FILE>` | `-c` | Use a specific config file |
| `--show-config` | | Print effective configuration and exit |
| `--format <text\|json\|sarif>` | | Output format (`sarif` applies to `audit`) |
| `--version` | `-V` | Print version (built-in clap flag) |
| `--help` | `-h` | Print help (built-in clap flag) |

Subcommands: `update` (default), `align`, `audit`, `clean-cache`, `self-update`.

#### Commands run by `--lock`

`upd --lock` runs the narrowest per-ecosystem refresh command that
updates only the packages `upd` just rewrote. Targeted forms are used
wherever the package manager supports them; targeting falls back to
`--lockfile-only` flags where no per-package form exists; otherwise
the manifest-wide refresh command is used.

| Ecosystem | Lockfile                 | Command                                        |
|-----------|--------------------------|------------------------------------------------|
| Python    | `poetry.lock`            | `poetry lock --no-update`                      |
| Python    | `uv.lock`                | `uv lock`                                      |
| Node      | `package-lock.json`      | `npm install --package-lock-only`              |
| Node      | `yarn.lock`              | `yarn install --mode update-lockfile`          |
| Node      | `pnpm-lock.yaml`         | `pnpm install --lockfile-only`                 |
| Node      | `bun.lockb`              | `bun install`                                  |
| Rust      | `Cargo.lock`             | `cargo update -p <changed> -p <changed> …`     |
| Go        | `go.sum`                 | `go mod tidy` (no targeted form)               |
| Ruby      | `Gemfile.lock`           | `bundle lock --update <changed> …`             |
| .NET      | `packages.lock.json`     | `dotnet restore` (no targeted form)            |
| Terraform | `.terraform.lock.hcl`    | `terraform providers lock` (no targeted form)  |

Manifests whose `upd` pass produced zero changes have their lockfile
refresh skipped entirely. A directory where only config pins were
applied is still refreshed, and the changed-package list includes
those pinned packages so `cargo update -p <pkg>` / `bundle lock --update <pkg>` stay scoped.

Stable `audit`-specific flags:

| Flag | Purpose |
|------|---------|
| `--fix-audit` | Bump each vulnerable package to minimum safe version |
| `--offline` | Use only cached OSV responses; cache misses are errors |
| `--format sarif` | Emit SARIF 2.1.0 for GitHub Code Scanning |

### Stable exit codes

| Code | Meaning |
|------|---------|
| `0` | Success — no action required, or updates applied cleanly |
| `1` | `--check` flagged pending updates / misalignments / vulnerabilities, or an audit could not complete |
| `2` | Invalid CLI arguments or unparseable configuration (clap default) |
| `3` | Vulnerabilities found (`upd audit`). Pass `--no-fail` to force exit 0. |

### Stable output

- **Text output** is designed for humans. Exact wording, colour, and spacing may change between minor versions — do not parse it.
- **JSON output** (`--format json`) follows an additive schema. New
  fields may appear in minor releases; existing fields will not change
  type, be renamed, or be removed before `1.0`.

### Stable configuration

- `.updrc.toml` / `upd.toml` / `.updrc` discovery order and the `ignore` array + `[pin]` table are stable.
- New top-level keys may be added in minor releases, but will always default to the pre-existing behaviour.

### Not covered by stability guarantees

- Error message wording and verbose/debug log lines.
- Cache file layout on disk (`$UPD_CACHE_DIR/versions.json`).
- The `upd` Rust library crate — internal types may change between any releases. Depend on the CLI, not the crate.

## License

MIT
