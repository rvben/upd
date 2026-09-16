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
    "deploy/*.env",
    "config/version.conf",
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
| `ecosystems` | `table` | Persistent enable/disable lists using `--lang` names |
| `update.pyproject.exact-pins` | `bool` | Update concrete `==` pins automatically (default `true`) |
| `normalize` | `table` | Opt-in `pyproject.toml` specifier normalization, configured per section |

Package matching is PEP 503-normalized, so `"Oven-SH/bun"` and `"oven-sh/bun"`
are one key, as are `"foo-bar"` and `"foo_bar"`.

`include` only fills gaps in file-type detection. It never reinterprets a
recognized manifest, so `main.tf` still uses the Terraform parser even when an
include glob matches it. Explicit file paths bypass both discovery globs, just
as they bypass ignore-file filtering. Run with `--verbose` to report files that
contain an `upd:` marker but are not discovery candidates; this diagnostic
inspection is limited to UTF-8 text files up to 1 MiB.

### Ecosystem selection

```toml
[ecosystems]
enable = ["python", "rust", "actions"]
disable = ["rust"]
```

An omitted `enable` includes all ecosystems; `enable = []` includes none.
`disable` removes ecosystems from that selection. An explicit `--lang`
replaces both lists for that invocation. Names are exactly those accepted by
`--lang`; unknown names and misspelled table fields are errors.

This is a root discovery policy, like `include`/`exclude`, shared by update,
interactive update, alignment, audit, and their associated lockfile handling.
Nested configuration does not re-enable files excluded by root discovery.

### Preserving exact Python pins

```toml
[update.pyproject]
exact-pins = false
```

The default is `true`. Set it to `false` to preserve a single concrete `==`
clause in `pyproject.toml`, including when its section has normalization
configured. These declarations are reported as not examined with the reason
`exact-pins-disabled`. Explicit `[pin]` entries still take precedence.

This switch does not apply to `===`, prefix matches such as `==1.2.*`, or
compound constraints. Concrete `===` operands retain their existing update
behavior. Prefix matches, ceiling-only and exclusion-only constraints remain
read-only: they are checked and reported without inventing a lower bound.
Bare names remain unchanged unless normalization is enabled.

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
pre-commit = "30d"       # a language, narrower than the registry it shares
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
NuGet, Gradle Maven metadata, Terraform Registry, and generic OCI tag listings do not expose
per-version publish dates we can consume today; cooldown is reported as
unavailable for those files.

### Keying a cooldown on a language

`[cooldown.ecosystem]` takes a language name as well as a registry name, spelled
the way `--lang` spells it:

| Kind | Accepted keys |
| --- | --- |
| Registry | `pypi`, `npm`, `crates.io`, `go-proxy`, `rubygems`, `nuget`, `gradle`, `github-releases`, `terraform`, `docker` |
| Language | `python`, `node`, `rust`, `go`, `ruby`, `dotnet`, `gradle`, `actions`, `pre-commit`, `mise`, `github-releases`, `terraform`, `docker` |

The language is the narrower key, so where both are set the language wins. That
matters most for GitHub releases, which answers for four languages at once: a
`github-releases` window cannot say anything about pre-commit hooks without
saying the same about Actions pins and mise tools.

```toml
[cooldown.ecosystem]
github-releases = "3d"
pre-commit = "30d"       # hooks wait a month, Actions pins and mise tools 3 days
```

A language key reaches only the dependencies its own registry answers for. An
annotated line names its source, so `# upd: pypi black` inside a workflow is a
PyPI dependency that happens to live in a workflow: it reads `pypi` or `python`,
never the workflow's `actions` key.

`--min-age` is a whole-run answer and still overrides both. Strongest first:
`--min-age`, the language key, the registry key, `[cooldown] default`.

An unknown key is reported as a warning and ignored, so a typo does not quietly
switch a cooldown off.

### Dating a repository that publishes no releases

A GitHub repository whose hook or action is tagged but never released has no
release dates to read, so `upd` dates its versions from the tags themselves: an
annotated tag by its tagger date, a lightweight tag by the date of the commit it
points at. A lightweight tag therefore reads as old as its commit, which can be
older than the day the tag was pushed.

Each date costs two API requests, so the walk dates only the newest five tags
per release track (stable and prerelease counted separately) and stops. What it
drops is the oldest candidates, which cooldown reaches only after rejecting
every newer version as too fresh; such a package is reported as skipped rather
than held back to a tag whose age was never measured.

A release whose tag is not a version is left out of the dates entirely.
github/codeql-action, for example, publishes its CodeQL bundles
(`codeql-bundle-v2.27.0`) as releases beside the action's own, and a bundle is
never a candidate to hold an action back to. A repository whose releases are all
of that kind is dated from its tags, as if it published no releases.

### Lockfiles

A manifest that keeps to the cooldown is not enough on its own: `upd --lock`
refreshes the lockfile afterwards, and the lock tool resolves the newest
release the new requirement allows, including the one the cooldown just held
back, and any transitive release published an hour ago. So the refresh carries
the cooldown too, in whatever form the tool understands.

This covers the refreshes `upd update` runs, interactive ones and the relock
that writes a version floor included. `upd audit --fix-audit` is different: it
moves a vulnerable package to the release that fixes it, however young that
release is, so its refreshes are neither gated nor checked as below.


| Lockfile | How the cooldown reaches it |
| --- | --- |
| `package-lock.json` | `npm install --before`, at the earlier of the cooldown and the project's own `before` or `min-release-age` |
| `pnpm-lock.yaml` | `--config.minimum-release-age` (pnpm 10.16 or newer) |
| `yarn.lock` (Berry) | `YARN_NPM_MINIMAL_AGE_GATE` (Yarn 4.10 or newer); `.yarnrc.yml` is not touched |
| `bun.lock` | `--minimum-release-age` (bun 1.3 or newer) |
| `uv.lock` | `uv lock --exclude-newer`, see below (a uv with `--exclude-newer-package`) |
| `Cargo.lock` | young crates.io entries are moved back with `cargo update --precise` |
| `poetry.lock`, `Gemfile.lock` | not gated, only checked |
| `packages.lock.json`, `.terraform.lock.hcl` | not gated and not checked, reported as such |
| `go.sum` | nothing to gate: it records checksums for the versions `go.mod` names |

A project setting that is already stricter than the cooldown is kept as it is.
Nothing is written to the project's configuration.

For uv, packages the lockfile already held are exempted at their own upload
time, so a locked release is never moved just because it is young. uv records
the cutoff in `uv.lock`, where it would make `uv lock --locked` fail without the
same flags, so upd removes it and runs a plain `uv lock` to confirm the result.
An exemption admits every release of its package up to the exempted time,
including a young one, which is why a gated refresh is still checked as below.

For Cargo, each crates.io entry the refresh introduced inside the cooldown is
held at the newest compatible release outside it, never below what the
lockfile held before. A version floor upd writes to `Cargo.lock` with
`cargo update --precise` is checked the same way: a companion crate it locked
inside the cooldown is held, and the floor itself never is, since it is the
release the run chose. A crate `[pin]` sets to an exact version is a floor in
the same way: no hold moves it below that version. In JSON each hold is an entry in `lockfile_holds` with
`lockfile`, `package`, `from`, `to`, `published_at` (of `from`) and `cooldown`:

```text
✓ Held clap at 4.6.6 in Cargo.lock (4.6.7 released 3h ago, cooldown 7d)
```

Every hold is read back from `Cargo.lock`. One that cargo did not carry out,
that moved another crate below the release the lockfile held before the run, or
that moved a crate below a version floor the run chose (including a floor
another floor's update already reached) is undone, and the entry is reported as
below with the reason as its note. A hold a later hold moved the crate away from
is dropped the same way, since the lockfile the run leaves behind no longer
carries it. When `Cargo.lock` cannot be put back after
such a hold, upd stops holding in that lockfile and reports an error (in JSON,
in the `errors` of the `Cargo.toml` it belongs to), and the run exits 2.

**What gets reported.** After the refresh, every lockfile upd can read
(`package-lock.json`, `uv.lock`, `poetry.lock`, `Cargo.lock`, `Gemfile.lock`) is
read back and each new entry is looked up, including a lockfile refreshed under
the tool's own gate: that gate exempts packages (npm `min-release-age-exclude`,
the uv exemptions above) and admits a release with no publish date. A release
still inside the cooldown is a warning, in JSON an entry in `lockfile_cooldown`
with `lockfile`, `package`, `version`, `published_at`, `cooldown` and, when upd
tried to move it back and could not, a `note`:

```text
Warning: Cargo.lock locks foo 1.2.4, released 2h ago, inside the 7d cooldown; no release outside the cooldown can replace it
```

An entry whose age upd cannot establish is never taken as outside the
cooldown. When the registry lookup fails, or the registry does not list the
version or lists no publish date for it, the entry is named in `warnings`:

```text
Warning: uv.lock: foo 1.2.4 could not be checked against the 7d cooldown (the registry does not list it)
```

Release dates are read only from the registries upd itself reads: the Python
indexes and npm registries set up as in
[Private registries](private-registries.md) (pypi.org and registry.npmjs.org
otherwise, with `.npmrc` scope registries for scoped packages), the indexes the
`pyproject.toml` beside a Python lockfile declares, crates.io and rubygems.org.
A Python entry is dated by the very index the lockfile records it came from,
and a release the refresh moved to another index counts as new. So does a
crate moved to another Cargo registry, or a gem moved to another gem server,
at the same version.
A new entry the lockfile records as coming from anywhere else is not looked up;
each lockfile names those entries in one warning instead. A `Gemfile.lock`
section that lists another gem server beside rubygems.org does not say which
one served each gem, so its gems count as coming from elsewhere. Git and path
dependencies are not registry releases and are left out.

```text
Warning: uv.lock: 2 new entries come from a registry upd does not read (internal-auth 2.1.0, internal-db 0.4.0), so they were not checked against the 7d cooldown
```

A refresh that ran without the gate after all says so and why, in `warnings`:
a gated resolution that failed and was rerun without it, or a uv that resolved
again once the cutoff was removed. Its lockfile is still checked as above.

```text
Warning: uv.lock was refreshed without the 7d cooldown (the gated refresh failed: error: No solution found when resolving dependencies)
```

A tool too old to have the setting is not a warning on its own when upd can
check the lockfile it wrote. Where it cannot (pnpm, Yarn, bun, NuGet and
Terraform lockfiles),
the warning names the reason and adds that the new entries were not checked.

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
