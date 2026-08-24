# GitLab merge requests

`upd` ships a reusable GitLab CI template that maintains one rolling dependency
merge request. The CLI edits the checkout; the template safely owns the branch,
commit, merge request, and optional GitLab-native auto-merge.

Every run rebuilds the automation branch from the latest default branch. That
keeps the proposal current and limits the branch to one generated commit.

## Set up authentication

Create a dedicated token with `api` and `write_repository` scopes. Its role must
be allowed to create branches and merge requests (normally **Developer**) and,
when auto-merge is enabled, merge into the protected target branch.

Project access tokens are available for self-managed GitLab and for GitLab.com
Premium or Ultimate. On GitLab.com Free, use a narrowly scoped group access token
or personal access token instead.

Add the token under **Settings > CI/CD > Variables** as
`UPD_GITLAB_TOKEN`. Mark it masked and protected, and schedule the protected
default branch so the variable is available.

`CI_JOB_TOKEN` is deliberately unsupported: its merge-request API permissions
are read-only, and Git pushes authenticated by job token do not start pipelines.

## Include the template

Reference an immutable commit and configure the template through typed inputs:

```yaml
include:
  - remote: "https://raw.githubusercontent.com/rvben/upd/<FULL_COMMIT_SHA>/ci/gitlab-dependency-update.yml"
    inputs:
      min_age: "7d"
      max_bump: "minor"
      validation_command: "make test"

upd-dependency-update:
  extends: .upd-dependency-update
```

Replace `<FULL_COMMIT_SHA>` with a revision containing the template. Pinning the
include prevents a later repository change from silently changing executable CI
code. The template also pins its default container by digest and its `upd`
archive by version and SHA-256.

If your GitLab instance cannot fetch public remote includes, mirror or copy
[`ci/gitlab-dependency-update.yml`](../ci/gitlab-dependency-update.yml) into the
project and use `include: project` or `include: local`. The file has a GitLab
`spec:inputs` interface, but is not published as a Catalog component because its
canonical source is outside GitLab.

The job uses the existing `test` stage by default and runs only for scheduled or
manually started (`web`) pipelines. Create a pipeline schedule targeting the
default branch. If the project defines `workflow: rules`, those rules must allow
both pipeline sources.

## Choose the job image

The safe default updates manifest constraints only (`lock: false`). Its pinned
Debian image contains no language toolchains, so it is suitable for repositories
that do not regenerate lockfiles or run ecosystem-specific validation.

For lockfiles and validation, use the same digest-pinned CI image as the project:

```yaml
include:
  - remote: "https://raw.githubusercontent.com/rvben/upd/<FULL_COMMIT_SHA>/ci/gitlab-dependency-update.yml"
    inputs:
      image: "registry.example.com/my-group/my-project/ci@sha256:<IMAGE_DIGEST>"
      lock: true
      prepare_command: "corepack enable"
      validation_command: "npm test"

upd-dependency-update:
  extends: .upd-dependency-update
```

The image must provide the tools needed by the selected ecosystems. The template
bootstraps Bash, Git, curl, jq, tar, and checksum utilities with `apt-get` or
`apk` only when they are absent. `prepare_command` can initialize existing tools,
but must leave the repository clean; dependency updates belong exclusively to
`upd`.

## Inputs

| Input | Default | Purpose |
|-------|---------|---------|
| `stage` | `test` | Existing pipeline stage for the job |
| `image` | pinned Debian digest | Linux job image; Debian and Alpine bootstrapping are supported |
| `upd_version` | `v0.6.4` | Exact released `upd` version |
| `upd_sha256` | built in for the default version | Exact archive checksum when changing the version or target |
| `upd_target` | detected | Release target; Linux x86-64 and ARM64 GNU are detected |
| `paths` | `.` | Whitespace-separated repository paths passed to `upd` |
| `langs` | empty | Comma-separated ecosystem filter |
| `packages` | empty | Comma-separated package filter |
| `min_age` | `7d` | Minimum eligible release age; empty uses project configuration |
| `max_bump` | `minor` | Highest applied bump; empty uses project configuration |
| `lock` | `false` | Regenerate lockfiles; requires ecosystem tools in the image |
| `prepare_command` | empty | Prepare project tooling without modifying repository files |
| `validation_command` | empty | Check updates before publishing |
| `branch` | `automation/upd-dependencies` | Automation-owned rolling branch |
| `commit_message` | `chore(deps): update dependencies with upd` | Generated commit message |
| `mr_title` | `chore(deps): update dependencies with upd` | Merge-request title |
| `auto_merge` | `false` | Ask GitLab to merge after project checks pass |

Input types and formats are checked while GitLab creates the pipeline. When
changing `upd_version` or `upd_target`, also supply the published archive digest:

```yaml
include:
  - remote: "https://raw.githubusercontent.com/rvben/upd/<FULL_COMMIT_SHA>/ci/gitlab-dependency-update.yml"
    inputs:
      upd_version: "v0.6.4"
      upd_target: "x86_64-unknown-linux-gnu"
      upd_sha256: "89fbf11df6bdd3b8542788d8775131e5891c3887ac203004c08327c2148c95e8"
```

The runner needs outbound HTTPS access to the pinned GitHub release artifact.
For isolated runners, vendor the binary in a trusted internal image and adapt a
local copy of the template.

## Safety and lifecycle

The template:

- downloads an exact release artifact and verifies its SHA-256 before execution;
- serializes jobs with a resource group;
- starts from the latest default branch on every run;
- updates the remote branch with `--force-with-lease`, never a blind force push;
- refuses ambiguous duplicate open merge requests;
- fails if preparation or validation leaves unexpected repository changes;
- retains the machine-readable update report as a one-week CI artifact;
- creates or updates one automation-owned merge request; and
- closes the obsolete merge request and lease-deletes its branch when no eligible
  updates remain.

Treat the configured branch, generated commit, title, and description as
automation-owned. A later successful run replaces them.

## Auto-merge

Auto-merge is deliberately opt-in:

```yaml
include:
  - remote: "https://raw.githubusercontent.com/rvben/upd/<FULL_COMMIT_SHA>/ci/gitlab-dependency-update.yml"
    inputs:
      validation_command: "make test"
      auto_merge: true

upd-dependency-update:
  extends: .upd-dependency-update
```

The template sends GitLab the exact source commit SHA. GitLab still enforces
required pipelines, approvals, resolved discussions, protected branches, and
merge trains; the job never bypasses those controls. Turning `auto_merge` off
cancels auto-merge if this job previously enabled it.

The `auto_merge` API option requires GitLab 17.11 or newer. Creating and updating
merge requests works on older supported versions without that option.

## Scope

This integration intentionally produces one policy-constrained rolling merge
request. It does not provide Renovate-style per-package branches, dependency
dashboards, reviewer assignment, conflict resolution, or automatic rebasing.
