# Ecosystems and supported files

Which files `upd` discovers, and what it does with each one.

GitHub Actions has its own page: [GitHub Actions](github-actions.md).

## Discovery

When no path argument is given, `upd` scans from the nearest `.git` ancestor
directory rather than the current working directory. This prevents accidental
rewrites when the working directory is a subdirectory inside a repository.

Discovery honors `.gitignore`, `.git/info/exclude`, and the global gitignore,
even outside a git repo. Hidden directories are pruned by default; `upd` only
opens the dotfiles it actually updates (`.github/workflows`,
`.pre-commit-config.yaml`, `.mise.toml`, `.tool-versions`). Use `--no-ignore`
to walk every file regardless.

An explicitly passed file path bypasses discovery entirely, which is how
`upd update path/to/versions.env` works for a file no pattern claims.

A symlink to a manifest, whether passed explicitly or found by the walk, is
scanned as the file it points at: the type comes from that file's name, its
lockfile is looked for beside it, and an update is written into it, so the
link survives. The report names the file rather than the link.

## Lockfiles

`update` reads manifests, not lockfiles. Whether a dependency is outdated is
decided from the version the manifest declares and what the registry
publishes, so a `uv.lock` or `package-lock.json` that already resolves to a
newer release changes nothing about what `upd` reports. A lockfile beside a
changed manifest is rewritten only under `--lock`, by the project's own
package manager; the complete commands are listed under
[Commands run by `--lock`](stability.md#commands-run-by---lock).

`audit` does read lockfiles. The lockfile beside a manifest is scanned for
the resolved versions it records, including transitive packages the manifest
never names, so advisories are matched against what the project installs
rather than against the ranges it declares. See [Audit](audit.md).

## Python

- `requirements.txt`, `requirements-dev.txt`, `requirements-*.txt`
- `requirements.in`, `requirements-dev.in`, `requirements-*.in`
- `dev-requirements.txt`, `*-requirements.txt`, `*_requirements.txt`
- `pyproject.toml` (PEP 621 and Poetry formats)
- `uv.lock` and `poetry.lock`: read for audit; refreshed by the package manager
  under `--lock`, rather than rewritten directly by ordinary updates

In addition to project dependencies, optional dependencies, and dependency
groups, `pyproject.toml` updates cover string requirements in
`tool.uv.constraint-dependencies`, `tool.uv.build-constraint-dependencies`,
`tool.uv.override-dependencies`, and `tool.uv.dev-dependencies`. Their existing
specifier shapes and additional clauses are preserved. Constraint and override
entries are not movable alignment targets. Direct Git, URL and path sources
are preserved; marker-dependent source alternatives remain untouched.

Reports identify the complete section (including group name) and show the
full old and new constraint. JSON updates add `section`, `previous_spec`, and
`new_spec` without changing the existing version fields.

### uv upload cutoffs

Python `update` candidate selection, including lock-only `--package` updates,
honors `[tool.uv].exclude-newer`,
`exclude-newer-package`, and per-index `exclude-newer`. Package overrides win
over index overrides, which win over the global setting. Each setting accepts
`false` to opt out. Dates include the entire local calendar day; RFC 3339
cutoffs exclude artifacts uploaded at or after the timestamp. Friendly
durations (`1 week`, `24 hours`) and ISO durations (`P7D`, `PT24H`) are resolved
once for the manifest operation. Calendar months and years are rejected.

```toml
[tool.uv]
exclude-newer = "1 week"
exclude-newer-package = { setuptools = false }

[[tool.uv.index]]
name = "internal"
url = "https://internal.example.com/simple"
exclude-newer = false
```

Cutoffs use each distribution artifact's upload time, before checking Python
compatibility. A recent wheel cannot borrow an older source archive's upload
time. Simple JSON `upload-time`, HTML `data-upload-time`, and legacy PyPI
`upload_time_iso_8601` are retained. Missing upload times make files unavailable
under a cutoff, except for package opt-outs or indexes with an explicit cutoff
or opt-out, following uv. Invalid timestamps cannot establish eligibility.

Workspace members use their uv workspace root's cutoff and index settings.
Existing upd cooldown settings and `--min-age` are additional restrictions;
uv opt-outs do not disable them. An explicit upd `[pin]` also has to pass the
active uv cutoff. An empty eligible set is reported as an error and leaves the
affected declaration unchanged.

This covers declared upload policies and the dependency arrays above, not
full uv resolver equivalence. Candidate checks do not solve the complete
transitive graph, emulate every uv setting, or load cutoff settings from
`uv.toml`, user configuration, or `UV_EXCLUDE_NEWER` environment overrides.
Relative cutoffs use the operation's time rather than a historical timestamp
stored in `uv.lock`. Use an absolute cutoff when reproducing an older resolution.
Audit fixes use advisory-provided versions and do not run this candidate filter.
The package manager remains responsible for final resolution; a failed
`--lock` refresh triggers the existing rollback behavior.

### Python compatibility

When `[project].requires-python` or `[tool.poetry.dependencies].python` is
present, update checks select the newest release whose non-yanked files cover
the declared Python range. For example, `>=3.10` prevents selecting a release
that requires `>=3.11`. Upper bounds, exclusions, compatible-release constraints,
and Poetry caret, tilde, wildcard, and union constraints are supported. When
both declarations exist, their intersection defines the resolver range.
Requirements files inherit the nearest enclosing `pyproject.toml`, stopping at
the repository boundary. Without a declaration, selection is unchanged; the
installed interpreter and `.python-version` do not override project metadata.

Compatibility checks read per-file `Requires-Python` from Simple JSON, Simple
HTML, or the legacy PyPI JSON API, including private indexes. Missing metadata
is treated as unrestricted; files with invalid metadata cannot establish
compatibility. An empty compatible set reports an error and leaves that
dependency unchanged. Explicit configuration pins remain user overrides.
Cooldown candidates are filtered by the same Python range. Complete metadata is
cached in memory for the current run, independently of project constraints;
previously cached latest-version answers cannot bypass compatibility checks.
When Python compatibility selects an older candidate, text and JSON reports
explain the selected version, the newer release's `Requires-Python`, and the
project range (including the dependency marker, when present). For example:
`demo: Python compatibility selects 1.5 instead of 2.0; 2.0 declares
Requires-Python '>=3.11'; project supports >=3.10`. This describes compatibility
selection; cooldowns and bump limits can further restrict the final update.

This is an interpreter-metadata check, not a full dependency resolution. It
does not validate platform wheel tags or transitive dependencies. PEP 508 dependency
markers narrow the Python range separately for each dependency occurrence,
including `python_version`, `python_full_version`, and combined `and`/`or`
expressions. Platform and extra conditions consider all possible environments;
they are not evaluated against the machine running `upd`. Dependencies whose
markers cannot apply to the project Python range stay unchanged without a
registry lookup. Invalid markers report an error and remain unchanged.
Use `--lock` to have the package manager validate the resulting dependency set.

## Node.js

- `package.json` (`dependencies` and `devDependencies`)

## Rust

- `Cargo.toml` (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`)

## Go

- `go.mod` (`require` blocks)

## Ruby

- `Gemfile` (gem declarations with version constraints)

## .NET / NuGet

- `.csproj` files (`PackageReference` elements)
- `Directory.Packages.props` and `Directory.Build.props` (`PackageVersion` elements)
- Supports both inline `Version` attributes and child `<Version>` elements
- Queries the NuGet v3 API (`api.nuget.org`)
- Does not rewrite interval-notation ranges (`[1.0,2.0)`), but reports them:
  up to date when the range admits the newest release, a warning when the
  release has outgrown it, an error when the notation cannot be read

## Docker / OCI images

- `Dockerfile` and `Dockerfile.*` (`FROM` references, including multi-stage files)
- `compose.yml`, `compose.yaml`, `docker-compose.yml`, `docker-compose.yaml`, and
  their named variants such as `compose.production.yml`
- Updates numeric tag channels while preserving their shape and suffix:
  `alpine:3.22` can move to `3.23`, and `rust:1.90-alpine` stays on the
  `*-alpine` channel
- Supports Docker Hub shorthand, explicit registries and ports, quoted Compose
  values, and defaults such as `${APP_IMAGE:-ghcr.io/acme/app:1.2.3}`
- Queries Docker Hub and OCI Distribution-compatible registries. Anonymous
  bearer-token challenges are handled automatically, private GHCR images can
  use GitHub Actions' repository token with `packages: read`, and Docker Hub
  lookups fall back to its OCI registry when the richer tag endpoint is
  unavailable
- Reports floating tags such as `latest`, runtime-only variables, and digest
  pins explicitly instead of guessing or claiming they are current
- Preserves comments, quoting, line endings, and every byte outside the tag

Docker image tags are mutable registry labels, not package releases. `upd`
therefore follows the exact numeric channel already chosen in the file and does
not cross between suffixes, precision levels, or `v`-prefixed and unprefixed
tags. Updating `tag@sha256:digest` safely also requires resolving and verifying
the replacement manifest digest, so digest pins are blocked in this release.

## Terraform / OpenTofu

- `.tf` files (HCL format)
- Updates `required_providers` version constraints and `module` version declarations
- Queries the Terraform Registry API (`registry.terraform.io`)
- Skips local modules (`./`, `../`) and git sources
- Supports pessimistic constraints (`~> 5.0`)

## GitHub Actions

- `.github/workflows/*.yml` and `.github/workflows/*.yaml`
- Updates `uses:` version references (e.g., `actions/checkout@v3` → `actions/checkout@v4`)
- Supports actions and reusable workflows
- Checks SHA-pinned actions by default
- Skips branch refs, local actions, and Docker references
- Authenticates via `GITHUB_TOKEN` or `GH_TOKEN` for higher API rate limits

SHA pinning, the safety rules around rewriting a commit pin, and the reusable
pull-request workflow are covered in [GitHub Actions](github-actions.md).

## Pre-commit

- `.pre-commit-config.yaml`
- Updates `rev:` fields for GitHub-hosted hook repositories
- Skips local hooks, meta hooks, and non-GitHub repositories

## Mise / asdf

- `.mise.toml`: `[tools]` and `[tools.<name>]`, with the version written as a
  string (`rust = "1.91.1"`), an inline table (`uv = { version = "0.12.5" }`)
  or an array (`node = ["20.11.0", "18.0.0"]`, first entry only)
- `.tool-versions` (space-delimited format; first version on a line only)

An entry that names its backend is checked against that backend's registry:

| Prefix | Registry |
| --- | --- |
| `cargo:` | crates.io |
| `npm:` | npm |
| `pipx:` | PyPI |
| `gem:` | RubyGems |
| `dotnet:` | NuGet |
| `go:` | Go module proxy |
| `github:`, `ubi:`, `aqua:` | GitHub releases |

`aqua:` names a package path whose first two segments are the GitHub repository,
so `aqua:kubernetes/kubernetes/kubectl` is checked against
`kubernetes/kubernetes`.

An entry with no prefix is checked when it is one of 24+ common dev tools
(node, python, go, rust, zig, deno, bun, uv, ruff, terraform, kubectl, helm,
and more), whose registry `upd` knows without asking mise.

Everything else is reported rather than dropped, because an unchecked pin is
not an up-to-date one. `upd` counts these in the summary, names them under
`--verbose`, and lists them in `skipped[]` in the JSON report with a reason:

| Reason | Entry |
| --- | --- |
| `unsupported-backend` | a backend with no `upd` registry (`asdf:`, `vfox:`, `http:`, ...) |
| `unknown-tool` | a bare name outside the table above; add a backend prefix to have it checked |
| `symbolic-version` | `latest`, `lts`, `system`, `global`, `ref:*`, `prefix:*`, which mise resolves at install time |

Ignore rules and pins in `.updrc.toml` match the key exactly as the file spells
it, backend prefix included: `ignore = ["cargo:cargo-zigbuild"]`.

## Annotated files

A version pinned in a file `upd` does not otherwise understand can declare its
own source with a trailing comment:

```makefile
BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli
NODE_VERSION := 22.11.0  # upd: npm node
```

- Syntax: `upd: <source> <package>` in a trailing `#` or `//` comment
- Sources: `pypi`, `npm`, `crates`, `go`, `rubygems`, `nuget`, `github-releases`
- Otherwise-unrecognized files scanned by name: `Makefile`, `makefile`,
  `GNUmakefile`, `*.mk`, `justfile`, `Justfile`, `*.sh`, `*.bash`, `*.yml`,
  `*.yaml`. Any other file works when passed explicitly:
  `upd update path/to/versions.env`
- The top-level `include` config key adds otherwise-unknown files to directory
  discovery as annotated files. Patterns are relative to the scanned directory:
  `include = ["deploy/*.env", "config/version.conf"]`
- `include` does not reinterpret a recognized file type (`main.tf` remains
  Terraform), and `exclude` takes precedence when both match
- A GitHub Actions workflow is the exception: it keeps its own updater and is
  scanned for annotations as well, so a tool version passed to an action through
  a `with:` input can be updated. See
  [GitHub Actions](github-actions.md#annotated-versions-in-a-workflow)
- The version on the line is found and rewritten in place, keeping a leading
  `v` and the line's own precision (`v2.60` becomes `v2.65`, not `v2.65.4`)
- One package name may not appear under two different sources in the same file
- `ignore` and `pin` in `.updrc.toml` reach annotated packages by name. Package
  matching is PEP 503-normalized, so `"Oven-SH/bun"` and `"oven-sh/bun"` are
  one key, as are `"foo-bar"` and `"foo_bar"`
- `--lang annotated` selects every annotated line whatever its source, and a
  source's own lang (`--lang github-releases`) selects its lines individually.
  In a workflow these are separate from `--lang actions`, which selects the
  `uses:` refs and nothing else: see
  [GitHub Actions](github-actions.md#selecting-them-with---lang)
- `exclude` filters discovered files with path globs; explicitly passed file
  paths bypass it
- `--verbose` inspects otherwise-unknown UTF-8 text files up to 1 MiB, reports
  those containing an `upd:` annotation marker, and suggests an `include` glob
- `upd align` and `upd audit` ignore annotated lines: a package name is only
  meaningful together with its source, so grouping them across files is not safe

## See also

- [Version constraints](../README.md#version-constraints) and
  [version precision](../README.md#version-precision) in the README
- [Configuration](configuration.md) for ignoring, pinning, and excluding packages
- [Stability](stability.md) for the per-ecosystem lockfile refresh commands
