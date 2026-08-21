# GitHub Actions

`upd` updates `uses:` references in `.github/workflows/*.yml` and `*.yaml`,
covering both actions and reusable workflows. It skips branch refs, local
actions, and Docker references, and authenticates via `GITHUB_TOKEN` or
`GH_TOKEN` for higher API rate limits.

```bash
upd --lang actions            # Preview Actions updates only
upd --apply --lang actions    # Write them
```

## Immutable SHA pins

Commit pins are checked by default, and rewriting one requires a concrete
version annotation:

```yaml
- uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2

jobs:
  conformance:
    uses: rvben/clispec/.github/workflows/conformance.yml@<full-commit-sha> # v0.3.0
```

```bash
upd update . \
  --apply \
  --lang actions \
  --min-age 7d \
  --max-bump minor
```

Before writing, `upd` verifies that the annotated current tag resolves to the
pinned commit. It then applies cooldown and bump policy to release versions,
resolves the selected tag to its full commit SHA, and updates both the SHA and
comment. A pin is never converted to a mutable tag. A short SHA, missing or
floating comment (`# v4`), moved tag, or stale comment is never rewritten.
Structured output reports these under `files[].skipped[]` with
`status: "blocked"` and a machine-readable `reason`.

The comment may write the version with or without the `v` prefix, whichever the
repo's own tags use: `# 7.0.1` and `# v7.0.1` both verify against a repo tagging
`v7.0.1`. The prefix style of each comment is preserved when it is rewritten, so
`# 5.0.0` becomes `# 7.0.1` and `# v5.0.0` becomes `# v7.0.1`. The exception is a
repository that publishes both spellings of one release at different commits:
there the comment takes the spelling of the tag that was actually resolved, since
the other one names a different commit. A comment that resolves to some commit
other than the pinned one is still refused, whichever spelling it uses.

### Turning SHA updates off

With SHA updates turned off (`--no-update-action-shas`, or
`update_action_shas = false` in `.updrc.toml`), the pins still appear in
`files[].skipped[]`, under `status: "not-examined"` with
`reason: "action-sha-updates-off"`, and are counted in
`summary.not_examined`.

The two statuses answer different questions: `blocked` means the pin was
examined and a safety condition refused the change, `not-examined` means it was
never looked at. Neither is `summary.unchanged`, which counts dependencies that
were checked and found current. In text output the count appears in the summary
and `--verbose` names each pin.

### Naming an action in config

Configuration pins and `--package` filters use the action's `owner/repo` name.
For example, `--package actions/checkout` selects every checkout reference,
including subdirectory actions and reusable workflow paths from that repository.
Configured targets for immutable pins must also be concrete SemVer tags; a
floating value such as `v5` is reported as blocked.

## Automated pull requests

This repository publishes a reusable workflow that installs a checksum-verified
`upd` release, applies only policy-approved Actions updates, validates every
workflow with `actionlint`, and opens one Conventional Commits pull request:

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
  actions:
    uses: rvben/upd/.github/workflows/dependency-health.yml@bdc55ece90b7333f177027ad208705b815b6caab # v0.5.2
    with:
      upd-version: v0.5.2
      min-age: 7d
      max-bump: minor
      validation-command: make test
    secrets:
      pull-request-token: ${{ secrets.UPD_PR_TOKEN }}
```

Select an `upd-version` that advertises `--update-action-shas`; the workflow
checks this capability explicitly and fails with upgrade guidance otherwise.

Use a narrowly scoped GitHub App token or fine-grained PAT for
`UPD_PR_TOKEN` when the generated PR must trigger CI. Without it, the workflow
falls back to `GITHUB_TOKEN`; GitHub suppresses subsequent workflow runs caused
by that token. Set `fail-on-blocked: true` when every SHA pin is expected to
carry a verified version annotation.

### Migrating off Dependabot

For an existing fleet, keep GitHub Actions updates enabled in Dependabot for
four successful weekly `upd` cycles. During that proving period, review the
workflow summaries, resolve intentionally blocked legacy pins, and configure
`UPD_PR_TOKEN` before relying on PR-triggered CI. After four green cycles,
remove only the overlapping `github-actions` Dependabot updates; keep its other
package ecosystems enabled.

## See also

- [Configuration](configuration.md) for `update_action_shas`, `ignore`, and `[pin]`
- [Private registries](private-registries.md#github-actions-and-pre-commit) for token setup
- [Ecosystems](ecosystems.md) for pre-commit and the other supported files
