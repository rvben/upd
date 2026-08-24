# GitHub dependency pull requests

`upd` publishes a reusable GitHub Actions workflow that maintains one rolling,
policy-constrained dependency pull request. It can update every ecosystem
supported by `upd`; its backward-compatible default updates GitHub Actions only.

Every run rebuilds the automation branch from the latest default branch. The
proposal therefore stays current and contains one generated commit.

## Quick start

Create `.github/workflows/upd.yml` in the consuming repository:

```yaml
name: Weekly dependency health

on:
  schedule:
    - cron: "17 6 * * 2"
  workflow_dispatch:

permissions:
  contents: write
  pull-requests: write

jobs:
  update:
    uses: rvben/upd/.github/workflows/dependency-health.yml@<FULL_COMMIT_SHA>
    with:
      upd-version: v0.6.3
      min-age: 7d
      max-bump: minor
      validation-command: make test
    secrets:
      pull-request-token: ${{ secrets.UPD_PR_TOKEN }}
```

Replace `<FULL_COMMIT_SHA>` with a revision containing the reusable workflow.
Pinning the workflow prevents later changes in this repository from silently
changing executable CI code. The workflow itself pins its third-party actions,
the default `upd` archive version, and that archive's SHA-256.

The `pull-request-token` secret is optional. Without it, the workflow uses the
caller's `GITHUB_TOKEN`. The caller must grant `contents: write` and
`pull-requests: write`; a reusable workflow cannot elevate permissions withheld
by its caller.

Use a narrowly scoped GitHub App installation token or fine-grained personal
access token when pull-request checks should start without manual approval. Give
it repository Contents and Pull requests write permissions, plus permission to
modify workflow files when `langs` includes `actions`. Pull requests created
with `GITHUB_TOKEN` can require a maintainer to approve their workflow runs, and
push events created by that token do not recursively start workflows.

## General dependency updates

The default `langs: actions` preserves the original integration's behavior. To
update all detected manifests, pass an empty language filter and select a Linux
runner with the project's toolchains:

```yaml
jobs:
  update:
    uses: rvben/upd/.github/workflows/dependency-health.yml@<FULL_COMMIT_SHA>
    with:
      runner: ubuntu-24.04
      langs: ""
      lock: true
      prepare-command: corepack enable
      validation-command: make test
    secrets:
      pull-request-token: ${{ secrets.UPD_PR_TOKEN }}
```

Lockfile regeneration requires the ecosystem tools used by the repository.
Choose an appropriate GitHub-hosted or self-hosted Linux runner, or install tools
with `prepare-command`. Preparation may initialize tools and caches but must
leave the repository clean; dependency changes belong exclusively to `upd`.

## Inputs

| Input | Default | Purpose |
|-------|---------|---------|
| `runner` | `ubuntu-24.04` | Linux runner label |
| `upd-version` | `v0.6.3` | Exact released `upd` version |
| `upd-sha256` | built in for the default version | Exact archive checksum when changing version or target |
| `upd-target` | detected | Release target; Linux x86-64 and ARM64 GNU are detected |
| `paths` | `.` | Whitespace-separated repository paths passed to `upd` |
| `langs` | `actions` | Comma-separated ecosystem filter; empty enables every detected ecosystem |
| `packages` | empty | Comma-separated package filter |
| `min-age` | `7d` | Minimum eligible release age; empty uses project configuration |
| `max-bump` | `minor` | Highest applied bump; empty uses project configuration |
| `lock` | `false` | Regenerate lockfiles using tools available on the runner |
| `update-action-shas` | `true` | Verify and update immutable Action SHA pins |
| `prepare-command` | empty | Prepare project tooling without changing repository files |
| `validation-command` | empty | Check updates before publishing |
| `validate-actions` | `true` | Run `actionlint` when workflow files change |
| `fail-on-blocked` | `false` | Fail when a safety condition blocks an update |
| `branch` | `automation/upd-github-actions` | Automation-owned rolling branch |
| `commit-message` | `ci(deps): update dependencies with upd` | Generated commit message |
| `pull-request-title` | `ci(deps): update dependencies with upd` | Pull-request title |
| `auto-merge` | `false` | Ask GitHub to merge after repository checks pass |
| `merge-method` | `squash` | Auto-merge strategy: `squash`, `merge`, or `rebase` |

When changing `upd-version` or `upd-target`, also provide the published archive
digest:

```yaml
with:
  upd-version: v0.6.3
  upd-target: x86_64-unknown-linux-gnu
  upd-sha256: 0e28562f06c852a5438e4e9745c8f99fc31240162ee65e633eeb7359b7e3a351
```

GitHub-hosted runner images are maintained over time rather than immutable. Use
a controlled self-hosted runner when the complete execution environment must be
reproducible. The downloaded `upd` binary remains independently checksum-pinned
in either case.

## Immutable GitHub Action pins

The Actions updater covers actions and reusable workflows in
`.github/workflows/*.yml` and `*.yaml`. It skips branch refs, local actions, and
Docker references. Full commit pins are checked by default, and rewriting one
requires a concrete version annotation:

```yaml
- uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2

jobs:
  conformance:
    uses: rvben/clispec/.github/workflows/conformance.yml@<full-commit-sha> # v0.3.0
```

Before writing, `upd` verifies that the annotated current tag resolves to the
pinned commit. It then applies cooldown and bump policy, resolves the selected
tag to its full commit SHA, and updates both SHA and annotation. A pin is never
converted to a mutable tag.

A short SHA, missing or floating annotation, moved tag, stale annotation, or
non-concrete configured target is reported as `blocked` with a machine-readable
reason. With `update-action-shas: false`, these pins are instead reported as
`not-examined`. Set `fail-on-blocked: true` when every immutable pin is expected
to be maintainable automatically.

`max-bump: minor` is a strict ceiling. Bare major references such as `@v4` are
therefore normally held back; use `max-bump: major` when those updates should be
eligible. Changed workflows are validated with `actionlint` before publication.

Configuration pins and package filters use an action's `owner/repo` name. For
example, `packages: actions/checkout` selects checkout references, subdirectory
actions, and reusable workflows from that repository.

## Safety and lifecycle

The reusable workflow:

- installs an exact `upd` release after verifying a trusted SHA-256;
- serializes runs per repository;
- validates the automation ref and keeps it distinct from the default branch;
- starts from the latest default branch on every run;
- updates or deletes the branch with an explicit `--force-with-lease` expectation;
- refuses ambiguous duplicate open pull requests;
- refuses partial results when `upd` reports errors;
- fails if preparation or validation leaves unexpected repository changes;
- retains the JSON update report for seven days;
- creates or updates the automation-owned title and detailed description; and
- closes the obsolete pull request and deletes its branch when no eligible
  updates remain.

Treat the configured branch, generated commit, title, and description as
automation-owned. Later successful runs replace them.

The reusable workflow exposes `changed` and `pull-request-url` outputs for caller
jobs.

## Auto-merge

Auto-merge is deliberately opt-in:

```yaml
with:
  validation-command: make test
  auto-merge: true
  merge-method: squash
```

The workflow gives GitHub the exact generated commit SHA through
`--match-head-commit`. GitHub still enforces required checks, approvals,
conversations, branch protection, rulesets, and merge queues. The workflow never
uses administrator bypass. Turning `auto-merge` off disables auto-merge if this
workflow previously enabled it.

The repository must allow the chosen merge method and have auto-merge enabled.

## Migrating off Dependabot

For an existing fleet, keep overlapping Dependabot updates enabled for four
successful weekly `upd` cycles. Review the generated reports, resolve blocked
legacy pins, and confirm that the chosen token starts the expected checks. Then
remove only the overlapping ecosystems from Dependabot.

## Scope

This integration intentionally produces one policy-constrained rolling pull
request. It does not provide Renovate-style per-package branches, dependency
dashboards, reviewer assignment, conflict resolution, or automatic rebasing.

See [Configuration](configuration.md) for repository policy and
[Private registries](private-registries.md) for ecosystem credentials.
