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
that requires `>=3.11`. Project upper bounds, exclusions, compatible-release
constraints, and Poetry caret, tilde, wildcard, and union constraints are
supported. When both declarations exist, their intersection defines the resolver range.
Requirements files inherit the nearest enclosing `pyproject.toml`, stopping at
the repository boundary. Without a declaration, selection is unchanged; the
installed interpreter and `.python-version` do not override project metadata.

Compatibility checks read per-file `Requires-Python` from Simple JSON, Simple
HTML, or the legacy PyPI JSON API, including private indexes. Missing metadata
is treated as unrestricted; files with invalid metadata cannot establish
compatibility. Dependency upper bounds are ignored so that a cap such as `<4`
does not reject a release for a project declaring `>=3.12` or cause a fallback
to an older release without that cap. Dependency minimum versions and explicit
exclusions are still checked; project constraints and dependency markers retain
their upper bounds. An empty compatible set reports an error and leaves that
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
- Queries Docker Hub and OCI Distribution-compatible registries. Reuses
  `docker login` credentials, configured credential helpers, and identity tokens
  for private images. GitHub Actions' repository token with `packages: read`
  remains a fallback for private GHCR images. Docker Hub lookups fall back to its
  OCI registry when the richer tag endpoint is unavailable. See
  [Docker authentication](private-registries.md#docker--oci-registries)
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
the replacement manifest digest. Digest-pin updates are unsupported in this
release: they are reported as `not-examined`, are left unchanged without registry
verification, and do not fail `--check --fail-on-blocked`.

## Linked release checksums

Versions annotated with `github-releases` can have linked SHA-256 values in
every file scanned by the annotation updater: Makefiles, shell scripts, YAML,
GitHub Actions workflows, Dockerfiles, and other explicitly selected text files.
The link names the version's assignment key or explicit ID and the exact
release asset. It does not guess relationships from a checksum's name or proximity.

```sh
MISE_VERSION=2025.12.9 # upd: github-releases jdx/mise
MISE_CHECKSUM=afe7e9f2ea8e1704e9cc41e4b020798b8c60e5924ab4a313ccbf201a062f54d0 # upd: checksum MISE_VERSION asset=mise-v{version}-linux-x64.tar.gz
```

Outside Dockerfiles, inline comments (`#` or `//`) and standalone comments
immediately before the value line both work. A blank line, another comment,
or another annotation between the directive and its value is refused. The
checksum line must contain exactly one complete, 64-digit hexadecimal SHA-256
token before its comment. Quotes, punctuation, `sha256:` prefixes, indentation,
line endings, and a missing final newline are preserved. Hashes in comments,
multiple hash tokens, and substrings of longer values are never selected.

Simple assignment keys such as `MISE_VERSION=...`, `MISE_VERSION ?= ...`,
`MISE_VERSION = ...`, `MISE_VERSION: ...`, and `const MISE_VERSION = ...` can be
referenced directly. This is text matching rather than language evaluation:
an inferred key must be unique in the file. For repeated YAML/TOML keys or a
version on another kind of line, give the version annotation a unique,
case-sensitive `id=<name>` and reference that ID:

```toml
[mise]
version = "2025.12.9" # upd: github-releases jdx/mise id=mise
sha256 = "afe7e9f2ea8e1704e9cc41e4b020798b8c60e5924ab4a313ccbf201a062f54d0" # upd: checksum mise asset=mise-{tag}-linux-x64.tar.gz
```

IDs take precedence over inferred keys. An ID starts with an ASCII letter or
underscore and contains only ASCII letters, digits, and underscores. Generic
links are file-local and independent of declaration order; duplicate IDs and
ambiguous keys block their groups. Existing annotation discovery rules still
apply: pass a custom text file explicitly or add an `include` glob for directory
walks. Recognized manifests retain their own parser; adding a checksum does not
opt them into annotation support. GitHub workflow annotations on native `uses:`
references remain refused; tool versions under `env:` or `with:` can be linked.

Dockerfiles keep their instruction and stage-scope rules. Use separate comments
immediately above literal, single-line `ARG` or `ENV` assignments:

```dockerfile
# upd: github-releases jdx/mise
ARG MISE_VERSION=2025.12.9

# upd: checksum MISE_VERSION asset=mise-v{version}-linux-x64.tar.gz
ARG MISE_CHECKSUM=afe7e9f2ea8e1704e9cc41e4b020798b8c60e5924ab4a313ccbf201a062f54d0

RUN curl -fsSL "https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-x64.tar.gz" -o mise.tar.gz \
    && echo "${MISE_CHECKSUM}  mise.tar.gz" | sha256sum -c -
```

Run `upd <file>` to preview, or `upd <file> --apply` to write. Interactive
approval selects the version together with all its linked checksum edits; a
file changed since preview must be rescanned before those edits can be applied.
Batch apply also checks the file again after release lookups and refuses to
overwrite changes made while those requests were in flight.

In a Dockerfile, a checksum reference must name an annotated
`github-releases` version visible before the checksum assignment in its Docker
stage. Names may be reused in independent stages. Global `ARG` defaults require
consumption with `ARG NAME` inside a stage; child stages inherit variables from
the stage named by `FROM`, following [Docker's scope rules](https://docs.docker.com/reference/dockerfile/#scope).
Multiple declarations of a linked variable within one stage are ambiguous and
blocked. Quoted values, multiline instructions, and heredoc contents are handled
as instruction arguments rather than separate variable declarations.
An explicit ID in a Dockerfile also follows the linked variable's stage scope.
Other registries and checksum algorithms are not supported yet.

- `{version}` expands to the value written in the version assignment, preserving
  its leading `v` if present. `{tag}` expands to the actual release tag.
  For an unprefixed `2025.12.9` assignment and tag `v2025.12.9`, use
  `mise-v{version}-linux-x64.tar.gz` or `mise-{tag}-linux-x64.tar.gz`.
- Templates are release asset filenames. Unknown placeholders, paths, URLs,
  unknown options, and duplicate options are refused. No commands or shell
  expressions are evaluated.
- By default, the checksum comes from the exact asset's GitHub `sha256:` digest.
  If the release does not expose that digest, the group is blocked with a hint
  to specify a checksum manifest. The updater does not download the binary to
  invent a replacement checksum.
- Add `checksums=SHASUMS256.txt` to the annotation to select a release manifest
  explicitly. GNU (`hash  filename`, including binary `*filename`) and BSD
  (`SHA256 (filename) = hash`) formats are supported. Leading `./` is accepted;
  the full asset filename must match exactly once. A bare hash is accepted only
  when the selected sidecar is named `<asset>.sha256`. Missing entries, duplicate
  entries and malformed hashes block the whole group. Missing assets (including
  HTTP 404) are safety refusals; other unsuccessful HTTP responses, transport
  failures and response-body read failures are request errors (exit 2). Either
  outcome leaves the whole group unchanged. The
  referenced archive must also exist as a unique, uploaded release asset. If
  GitHub provides its SHA-256 digest, the manifest must agree with it.
  The checksum manifest itself must be uploaded, and its downloaded bytes must
  match its own GitHub SHA-256 digest when available. Concurrent files share
  one in-memory metadata and manifest snapshot for the run. Failed downloads
  are not cached, so a later lookup can retry.
- Multiple checksum annotations may reference the same version key or ID, for
  example one per architecture. Every checksum is resolved before any member of
  that group changes. Unrelated dependency groups can still update.
- Version pins, ignore rules, package selection, cooldown, prerelease tracks,
  and bump ceilings apply to the version and its checksums together.
  `--lang annotated` and `--lang github-releases` select these groups;
  `--lang docker` selects image references. A checksum-linked version must name
  the complete release; use `--full-precision` when starting from a shortened pin.
- An unchanged version with a different recorded checksum is reported as blocked;
  its hash is never silently repaired. Checksum manifests and GitHub digests
  provide integrity metadata; fetching them does not verify a publisher signature.

JSON reports include `checksum_updates` with the version line, checksum line,
asset, release tag, checksum source, and old/new SHA-256. Each entry's `change`
is `changed` or `verified_unchanged`: the latter is a companion whose hash
matches the new release. It stays in the report and the atomic approval plan,
and text output labels it as verified unchanged. `change` describes the hash
comparison; the existing optional `status` records a failed or rolled-back
write. Blocked groups appear
in `skipped` with `checksum-invalid`, `checksum-unavailable`, or
`checksum-mismatch` reasons. A malformed directive whose intended relationship
cannot be established, including an unknown version variable, conservatively
blocks GitHub release version updates in that file until the annotation is fixed.

## Annotation tools

Validate syntax, version tokens, assignment/ID links, SHA-256 token shape, asset
templates, and Docker stage visibility before running an update:

```sh
upd annotations validate Dockerfile versions.sh pins.toml
upd annotations validate . --output json
```

Validation shares the update parser and checksum-link resolver. It uses normal
file discovery, gitignore, and config `include`/`exclude` rules; an explicit file
bypasses discovery exclusions. With no paths, it checks the nearest Git root.
Update selection policies such as ignored packages and cooldowns do not hide
structural errors. Recognized manifests that use only their native parser are
reported as unsupported if they contain annotations. GitHub Actions `uses:`
collisions and Docker's inline directives are refused as they are during updates.

Exit status is 0 for valid input and 2 for validation or I/O errors. Text mode
prints file-and-line diagnostics on stderr; JSON includes `valid`, `files` with
`diagnostics` (`lines`, `message`), and a `summary` of files, versions, checksums,
and errors. This checks local structure only: it does not establish that a
release asset exists or that a recorded checksum matches published metadata.

Generate a snippet from an exact GitHub release asset URL and its published SHA:

```sh
upd annotations init \
  https://github.com/jdx/mise/releases/download/v2025.12.9/mise-v2025.12.9-linux-x64.tar.gz \
  --checksum afe7e9f2ea8e1704e9cc41e4b020798b8c60e5924ab4a313ccbf201a062f54d0 \
  --syntax docker --output text
```

The default syntax is `shell`; `docker`, `toml`, `yaml`, and `javascript` are
also supported. The variable prefix defaults to the repository name in
uppercase; override it with `--name MISE`. Add `--checksums SHASUMS256.txt` when
the publisher provides a manifest. The generator replaces tag/version tokens
in the asset name with `{tag}`/`{version}` and preserves a fixed asset name.
Inspect that template before using it with a publisher whose naming varies
between releases. Override inference with `--asset-template 'mise-{tag}-linux-x64.tar.gz'`.
An override can be a fixed filename or use `{version}` and `{tag}`, but must
expand exactly to the URL's asset name. Paths, URLs, unknown placeholders,
whitespace, and mismatching filenames are refused before network access.
Every generated snippet passes the same structural validator.

For online generation, replace `--checksum <sha>` with `--resolve-checksum`:

```sh
upd annotations init \
  https://github.com/jdx/mise/releases/download/v2025.12.9/mise-v2025.12.9-linux-x64.tar.gz \
  --resolve-checksum --asset-template 'mise-{tag}-linux-x64.tar.gz' \
  --syntax docker --output text
```

Exactly one checksum mode is required. Online mode queries only the tag and
asset named in the URL, uses GitHub's published SHA-256, and never downloads
the binary or looks up the latest release. When a release has no GitHub digest,
explicitly select its published manifest with `--checksums <filename-or-template>`.
The manifest must be a different release asset from the binary being pinned.
No manifest name is guessed, and an explicitly selected manifest must agree
with any published archive digest. Archive presence, upload state, unique
manifest entries, and the manifest's own digest (when published) are verified
using the same resolver as updates. A failed resolution prints no snippet;
it never falls back to an unverified hash. Online mode uses the existing GitHub
authentication and TLS settings.

Both commands never write files, even with `--apply`. Init does not load
configuration. `--checksum` stays offline and does not verify the supplied hash;
validation remains offline. JSON generation output includes `checksum_source`:
`supplied`, `github-asset-digest`, or the selected manifest's expanded filename.
Piped output defaults to JSON; select `--output text` for a pasteable snippet.
Snippets belong in annotation-capable files; generating TOML does not enable
annotations inside native manifests such as `Cargo.toml`.

## Terraform / OpenTofu

- `.tf` files (HCL format)
- Updates `required_providers` version constraints and `module` version declarations
- Queries the Terraform Registry API (`registry.terraform.io`)
- Skips local modules (`./`, `../`) and git sources
- Supports pessimistic constraints (`~> 5.0`)

## Nix flakes

- `flake.lock` (lock format version 7, as written by current Nix)
- Moves each direct `github:` and `gitlab:` input to the commit its branch or
  tag points at today. `flake.nix` is never edited: it keeps naming the branch,
  and only the locked commit changes
- Reports each change with bump `revision` and short commit hashes
  (`nixpkgs 00455b0a3690 -> 4975466d3247`). A revision has no semver level, so
  `--max-bump` and `--only-bump` never hold it back. `--strict-bump` does: it
  reports the revision in `capped` with reason `strict-bump` and leaves the
  lock as it is
- Reads upstream heads straight from the GitHub and GitLab APIs, so checking
  needs no Nix installation. `GITHUB_TOKEN` (or `GH_TOKEN`) raises the GitHub
  rate limit and reaches private repositories, as for GitHub Actions. For a
  private GitLab project set `GITLAB_TOKEN`, plus `GITLAB_HOST` when it is not
  on gitlab.com; the token is only ever sent to that one host
- Writing needs Nix: the lock records a hash of the fetched source that only
  Nix computes. `--apply` runs `nix flake update <input>...` for the inputs
  that moved, then checks the lock holds exactly the commits `upd` resolved and
  that nothing else changed: inputs of an updated input may move with it, but
  every other input, direct or transitive, must be locked exactly as before,
  and no new input may appear. Anything else, a missing `nix` included, is an
  error and the lock is put back as it was
- Inputs that `follows` another input move with it and are not listed. Inputs
  pinned to a commit in `flake.nix` (`?rev=`) count as up to date. Registry
  (`nixpkgs` without a URL), `git+https:`, tarball, and GitHub Enterprise
  inputs are left alone and reported as not examined
- `ignore` and `--package` work by input name. `[pin]` does not apply: name
  the commit in `flake.nix` instead

Cooldown works differently here, because a branch has no releases to age.
With a cooldown of 7 days, an input moves only once its locked commit is at
least 7 days old, and then to the newest commit. That limits how often the
lock changes without ever holding it on a commit nobody picked. Set it per
ecosystem with `nix = "7d"` under `[cooldown.ecosystem]`.

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

- `.pre-commit-config.yaml` and `prek.toml`
- Both `--lang pre-commit` and `--lang prek` select both formats
- Updates `rev` fields for GitHub-hosted hook repositories; special repositories
  (`local`, `meta`, `builtin`) and non-GitHub repository revisions stay unchanged

A `rev` is a git reference, not a version, so only one upd can read as a version
tag is rewritten. `v4.5.0`, `24.3.0`, four-segment tags such as `v0.11.0.1`,
prereleases, and single-number tags all qualify. Anything else is left exactly as
it is, with the status and reason naming which kind it was:

| Status | Reason | Revisions |
| --- | --- | --- |
| `not-examined` | `sha-pinned-rev` | a full 40-character commit SHA, including `pre-commit autoupdate --freeze` pins |
| `blocked` | `unrecognized-rev` | an abbreviated SHA, a branch (`main`), a moving pointer (`1.x`), a prefixed tag (`black-24.3.0`) |

Neither costs a revision lookup. Frozen commit-pin updates are unsupported and
do not fail `--check --fail-on-blocked`; an unrecognized revision fails this
strict check because no safe version-tag rewrite is available. Plain `--check`
does not fail for either revision alone. A configured `[pin]` does not override
this: the pinned version
is written in the shape of the revision it replaces, which for an unreadable
revision is the truncation this guard exists to prevent. An abbreviated commit
SHA made only
of decimal digits is indistinguishable from a numeric tag such as the CalVer
`20250101`, and is read as the tag.

Hook `additional_dependencies` are updated according to the hook's language,
including `repo: local` hooks:

| Language | Registry | Supported versioned install arguments |
| --- | --- | --- |
| `python` | PyPI | PEP 508 requirements, such as `flake8-docstrings==1.6.0` or `demo[extra]>=1.0,<2` |
| `node` | npm | `package@version` and `@scope/package@range` |
| `rust` | crates.io | `crate:version` and `cli:crate:version` |

An explicit hook `language` takes precedence. Otherwise, remote GitHub hooks
get their language from `.pre-commit-hooks.yaml` at the revision selected for
that update (or the existing revision when it stays unchanged). This uses the
GitHub API and the existing `GITHUB_TOKEN` / `GH_TOKEN` authentication; it does
not install or execute hooks. A missing or unsupported language produces a
warning and leaves that hook's additional dependencies unchanged.

Dependency updates reuse the corresponding ecosystem's constraints, package
filters, ignores, pins, bump limits, precision, and cooldown rules. Hook Python
environments do not inherit the enclosing project's Python or uv settings.
Dry-run, `--check`, `--apply`, and interactive selection work for both formats.
When an inherited language was resolved at a proposed new repository revision,
interactive selection must include that revision along with its dependency updates.
Comments, quoting, line endings, and unrelated content are preserved.

Hook revisions resolve through GitHub releases, which also answers for Actions
pins and mise tools, so `[cooldown.ecosystem]` takes `pre-commit` as a key of
its own and it outranks the shared `github-releases` key. A hook repository that
tags its versions but publishes no releases is dated from those tags, so it is
held to the cooldown like any other. Both are described in
[Configuration](configuration.md#cooldown-minimum-release-age).

Unpinned arguments, direct URLs and Git references, installer switches,
parenthesized Python requirements, and unsupported dependency formats remain
unchanged. Shared YAML anchors/aliases, merge mappings, escaped strings, and
multiline scalar spellings are not rewritten. Additional dependencies are not
included in version alignment, which continues to align repository revisions.

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
own source with an inline comment or a comment immediately before its value:

```makefile
BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli
NODE_VERSION := 22.11.0  # upd: npm node
```

- Syntax: `upd: <source> <package> [id=<name>]` in a `#` or `//` comment
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
  described above; workflow `with:` inputs accept inline or preceding comments. See
  [GitHub Actions](github-actions.md#annotated-versions-in-a-workflow)
- The version on the line is found and rewritten in place, keeping a leading
  `v` and the line's own precision (`v2.60` becomes `v2.65`, not `v2.65.4`)
- One package name may not appear under two different sources in the same file
- Annotated GitHub releases can link SHA-256 values with
  `upd: checksum <key-or-id> asset=<filename>`; see
  [linked release checksums](#linked-release-checksums)
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
