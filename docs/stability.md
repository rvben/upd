# Stability

Starting with `0.1.0`, `upd` commits to the following public surfaces.
Anything listed here will not change in a backwards-incompatible way
without a major-version bump.

## Stable CLI

Global flags (accepted on every subcommand):

| Flag | Short | Purpose |
|------|-------|---------|
| `--apply` | | Write changes to files (omit for dry-run preview) |
| `--dry-run` | `-n` | Preview changes without writing (explicit form) |
| `--verbose` | `-v` | Verbose output |
| `--quiet` | `-q` | Suppress decorative output (errors still shown) |
| `--interactive` | `-i` | Approve each update individually |
| `--check` | | Make `align` exit 1 if misalignments are found (`update` and `audit` already exit non-zero; see exit codes) |
| `--only-bump <major\|minor\|patch>` | | Restrict to exactly these bump levels (repeatable, comma-separated) |
| `--max-bump <major\|minor\|patch>` | | Include updates up to and including this level |
| `--package <NAME>` | `-p` | Restrict to package names or case-sensitive globs (quote globs; repeatable, comma-separated) |
| `--lang <LANG>` | `-l` | Filter by ecosystem (repeatable) |
| `--full-precision` | | Output full versions |
| `--no-cache` | | Disable version cache |
| `--no-color` | | Disable colored output |
| `--no-ignore` | | Disable `.gitignore` filtering during discovery |
| `--lock` | | Regenerate lockfiles after updates or security fixes |
| `--config <FILE>` | `-c` | Use a specific config file |
| `--show-config` | | Print effective configuration and exit |
| `--format <text\|json\|sarif>` | | Output format (`sarif` applies to `audit`) |
| `--version` | `-V` | Print version (built-in clap flag) |
| `--help` | `-h` | Print help (built-in clap flag) |

Subcommands: `update` (default), `align`, `audit`, `clean-cache`, `self-update`.

Stable `audit`-specific flags:

| Flag | Purpose |
|------|---------|
| `--fix-audit` | Bump each vulnerable package to minimum safe version |
| `--offline` | Use only cached OSV responses; cache misses are errors |
| `--format sarif` | Emit SARIF 2.1.0 for GitHub Code Scanning |

## Commands run by `--lock`

`upd --lock` runs the narrowest per-ecosystem refresh command that
updates only the packages `upd` just rewrote. Targeted forms are used
wherever the package manager supports them; targeting falls back to
`--lockfile-only` flags where no per-package form exists; otherwise
the manifest-wide refresh command is used. The flag is honored by
`update` (including `--interactive`) and by `audit --fix-audit --apply`.

| Ecosystem | Lockfile                 | Command                                        |
|-----------|--------------------------|------------------------------------------------|
| Python    | `poetry.lock`            | `poetry lock --no-update`                      |
| Python    | `uv.lock`                | `uv lock`                                      |
| Node      | `package-lock.json`      | `npm install --package-lock-only`              |
| Node      | `yarn.lock`              | `yarn install --mode update-lockfile` (Yarn 2+)|
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

## Bump levels

`--only-bump` and `--max-bump` classify a change by comparing the two version
numbers:

| Change | Level |
|--------|-------|
| `1.2.3` -> `2.0.0` | major |
| `1.2.3` -> `1.3.0` | minor |
| `1.2.3` -> `1.2.4` | patch |
| `0.12.1` -> `0.13.0` | **major** |
| `0.0.3` -> `0.0.4` | **major** |
| `0.12.1` -> `0.12.4` | patch |

Below `1.0.0` the compatible range is narrower than the version numbers suggest.
SemVer leaves a zero major version unstable, and Cargo and npm both read `^0.12`
as `>=0.12, <0.13`, so moving from `0.12` to `0.13` breaks callers exactly the
way `1.0` to `2.0` does. `upd` therefore classifies such a step as major, which
holds it behind a `--max-bump minor` ceiling instead of applying it unattended.
The same reasoning goes one digit further down, where `^0.0.3` means
`>=0.0.3, <0.0.4` and every release is breaking.

Where a manifest pins a range rather than an exact version (`^1.2.3`, `>=1.0`,
`~=1.4`), the level is read from the range's lower bound, which is the same
anchor the ceiling compares. A spec and a bare version therefore always report
the level that decided whether the change was let through. A range with several
clauses is a set, not a sequence, so the lower bound is found wherever it was
written: `botocore<1.35.0,>=1.34.0` and `botocore>=1.34.0,<1.35.0` are one
requirement and answer alike.

An update moves that lower bound and leaves every other clause exactly as the
author wrote it, so `>=1.0, <2.0` becomes `>=1.5.0, <2.0`. An upper bound is
also honored when picking the new version: the release chosen is the newest one
the requirement already admits, never one above the cap.

Only an *inclusive* lower bound is that anchor. `>1.2.3` names the one version
its author refuses rather than the one they are on, a ceiling (`<3`, `<=3`) and
an exclusion (`!= 1.5`) name no floor at all, an npm OR range (`^1 || ^2`) has
no single branch to edit, and a NuGet interval (`[12.0.0,14.0.0)`) is not a form
`upd` writes. None of them is raised, and none is reported as an update. Each is
still measured against the registry and lands in one of three places: counted as
up to date when the constraint admits the newest release, a warning in
`files[].warnings` naming the release and the constraint when it does not, or an
error in `files[].errors` when the spec cannot be read. Only the last changes
the exit code, and it does so deliberately: nothing looked at that dependency,
so counting it as up to date would put a green tick over an unchecked answer.

A `[pin]` onto one of these specs is reported the same way. A pin names the
version the spec must resolve to, and a spec with no floor has nowhere to put
it: writing the pinned version over the number such a spec does contain would
raise a ceiling or invert an exclusion rather than pin anything. The pin is
reported in `files[].errors` and the spec is left as its author wrote it.

An update above the ceiling is reported as held back, in `files[].capped` and
`summary.capped`. It is never counted as up to date, and it does not change the
exit code: the ceiling exists to keep such a change out of the gate, so a run
can exit `0` with work waiting in `capped`. This covers lock-only version
floors as well: a transitive package whose newer release sits above the ceiling
is reported against every manifest the floor would have been written to. A
manifest already carrying the floor is one of them, since the floor is not the
only thing the update moves: the constraint needs no rewrite, the lock below it
does, and regenerating that lock is the work the ceiling is holding back.

A floor `upd` cannot write is a separate answer, not a capped one, because no
ceiling is holding it: it is reported in `files[].updates[]` with its own
`status` and an `error` naming what to do instead. Held back and blocked are
decided independently, so a candidate gets the same verdict wherever the ceiling
sits, and only the manifests that would really have taken the floor are
described as waiting on `--max-bump`. Three things can refuse it. The lock may
have no floor mechanism at all (`poetry.lock`), which makes the verdict belong
to the lock rather than to the package: one transitive package resolved by both
a `uv.lock` and a `poetry.lock` is held back for the uv project and
`unfixable` for the poetry one. The manifest may already hold an entry `upd`
will not rewrite, such as a multi-clause `constraint-dependencies` entry or an
npm override in the nested object form, which is `unfixable` too. Or `--no-lock`
may be in force against a `cargo-precise` floor, which mutates nothing but
`Cargo.lock` and so reports `skipped`. An `unfixable` entry is counted in
`summary.unfixable` and a `skipped` one in `summary.skipped_floors`; like
`capped`, none of these is folded into the up-to-date tally and none changes the
exit code.

One floor is written per manifest and package, whatever the lock holds: an
`overrides` or `constraint-dependencies` entry lifts every locked copy of the
package at once. Held back is reported against that same single floor, so a
package locked at several versions produces one `capped` entry per manifest,
carrying the highest locked version, and produces none at all when a copy within
the ceiling is already floored to a version that covers the rest. A refusal
folds the same way, since it is the same floor being refused: a package locked
at several versions produces one `unfixable` or `skipped` entry per manifest,
whichever side of the ceiling it falls. The exception is `cargo-precise`, where
each locked copy is lifted by its own `cargo update --precise` and so is its own
entry.

Sibling projects that lock the same package each answer for themselves. A floor
is written to a project only if that project's own `.updrc` admits the package,
so an `ignore` entry keeps the floor out of that manifest (reported in its
`ignored[]`) without withholding it from a sibling whose config says nothing
about the package. Ignoring is a complete answer: an ignored project is not
reported as held back or as unfixable either, since neither describes a floor
that project asked for.

## Stable exit codes

| Code | Meaning |
|------|---------|
| `0` | Success. No action required, or updates applied cleanly |
| `1` | Pending updates or misalignments found (dry-run / `--check`). Not an error. |
| `2` | An error was reported. A file could not be read/written, a required path does not exist, a lockfile refresh failed, a dependency could not be checked (its constraint could not be read, or its registry lookup did not answer), or `--interactive` was given with no terminal on stdin. Takes precedence over every other code. An interactive session reports these the same way a plain run does, after applying whatever it approved |
| `3` | Network error. A registry was unreachable or timed out |
| `4` | Invalid CLI arguments or an unparseable dependency file / configuration |
| `6` | Vulnerabilities found (`upd audit`). Pass `--no-fail` to force exit 0. |

> The authoritative exit-code contract is emitted by `upd schema` (`outcomes` and
> `errors`). A bare `upd` / `upd audit` already signals these codes; `--check` does
> not change `update`/`audit` exit codes (it gates `align`, which otherwise exits 0).

## Stable output

- **Text output** is designed for humans. Exact wording, colour, and spacing may change between minor versions, so do not parse it.
- **JSON output** (`--format json`) follows an additive schema. New
  fields may appear in minor releases; existing fields will not change
  type, be renamed, or be removed before `1.0`.

## Stable configuration

- `.updrc.toml` / `upd.toml` / `.updrc` discovery order and the `ignore` array + `[pin]` table are stable.
- New top-level keys may be added in minor releases, but will always default to the pre-existing behaviour.

## Not covered by stability guarantees

- Error message wording and verbose/debug log lines.
- Cache file layout on disk (`$UPD_CACHE_DIR/versions.json`).
- The `upd` Rust library crate. Internal types may change between any releases. Depend on the CLI, not the crate.
