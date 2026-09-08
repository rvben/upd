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

## Gradle (JVM / Android)

- `*.versions.toml` catalogs, including `gradle/libs.versions.toml`
- Literal Maven dependency and plugin versions in `build.gradle`, `build.gradle.kts`,
  `settings.gradle`, and `settings.gradle.kts`
- `gradle-wrapper.properties` distribution versions and SHA-256 checksums
- Select with `--lang gradle`
- Libraries use Maven Central; plugins use Gradle Plugin Portal marker artifacts

Catalogs support `[libraries]` entries with `module` or `group`/`name`,
`"group:artifact:version"` shorthand, `[plugins]` entries with `id`, and literal
versions or `version.ref` pointing into `[versions]`. Scripts support
`id("org.example") version "1.2.3"`, Groovy's `id 'org.example' version '1.2.3'`,
and Kotlin's `kotlin("jvm") version "2.2.0"` inside `plugins` blocks.
Within `dependencies` and `constraints` blocks, standard configurations such as
`implementation("group:artifact:1.2.3")`, `testImplementation 'group:artifact:1.2.3'`,
and `classpath(platform("group:bom:1.2.3"))` are supported. Computed versions,
classifiers, artifact extensions, and arbitrary custom configuration methods
are not rewritten.
Comments, quoting, and unrelated bytes are preserved. Actual published version
strings are written in full, even without `--full-precision`: `1.2` must not
be invented by shortening a release named `1.2.3`.

Config and package filters identify libraries as `group:artifact` and plugins
as `gradle-plugin:plugin.id`. For example, preserve a library that must match
an IDE's bundled runtime with:

```toml
[pin]
"org.eclipse.lsp4j:org.eclipse.lsp4j" = "0.21.1"
```

A shared version changes only when every consumer permits the same target.
An ignored, filtered, pinned-to-current, failed, capped, or differently updated
consumer keeps the shared value unchanged, with a warning. Interactive approval
must include all consumers of a shared value; a partial selection is refused.

This is static support for public Maven Central and Plugin Portal packages.
Custom repositories and plugin resolution overrides are not interpreted.
Dynamic versions, snapshots, rich constraints, externally managed versions,
and computed plugin versions are reported as unsupported and left unchanged.
An unsupported consumer of `version.ref` prevents the catalog from being edited.
Scripts containing slash expressions outside comments or quoted strings are
refused because Groovy slashy strings cannot safely be interpreted as code.

### Gradle wrapper

Select the distribution with `--package gradle-wrapper`. Only a single literal
HTTPS distribution URL on `services.gradle.org` or `downloads.gradle.org` is
accepted; `bin`/`all` and the original URL spelling are preserved. The matching
official SHA-256 checksum is fetched before proposing or applying the change.
`distributionSha256Sum` is updated or added atomically with the URL. Missing,
malformed, or unavailable checksum metadata prevents the update. Existing
checksum entries are never silently removed.

This updates the distribution configuration only. It does not execute Gradle,
regenerate wrapper scripts/JARs, or change custom `gradle.properties` version
sources such as IntelliJ target versions or `gradleVersion`. Run the project's
wrapper task when adopting new wrapper bootstrap code, and validate toolchain
compatibility with the project's build and tests.

### Maven audit coverage

`upd audit --lang gradle` reads resolved Maven coordinates from adjacent
`gradle.lockfile` and `buildscript-gradle.lockfile` files. Both Kotlin and Groovy
build-script filenames are recognized. Unlocked declarations are not treated
as a resolved graph. A coverage warning identifies the limits: missing plugin
resolution graphs, IDE/JDK distributions, and the wrapper are not audited.
Malformed lockfiles produce a scan warning instead of a partial clean result.
Gradle lockfile regeneration, automatic audit fixes, and alignment remain
unsupported; remediate through the owning build configuration and re-lock with
Gradle. No Gradle build is executed automatically.

Maven metadata does not provide per-release publication timestamps, so cooldown
is reported as unavailable for this ecosystem.

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
- Dockerfiles support a standalone `# upd: pypi uv` comment immediately above
  a single-line `ARG UV_VERSION=0.9.30` or `ENV UV_VERSION=0.9.30` assignment.
  Renovate comments (`# renovate: datasource=pypi depName=uv`) work too. Inline
  annotations, multiline assignments, variable values, and multiple assignments
  on one line are refused. `FROM` tags remain owned by the Docker updater
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
- Dockerfiles and GitHub Actions workflows keep their own updaters and are
  scanned for annotations as well. Dockerfiles require preceding comments as
  described above; workflow `with:` inputs use trailing comments. See
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
