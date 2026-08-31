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
  contents: read
  id-token: write
  packages: read
  pull-requests: read

jobs:
  update:
    uses: rvben/upd/.github/workflows/dependency-health.yml@<FULL_COMMIT_SHA> # vX.Y.Z
    with:
      min-age: 7d
      max-bump: minor
      validation-command: make test
```

Replace `<FULL_COMMIT_SHA>` with a revision containing the reusable workflow.
Pinning the workflow prevents later changes in this repository from silently
changing executable CI code. The workflow itself pins its third-party actions,
and its default install resolves the current release from upd's canonical
`release-pins.json` manifest before verifying that archive's SHA-256. To freeze
the binary version as well as the workflow code, set both `upd-version` and
`upd-sha256` explicitly.

Broker access is described under [Credentials](#credentials) below. With the
default `langs: actions`, the only files this workflow changes are the ones
`GITHUB_TOKEN` is forbidden to push.

## Credentials

The workflow uses upd's hosted GitHub App token broker by default and accepts a
fine-grained personal access token as a single-repository alternative. The
caller grants `contents: read`, `packages: read`, `pull-requests: read`, and
`id-token: write`. Package read access lets Docker updates authenticate to
private GHCR images linked to the repository; credentials are sent only to
GHCR's exact HTTPS token endpoint.
Publication uses the independent App installation token or PAT instead, so the
caller does not grant repository write access to commands that run against the
checked-out project.

`GITHUB_TOKEN` is used only for read access while preparing and inspecting a
proposal. It is intentionally not used for publication because two boundaries
are load-bearing here:

- **It may not write `.github/workflows`.** GitHub rejects a push that creates
  or updates a workflow file unless the pushing credential carries the
  `workflows` permission, and `GITHUB_TOKEN` cannot be granted it: the
  `permissions:` block has no key for it. The default `langs: actions` updates
  workflow files and nothing else, so with `GITHUB_TOKEN` such a run does all of
  its work and then fails at the push. The workflow detects this before the push
  and fails with the remedy instead.
- **Its event behavior is not an independent publication signal.** GitHub's
  recursion protection and the repository's approval policy can suppress or
  hold workflows associated with `GITHUB_TOKEN` activity. An App or PAT keeps
  PR publication independent so checks can start according to repository
  policy.

When an eligible update exists but the App is not installed and no PAT is
provided, the workflow may build and validate a local proposal artifact, but it
fails before any external write. This prevents a pull request from arriving
without the checks that decide whether it is safe to merge.

### Hosted GitHub App broker (recommended)

The broker keeps the App private key out of GitHub Actions and signs only inside
managed HSM-backed infrastructure. The isolated publication job exchanges a
short-lived GitHub OIDC token for a repository-scoped, short-lived installation
token. The broker validates the stable repository identity, owner, reusable
workflow revision, event, ref, and requested permissions before minting it.

For a repository using the hosted service:

1. Install the upd GitHub App for the repository.
2. Grant the caller `id-token: write`; this permits OIDC token issuance but does
   not grant repository write access.
3. Pin the reusable workflow to a reviewed full commit SHA.

The hosted endpoint and OIDC audience are constants inside that pinned workflow,
so a consumer repository cannot redirect its identity token through a mutable
Actions variable. No broker URL, App client ID, or private key is configured in
the consuming repository. A self-hosted operator should fork the reusable
workflow and replace both hosted constants in the reviewed fork; the official
workflow deliberately has no runtime endpoint override.

The reusable workflow requests and immediately masks both credentials. Neither
the App key nor an installation token is stored as a caller secret. Invoke the
reusable workflow through a thin caller such as the quick-start example; direct
dispatch is deliberately not part of the reusable workflow's authorization
contract.

### Fine-grained personal access token

A single-repository alternative when an App is not available. Grant Contents
and Pull requests write; add Workflows write only when the selected update scope
can change `.github/workflows`. Pass it as the `pull-request-token` secret. It
is scoped to the account that issued it, expires on a calendar rather than per
run, and must be rotated by hand, which is why the App is preferred for more
than one repository.

```yaml
jobs:
  update:
    uses: rvben/upd/.github/workflows/dependency-health.yml@<FULL_COMMIT_SHA>
    secrets:
      pull-request-token: ${{ secrets.UPD_PR_TOKEN }}
```

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
```

An empty `langs` includes Actions, so this configuration also needs a
credential that may write workflow files.

Lockfile regeneration requires the ecosystem tools used by the repository.
Choose an appropriate GitHub-hosted or self-hosted Linux runner, or install tools
with `prepare-command`. Preparation may initialize tools and caches but must
leave the repository clean; dependency changes belong exclusively to `upd`.

## Security remediation pull requests

The separate `dependency-remediation.yml` reusable workflow turns a complete
OSV audit into a narrowly scoped security proposal. It is intentionally not a
mode of the ordinary updater: security fixes ignore freshness cooldowns and
bump ceilings, use different failure semantics, and keep auto-merge off unless
the caller explicitly opts in after configuring repository protections.

Scheduled remediation is opt-in policy stored with the repository. Add this to
the root `.updrc.toml` on the trusted default branch:

```toml
[automation]
security_remediation = true
```

When the setting is absent or `false`, scheduled runs stop after resolving the
verified UPD release and effective configuration: they do not audit, mint a
token, change a branch, or touch a pull request. An explicit
`workflow_dispatch` remains available for dry runs and one-off publication, so
temporarily pausing the schedule does not remove operator control. Invalid
configuration fails closed.

```yaml
name: Daily dependency security remediation

on:
  schedule:
    - cron: "47 7 * * *"
  workflow_dispatch:

permissions:
  actions: read
  contents: read
  id-token: write

jobs:
  remediate:
    uses: rvben/upd/.github/workflows/dependency-remediation.yml@<FULL_COMMIT_SHA>
    with:
      publish: true
      langs: rust
      allowed-paths: Cargo.toml Cargo.lock
      validation-command: cargo test --locked
```

`allowed-paths` is a required, whitespace-separated list of exact
repository-relative files. It is a publication boundary, not a discovery
filter: `paths` and `langs` decide what `upd` audits, while `allowed-paths`
prevents a package manager, validation command, or malformed proposal from
publishing any other file. Paths containing whitespace are therefore not
supported by this workflow.

The workflow has two jobs separated by an artifact boundary. The first job has
read-only repository access, applies available fixes, regenerates lockfiles,
runs the caller's validation command, and performs a fresh uncached audit of
the proposed tree. The second job independently verifies the patch and only
then requests the short-lived App token used for Git and pull-request operations.
Project code and package managers never run with that token available.

Publishing deliberately has no `GITHUB_TOKEN` or personal-token fallback. A
pull request created with `GITHUB_TOKEN` does not start ordinary
`pull_request` workflows, which would leave a security proposal without the CI
signal it exists to obtain. Broker authorization is therefore required when
`publish: true`; set `publish: false` for a credential-free dry run.

The post-fix audit controls the lifecycle:

| Result | Behavior |
|--------|----------|
| Complete and clean, with changes | Create or refresh the one-commit rolling security PR |
| Complete and clean, without changes | Close and lease-delete only the obsolete remediation PR and branch |
| Complete with residual findings and safe changes | Publish the partial fix, list residual findings, then fail visibly |
| Residual findings without a safe change | Preserve remote state and fail |
| Incomplete audit, invalid report, unexpected path, or failed validation | Preserve remote state and fail before credentials are minted |

Pre-fix and post-fix JSON reports are retained for 14 days. Their vulnerability
counts are described as OSV advisory records rather than unique CVEs because
different database aliases can describe the same underlying issue. The normal
read-only audit remains the sole SARIF publisher; uploading SARIF from an
uncommitted remediation workspace could misattribute results to the default
branch.

The pull request presents a separate, bounded review model derived from that
evidence. It correlates overlapping OSV aliases into underlying vulnerabilities,
prefers a CVE and then a GHSA as the human-facing identifier, preserves every
package occurrence, and distinguishes raw advisory-record counts from correlated
issues. The generated title names a single issue and package when possible. The
body leads with review status, impact, file scope, exact version changes, and the
post-fix result; full JSON reports and the patch remain available in the workflow
artifact. Advisory-controlled text is stripped of control characters and escaped
before it reaches GitHub-flavored Markdown.

### Remediation inputs

| Input | Default | Purpose |
|-------|---------|---------|
| `publish` | `false` | Publish or clean up the rolling pull request; false performs a credential-free dry run. Scheduled runs also require `automation.security_remediation = true` |
| `runner` | `ubuntu-24.04` | Linux runner label |
| `upd-version` | current verified manifest | Exact released `upd` version |
| `upd-sha256` | manifest checksum | Archive digest; required for a version outside the release manifest |
| `upd-target` | detected | Linux release target |
| `paths` | `.` | Whitespace-separated audit roots |
| `langs` | all auditable ecosystems | Comma-separated ecosystem filter |
| `allowed-paths` | required | Exact files a validated patch may contain |
| `prepare-command` | empty | Prepare ecosystem tooling without changing tracked files |
| `validation-command` | required | Validate the complete proposed tree |
| `branch` | `security/upd` | Automation-owned rolling branch |
| `commit-message` | `fix(deps): remediate vulnerable dependencies with upd` | Generated Conventional Commit message |
| `pull-request-title` | derived from audit evidence | Optional rolling pull-request title override |
| `auto-merge` | `false` | Ask GitHub to auto-merge only a complete, clean remediation |
| `merge-method` | `squash` | Auto-merge strategy: `squash`, `merge`, or `rebase` |

Remediation accepts no publishing secret or PAT fallback. The broker issues an
installation token only after authorizing the caller's OIDC claims and requests
only Contents and Pull requests write for the target repository.

## Inputs

| Input | Default | Purpose |
|-------|---------|---------|
| `runner` | `ubuntu-24.04` | Linux runner label |
| `upd-version` | current verified manifest | Exact released `upd` version; empty follows the canonical release manifest |
| `upd-sha256` | manifest checksum | Exact archive checksum; required when the selected version is absent from the manifest |
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
| `pull-request-title` | derived from update evidence | Optional pull-request title override |
| `auto-merge` | `false` | Ask GitHub to merge after repository checks pass |
| `merge-method` | `squash` | Auto-merge strategy: `squash`, `merge`, or `rebase` |

## Secrets

| Secret | Purpose |
|--------|---------|
| `pull-request-token` | Fine-grained personal access token; the single-repository alternative to a GitHub App |

If the hosted App is not installed and no `pull-request-token` is supplied, upd
can prepare and validate the proposal but fails before publication. See
[Credentials](#credentials).

For a fully static installation, provide the published archive version and
digest together:

```yaml
with:
  upd-version: v0.9.0
  upd-target: x86_64-unknown-linux-gnu
  upd-sha256: 2e0a27928b44803293a1d3e40042dc8a646421c0aff51b2f95aa18ea99d9d1c4
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

A pin with no annotation at all is not blocked. The commit itself says which
release it belongs to, so `upd` asks the repository which tags name that commit
and takes the highest concrete version among them, discarding moving aliases
such as `v7`. From there the pin behaves like an annotated one: behind the
latest release it is an ordinary update, and already at it, the recovered
version is written beside the unchanged commit and reported in `annotations[]`
rather than as an update. Both spellings of the run say what happened:

```yaml
# before
- uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1

# after
- uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
```

An annotation is a write, so `--check` reports it and exits `1`, and `--apply`
performs it. It is counted separately from updates because nothing moved: the
same commit runs before and after.

Three conditions leave a bare pin alone, each with its own reason. The commit
may belong to no release (`unreleased-commit`, typical of a pin taken from a
branch head), it may be named only by a moving alias (`floating-tag-only`), or
the registry may have no tags to consult (`missing-version-comment`). All three
are settled by writing the version comment yourself. A lookup that never
answered - a rate limit, an outage - is an error rather than a blocked pin, so
a run does not tell you to edit a workflow that would have resolved itself.

A short SHA, floating annotation, moved tag, stale annotation, non-concrete
configured target, or trailing text where the annotation would go is reported as
`blocked` with a machine-readable reason. With `update-action-shas: false`, these
pins are instead reported as `not-examined`. Set `fail-on-blocked: true` when
every immutable pin is expected to be maintainable automatically.

Interactive runs report annotations but do not write them; run without
`--interactive` to apply them.

`max-bump: minor` is a strict ceiling. Bare major references such as `@v4` are
therefore normally held back; use `max-bump: major` when those updates should be
eligible. Changed workflows are validated with `actionlint` before publication.

Configuration pins and package filters use an action's `owner/repo` name. For
example, `packages: actions/checkout` selects checkout references, subdirectory
actions, and reusable workflows from that repository.

## Annotated versions in a workflow

Not every version in a workflow is a `uses:` ref. A tool version passed to an
action through a `with:` input is a real pin, and nothing in the Actions grammar
can see it: the input is an opaque string to `upd`, and the action's own repo
says nothing about which release of the tool it will install. Left alone, such a
version floats or goes stale silently.

An [`upd:` annotation](ecosystems.md#annotated-files) declares its source, and a
workflow is the one recognized file type that is scanned for annotations as well
as by its own updater:

```yaml
- uses: jdx/mise-action@v4
  with:
    version: 2026.8.14 # upd: github-releases jdx/mise
```

Both passes run over the file in one invocation and their findings are merged
into one report, each entry naming the package it belongs to. A `uses:` line
belongs to the Actions updater, so an annotation written there is refused with a
warning rather than acted on: the ref already resolves against the action's own
repository, and a second source for one value is a mistake worth naming. Every
other line is free.

Annotated entries are updates like any other. They obey `ignore`, `pin`,
cooldown, and `max-bump`, and they keep the line's own precision.

### Selecting them with `--lang`

The two passes are selected separately, because they update different things:

| `--lang` | `uses:` refs | annotated lines |
| --- | --- | --- |
| *(omitted)* | yes | yes |
| `actions` | yes | no |
| `annotated` | no | yes |
| the annotation's own source, e.g. `github-releases` | no | yes |
| `actions,annotated` | yes | yes |

So `-l actions` alone does **not** cover the annotated lines in a workflow, and
`-l annotated` alone does not touch its `uses:` refs. A weekly job that wants
both asks for both:

```bash
upd --dry-run -l actions,annotated
```

A workflow is opened by any selection that could reach an annotation, whichever
source it names, and each line is then filtered on its own. A selection that
reaches neither the workflow's own refs nor any annotation, such as `-l
terraform`, leaves the file out of discovery entirely.

## Safety and lifecycle

The reusable workflow:

- installs an exact `upd` release after verifying a trusted SHA-256;
- serializes runs per repository;
- refuses to publish any changed proposal without an App or PAT whose pull
  requests can start checks automatically;
- transfers the validated one-commit proposal across an artifact boundary into
  a separate publication job that never executes repository code;
- keeps the publication credential out of checkout, preparation, package
  managers, validation, and process command lines;
- requests the App's Workflows write permission only when the proposal actually
  changes a workflow file;
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

## Pull-request review experience

The workflow derives a concise title from the machine-readable update report.
A single update becomes a title such as
`chore(deps): refresh serde to 1.0.228`; multi-package proposals report the exact
number of updated dependencies. Set `pull-request-title` only when a repository
needs a fixed override.

The pull-request body is built from a bounded, provider-neutral presentation
model and deliberately uses an email-safe subset of Markdown. The same compact
summary remains legible in GitHub and in GitHub notification email. It shows:

- the update count and major/minor/patch mix;
- up to three non-patch updates that deserve attention, with exact versions;
- counts for releases held by policy and dependencies upd could not change
  safely;
- repository-validation and proposal-integrity results;
- auto-merge intent without claiming the pull request is ready to merge before
  repository checks complete; and
- one link to a complete, human-readable report rendered on the GitHub workflow
  run summary.

Untrusted registry and manifest text is stripped of control characters and
escaped before rendering. The body avoids tables and disclosure sections so it
does not turn into an unreadable notification. The rendered report lists every
applied update, policy hold, blocked item, changed file, and validation result.
Its final section labels the separate ZIP download explicitly as a
machine-readable JSON archive. The report and presentation JSON remain
available in that workflow artifact for seven days.
The GitLab template continues to render the same provider-neutral presentation
model with GitLab-compatible Markdown.

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

For security remediation, opt-in auto-merge has additional fail-closed rules:

- only the `clean_changed` disposition can request it;
- a partial remediation always leaves auto-merge off, even when the caller asks;
- the request is bound to the exact validated commit with
  `--match-head-commit`;
- disabling the input removes a stale auto-merge request from the rolling pull
  request; and
- administrator bypass is never used.

Configure a branch protection rule or ruleset with stable required check names
before enabling this input. Without a required repository condition to wait for,
GitHub may merge an eligible pull request immediately. The safe rollout order is:

1. Require the repository's stable CI and specification checks on the default
   branch.
2. Enable repository auto-merge and confirm the selected merge method is
   allowed.
3. Run one published remediation with `auto-merge: false` and inspect all
   checks.
4. Set `auto-merge: true`, then verify a fresh canary waits for the required
   checks and uses the exact generated head commit.

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
