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

## Python

- `requirements.txt`, `requirements-dev.txt`, `requirements-*.txt`
- `requirements.in`, `requirements-dev.in`, `requirements-*.in`
- `dev-requirements.txt`, `*-requirements.txt`, `*_requirements.txt`
- `pyproject.toml` (PEP 621 and Poetry formats)

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
- Queries Docker Hub and public OCI Distribution-compatible registries. Anonymous
  bearer-token challenges are handled automatically, and Docker Hub lookups fall
  back to its OCI registry when the richer tag endpoint is unavailable
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

- `.mise.toml` (`[tools]` section)
- `.tool-versions` (space-delimited format)
- Supports 24+ common dev tools: node, python, go, rust, zig, deno, bun, uv, ruff, terraform, kubectl, helm, and more
- Skips `latest` versions and `cargo:*` tools

## Annotated files

A version pinned in a file `upd` does not otherwise understand can declare its
own source with a trailing comment:

```makefile
BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli
NODE_VERSION := 22.11.0  # upd: npm node
```

- Syntax: `upd: <source> <package>` in a trailing `#` or `//` comment
- Sources: `pypi`, `npm`, `crates`, `go`, `rubygems`, `nuget`, `github-releases`
- Scanned by name: `Makefile`, `makefile`, `GNUmakefile`, `*.mk`, `justfile`,
  `Justfile`, `*.sh`, `*.bash`. Any other file works when passed explicitly:
  `upd update path/to/versions.env`
- The top-level `include` config key adds otherwise-unknown files to directory
  discovery as annotated files. Patterns are relative to the scanned directory:
  `include = ["ansible/roles/*/vars/*.yml", "docker-compose.yml"]`
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
