# Version behavior and annotations

[← upd overview](../README.md) · [Documentation index](README.md)

## Version Constraints

`upd` respects version constraints in your dependency files:

| Constraint | Behavior |
|------------|----------|
| `>=2.0,<3` | Updates within 2.x range only |
| `^2.0.0` | A complete caret version updates within 2.x only (npm/Cargo) |
| `~2.0.0` | Updates within 2.0.x range (npm); `~2.0.0` (Cargo) stays within 2.0.x |
| `~> 7.1` | Updates within 7.x range (Ruby pessimistic) |
| `>=2.0` | Updates to any version >= 2.0 |
| `==2.0.0` | Updates the exact pin to the latest version (e.g. `==2.0.0` → `==3.1.5`). To freeze a package, use `[pin]` or `ignore` in `.updrc.toml`. |
| `==2.0.*` | Never rewritten. A PEP 440 prefix match names a series, not a release. See [Bounds that are not floors](#bounds-that-are-not-floors). |

An update moves the **lower bound** and leaves every other clause where the
author wrote it, so `>=1.0, <2.0` becomes `>=1.5.0, <2.0`. A constraint is an
unordered set of clauses, so the lower bound is found wherever it sits
(`<2.0, >=1.0` answers alike), and an upper bound is honored when picking the
new version: the release chosen is the newest one the constraint already admits.

Complete caret and tilde versions retain their implied bounds. A partial npm
range such as `^1.2` or `1.2.x` instead follows newer releases, even across a
major boundary, while retaining its operator and number of components.

npm ranges keep the shape they were written in. A comparator range
(`">=1.0.0 <2.0.0"`) and a hyphen range (`"4.17.0 - 4.18.0"`) each keep their
ceiling. A wildcard or partial range is rewritten to the newest release in the same
shape:
`"4.3.x"` becomes `"4.4.x"` and `"^1.2"` becomes `"^3.1"`, never a fully
written version. npm lets a comparator stand apart from the version it applies
to, and that spacing is part of the shape: `">= 1.2.7 < 1.3.0"` is read as the
range it is and comes back spaced the same way. npm's tilde has two spellings
and `"~>1.2.3"` means what `"~1.2.3"` does, ceiling included; each comes back
spelled the way it was written.

An npm spec that names no published version is left alone and reported nowhere:
`"*"`, a dist-tag (`"latest"`, `"next"`, `"beta"`), and the `workspace:`,
`file:`, `link:`, `npm:`, `git+ssh:` and `github:owner/repo` forms all resolve
somewhere other than a release on the registry, so there is no version to
compare and nothing an update could move.

Python bump labels and `--only-bump` / `--max-bump` follow the changed release
component: `0.0.77` → `0.0.78` is patch, and `0.77` → `0.78` is minor.
Other ecosystems retain their existing pre-1.0 compatibility classification.
Python update reports include each declaration's section and full constraint.

Specifier shape changes are opt-in. A `[normalize.pyproject]` table in
`.updrc.toml` can independently make project dependencies, optional
dependencies, or standardized dependency groups use `==`, `>=`, or `<=`; projects without
that table retain the shape-preserving behavior above. See
[the configuration guide](configuration.md#normalizing-pyproject-specifiers).

### Bounds that are not floors

Only an **inclusive** lower bound names the version a project is on, so only
that bound is raised. `>1.2.3` names the one version its author refuses, `<3`
and `<=3` are ceilings, `!= 1.5` is an exclusion, and an OR range
(`"^1 || ^2"`) has no single branch to edit. None of them is a floor, so none
of them is moved. They are checked against the registry and reported anyway:

| Outcome | Reported as |
|---------|-------------|
| The constraint admits the newest release | Up to date |
| The newest release has outgrown it | A warning naming the release and the constraint |
| The spec cannot be read at all | An error, exit `2` |

The last row is the point of the other two: a dependency nothing looked at must
not be counted as up to date, and a constraint that has quietly frozen a
dependency should say so rather than pass under a green tick.

A PEP 440 prefix match reads the same way for the same reason. `==6.*` and
`==6.0.*` name a series rather than a release, so there is no version in them to
raise and nothing an update may write over them; they are checked and reported
by the table above. `pyyaml==6.*` with 6.0.3 published is up to date, and
`rich==13.7.*` with 15.0.0 published is a warning. `[pin]` and `upd align`
answer the same way: neither writes a release into a specifier that has nowhere
to hold one, and the pin reports an error rather than a rewrite.

## Annotated Version Pins

Files without a dependency-manifest format can carry an inline annotation or
one immediately above its value:

```yaml
shinyhub_version: "0.11.16"  # upd: pypi shinyhub
```

Directory walks scan annotations in otherwise-unrecognized `Makefile`,
`makefile`, `GNUmakefile`, `justfile`, `Justfile`, `*.mk`, `*.sh`, `*.bash`,
`*.yml`, and `*.yaml` files. Any file passed explicitly is scanned as annotated.
To add other files to normal repository discovery, use repository-relative globs
in `.updrc.toml`:

```toml
include = ["deploy/*.env", "config/version.conf"]
exclude = ["**/archive/**"] # exclude wins over include
```

An include never changes a recognized manifest's parser: for example, a
matching `main.tf` remains Terraform. Use `--verbose` to diagnose an `upd:`
marker in an otherwise undiscovered UTF-8 text file up to 1 MiB.

Dockerfiles also scan annotated `ARG` and `ENV` versions alongside `FROM` updates:

```dockerfile
# upd: pypi uv
ARG UV_VERSION=0.9.30
```

Use `upd --apply` to update both image tags and annotated versions.
`--lang annotated` selects annotations only; `--lang python` selects PyPI
annotations, and `--lang docker` selects image tags only. Place the comment
immediately above a single-line `ARG` or `ENV` assignment. Docker treats inline
`#` text as instruction arguments, so inline annotations are refused.
Renovate comments such as `# renovate: datasource=pypi depName=uv` work too.

Downloaded GitHub release binaries can keep their version and SHA-256 together
in any file scanned for annotations:

```sh
MISE_VERSION=2025.12.9 # upd: github-releases jdx/mise
MISE_CHECKSUM=afe7e9f2ea8e1704e9cc41e4b020798b8c60e5924ab4a313ccbf201a062f54d0 # upd: checksum MISE_VERSION asset=mise-v{version}-linux-x64.tar.gz
```

This works in Makefiles, shell scripts, YAML, GitHub workflows, and other
explicitly selected text files. Add `id=<name>` to the version annotation when
the version has no simple assignment key or the file repeats that key. The
checksum references that ID. Comments may also immediately precede the value
line; Dockerfiles require that form:

```dockerfile
# upd: github-releases jdx/mise
ARG MISE_VERSION=2025.12.9

# upd: checksum MISE_VERSION asset=mise-v{version}-linux-x64.tar.gz
ARG MISE_CHECKSUM=afe7e9f2ea8e1704e9cc41e4b020798b8c60e5924ab4a313ccbf201a062f54d0
```

`upd` resolves the exact release asset's GitHub SHA-256 digest before updating
both assignments. Add `checksums=SHASUMS256.txt` to explicitly use a release
checksum manifest instead. If any linked checksum cannot be resolved, the
entire version/checksum group stays unchanged. See
[linked release checksums](ecosystems.md#linked-release-checksums) for
multiple architectures, templates, and diagnostics.

Check annotation syntax and checksum links without accessing any registry:

```sh
upd annotations validate Dockerfile versions.sh
```

`upd annotations init <release-asset-url> --checksum <published-sha256>` prints
a linked snippet. Choose `--syntax docker`, `shell`, `toml`, `yaml`, or
`javascript`; use `--output text` for plain text when piping it. Replace
`--checksum` with `--resolve-checksum` to fetch the published digest online,
optionally selecting a manifest with `--checksums`. Use `--asset-template` to
override filename inference; it must expand exactly to the URL asset. Neither
command writes files. See
[annotation tools](ecosystems.md#annotation-tools) for examples.

A GitHub Actions workflow keeps its Actions updater and is
scanned for annotations as well, so a tool version passed to an action through a
`with:` input can be updated beside the `uses:` refs around it. See
[GitHub Actions](github-actions.md#annotated-versions-in-a-workflow).

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
