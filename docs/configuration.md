# Configuration

`upd` supports configuration files to customize update behavior on a
per-project basis.

## File discovery

`upd` searches for configuration files in the following order (first found wins):

1. `.updrc.toml` - Recommended, explicit config file
2. `upd.toml` - Alternative name
3. `.updrc` - Minimal name (TOML format)

The search starts from the target directory and walks up to parent directories,
allowing you to place a config file at the repository root.

Use `--config <FILE>` to point at a specific file, and `--show-config` to print
the effective configuration and exit.

## Configuration options

```toml
# .updrc.toml

# Packages to ignore during updates (never updated)
ignore = [
    "legacy-package",
    "internal-tool",
    "actions/checkout",        # GitHub Actions use owner/repo
    "pre-commit/pre-commit-hooks",  # Pre-commit hooks too
]

# Add otherwise-unknown files to discovery as annotated files. Patterns are
# relative to the directory being scanned.
include = [
    "ansible/roles/*/vars/*.yml",
    "docker-compose.yml",
]

# Drop matching files from discovery. Exclude takes precedence over include.
exclude = ["**/archive/**"]

# Leave SHA-pinned GitHub Actions unchecked for this repository. Checking them
# is the default; --update-action-shas still wins when given.
update_action_shas = false

# Allow the scheduled GitHub security-remediation workflow to maintain its
# rolling pull request. This is false when omitted. Manual dry runs remain
# available either way.
[automation]
security_remediation = true

# Pin packages to specific versions (bypasses registry lookup)
[pin]
flask = "2.3.0"
django = "4.2.0"
"actions/setup-node" = "v4"   # Pin GitHub Actions
"psf/black" = "24.0.0"        # Pin pre-commit hooks
```

| Option | Type | Description |
|--------|------|-------------|
| `ignore` | `string[]` | List of package names to skip during updates |
| `include` | `string[]` | Path globs that add otherwise-unknown files to discovery as annotated files |
| `exclude` | `string[]` | Path globs removed from discovery; takes precedence over `include` |
| `pin` | `table` | Map of package names to pinned versions |
| `update_action_shas` | `bool` | Whether SHA-pinned GitHub Actions are checked and updated. Defaults to `true`; `--update-action-shas` and `--no-update-action-shas` override it |
| `automation.security_remediation` | `bool` | Allow scheduled security remediation to publish or clean up its rolling pull request. Defaults to `false` |
| `normalize` | `table` | Opt-in `pyproject.toml` specifier normalization, configured per section |

Package matching is PEP 503-normalized, so `"Oven-SH/bun"` and `"oven-sh/bun"`
are one key, as are `"foo-bar"` and `"foo_bar"`.

`include` only fills gaps in file-type detection. It never reinterprets a
recognized manifest, so `main.tf` still uses the Terraform parser even when an
include glob matches it. Explicit file paths bypass both discovery globs, just
as they bypass ignore-file filtering. Run with `--verbose` to report files that
contain an `upd:` marker but are not discovery candidates; this diagnostic
inspection is limited to UTF-8 text files up to 1 MiB.

### Normalizing pyproject specifiers

By default, `upd` moves a dependency's lower bound and preserves its other
clauses. `[normalize.pyproject]` opts individual pyproject dependency sections
into a single-clause policy at the release selected by the active policy:

```toml
# .updrc.toml for a library
[normalize.pyproject]
dependencies = "at-least"          # >=
optional-dependencies = "at-least" # >=
dependency-groups = "exact"        # ==
```

The accepted values are `exact` (`==`), `at-least` (`>=`), and `at-most`
(`<=`). Omitted sections retain the default shape-preserving behavior. This is
an explicit policy; `upd` does not infer it from another tool's project type.

Normalization gives bare names a specifier and collapses ranges to one clause,
using the selected release's full version precision. `at-most` writes an
inclusive ceiling; it does not change how the release itself is selected. It preserves extras, markers,
comments, array formatting, and literal-string quotes. Direct URL requirements,
non-index `[tool.uv.sources]` dependencies, Poetry path/git/URL/source
dependencies, and `[tool.poetry.dependencies]` tables are left untouched.

The usual `ignore`, `[pin]`, `--package`, private-index, and cooldown policies
still apply. A bump ceiling applies when the old specifier has an inclusive
lower-bound version to classify; a bare or ceiling-only declaration has no
current-version anchor and therefore no meaningful bump level. Ordered operators reject local-version labels;
`exact` accepts them. Text output reports shape changes as `Would normalize` or
`Normalized`; JSON places them in `files[].normalized[]` and counts them in
`summary.normalized`. Dry runs and `--check` treat them as pending work.
Interactive mode prompts for configured shape changes as well as ordinary
version updates. Configured version-only pins retain their established
automatic behavior.

### Seeing what was ignored or pinned

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
docker = "7d"
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
GitHub releases (covers GitHub Actions, pre-commit, Mise), and Docker Hub.
NuGet, Terraform Registry, and generic OCI tag listings do not expose
per-version publish dates we can consume today; cooldown is reported as
unavailable for those files.

## Caching

Version lookups are cached for 24 hours in:

- macOS: `~/Library/Caches/upd/versions.json`
- Linux: `~/.cache/upd/versions.json`
- Windows: `%LOCALAPPDATA%\upd\versions.json`

Use `upd clean-cache` to clear the cache, or `upd --no-cache` to bypass it.
Set `UPD_CACHE_DIR` to relocate it.

## Environment variables

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

## See also

- [Private registries](private-registries.md) for where these credentials come from
- [Stability](stability.md#stable-configuration) for the configuration compatibility guarantee
