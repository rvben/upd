# GitLab merge requests

`upd` ships a reusable GitLab CI template that maintains one rolling dependency
merge request. The template installs a pinned `upd` release and runs
`upd gitlab run`, which edits the checkout and safely owns the branch, commit,
merge request, and optional GitLab-native auto-merge.

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

Replace `<FULL_COMMIT_SHA>` with a revision containing the template, preferably
the `chore(release): refresh integration pins` commit that follows a release:
that revision's default `upd_version` is the release itself, while the tagged
revision still defaults to the release before it. Pinning the
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
bootstraps Bash, Git, curl, tar, and checksum utilities with `apt-get` or
`apk` only when they are absent. `prepare_command` can initialize existing tools,
but must leave the repository clean; dependency updates belong exclusively to
`upd`.

## Inputs

| Input | Default | Purpose |
|-------|---------|---------|
| `stage` | `test` | Existing pipeline stage for the job |
| `image` | pinned Debian digest | Linux job image; Debian and Alpine bootstrapping are supported |
| `upd_version` | `v0.14.4` | Exact released `upd` version |
| `upd_sha256` | built in for the default version | Exact archive checksum when changing the version or target |
| `upd_target` | detected | Release target; Linux x86-64 and ARM64 GNU are detected |
| `paths` | `.` | Whitespace-separated repository paths passed to `upd` |
| `langs` | empty | Comma-separated ecosystem filter |
| `packages` | empty | Comma-separated exact-name or case-sensitive glob filter |
| `min_age` | `7d` | Minimum eligible release age; empty uses project configuration |
| `max_bump` | `minor` | Highest applied bump; empty uses project configuration |
| `lock` | `false` | Regenerate lockfiles; requires ecosystem tools in the image |
| `security_remediation` | `true` | First move dependencies with a published advisory to the lowest release that resolves it (see [Security fixes](#security-fixes)) |
| `prepare_command` | empty | Prepare project tooling without modifying repository files |
| `validation_command` | empty | Check updates before publishing |
| `branch` | `automation/upd-dependencies` | Automation-owned rolling branch |
| `commit_message` | `chore(deps): update dependencies with upd` | Generated commit message |
| `mr_title` | derived from update evidence | Optional merge-request title override |
| `auto_merge` | `false` | Ask GitLab to merge after project checks pass |
| `major_mr` | `false` | Keep a second rolling merge request for the major-version upgrades `max_bump` holds back (see [Major upgrades](#major-upgrades)) |
| `major_branch` | `automation/upd-dependencies-major` | Automation-owned rolling branch of the major merge request |
| `major_commit_message` | `chore(deps): update major dependencies with upd` | Generated commit message of the major merge request |

Input types and formats are checked while GitLab creates the pipeline. The job
runs `upd gitlab run`, so `upd_version` must name a release that provides that
command; an older release fails with an unrecognized-subcommand error. When
changing `upd_version` or `upd_target`, also supply the published archive digest:

```yaml
include:
  - remote: "https://raw.githubusercontent.com/rvben/upd/<FULL_COMMIT_SHA>/ci/gitlab-dependency-update.yml"
    inputs:
      upd_version: "v0.14.4"
      upd_target: "x86_64-unknown-linux-gnu"
      upd_sha256: "493b7faa112cfd836c391725153a01085bc666718d456df1a52560a6292f60de"
```

The runner needs outbound HTTPS access to the pinned GitHub release artifact.
For isolated runners, vendor the binary in a trusted internal image and adapt a
local copy of the template.

## Safety and lifecycle

The template:

- downloads an exact release artifact and verifies its SHA-256 before execution;
- serializes jobs with a resource group;
- passes `UPD_GITLAB_TOKEN` only to its own Git and GitLab API calls, never to
  `prepare_command`, `validation_command`, or the dependency update itself;
- starts from the latest default branch on every run;
- pauses if the automation branch contains commits outside its single generated
  commit, preserving the branch and adding a notice to the open merge request;
- updates the remote branch with a lease on the commit it last saw, never a
  blind force push, and fails with a conflict if the branch moved meanwhile;
- leaves the branch untouched when it already holds exactly the commit the run
  would write (same content, on the current default branch, with the configured
  identity and message), so a scheduled run with nothing new does not restart
  the merge request's pipeline or reset its approvals, and edits the merge
  request only when its title or description differ;
- refuses ambiguous duplicate open merge requests;
- fails if preparation or validation leaves unexpected repository changes;
- applies security fixes in a separate step before the update, from the same
  checkout and with the same token isolation, and stops before the update if
  one fails;
- retains the machine-readable update and security reports as a one-week CI
  artifact;
- creates or updates one automation-owned merge request; and
- lease-deletes the obsolete branch and then closes its merge request when no
  eligible updates remain, so a commit pushed during the run keeps the merge
  request open.

The pause check runs before both branch replacement and no-update cleanup. If
someone needs to adapt an update, they can commit to the automation branch;
scheduled runs will leave that work in place. Automation resumes after those
commits are removed or the merge request is resolved. The check expects the
existing branch to contain one commit, based on the default branch's history,
that automation wrote: either a commit with the configured automation author
and committer email and the exact commit message, or the commit that the
single open merge request's description records in a hidden
`<!-- upd-commit: ... -->` line. Every publishing run writes that record, so a
run can still replace its own commit after the `commit_message` input or
`UPD_GIT_EMAIL` changes while a merge request is open. A merge request whose
description lacks the record, such as one last updated by an earlier upd version,
still pauses after such a change; delete the automation branch to let the
next run start over. The name is not part of the check, so changing
`UPD_GIT_NAME` alone never pauses.

Treat the configured branch, generated commit, title, and description as
automation-owned unless intentionally pausing the branch as above.

## Merge-request review experience

The template derives a concise title from the machine-readable update report.
A single update becomes a title such as
`chore(deps): update serde to 1.0.228`; multi-package proposals report the exact
number of updated dependencies. Set `mr_title` only when a project needs a fixed
override.

GitLab and GitHub share the same bounded presentation schema. The merge request
leads with review status, exact version and file changes, active update policy,
validation evidence, policy-held releases, and dependencies that need manual
attention. GitLab uses project-native Markdown rather than GitHub alert syntax.

Registry and manifest text is stripped of control characters and escaped before
rendering. The complete update report, presentation JSON, and rendered merge
request description are retained under the `.upd-ci` pipeline artifact even when
the description reaches its display budget.

## Security fixes

Before the update, each run applies `upd audit --fix-audit`: every dependency
with a published advisory moves to the lowest release that resolves it. The
fix follows the advisory, not the update policy, so `min_age`, `max_bump` and
`packages` do not hold it back; `paths`, `langs` and the repository
configuration still apply, so a package the configuration ignores, or pins
below the fix, is left as it is. The update then runs on top, under its usual
policy.

The merge request lists the fixes first, with their advisories and severity, and
its title says so (`fix(security): resolve advisory in lodash`). A merge request
that carries only security fixes is still published. Advisories are looked up
through [OSV](https://osv.dev), so the runner needs outbound HTTPS access to
`api.osv.dev`.

A fix's relock can lock other releases besides the fix, such as a new
transitive dependency the fixed version requires. The security step reads each
relocked lockfile back against `min_age`, and lists every release it finds
inside the freshness window, other than the fixes themselves, under "Needs
attention". upd leaves them locked, since holding one back could undo the fix.

The update runs after the fixes and can move a fixed dependency again. When
it changes the tree the fixes left, upd audits the result once more, over the
same paths and languages. A fixed dependency that audit finds vulnerable again
is listed under "Needs attention" with the release it moved to, the
advisories that affect it and the files the security step fixed it in (that
audit does not say which lockfile holds the release), and its advisories no
longer count as resolved;
the merge request is still published. For a fix that awaits lockfile
regeneration, the vulnerable version the security step found is not flagged
again, since a lockfile left as it was still records it. An
audit of the updated tree that reports errors stops the run, as a security
report with errors does.

A security audit can also report something it could not check, such as a
release a fix's relock locked whose age the registry could not tell. Each such
warning, from the security step or from the audit of the updated tree, is
printed to the job log and listed under "Needs attention", since what it names
was not verified.

With `lock: false`, a fix rewrites the manifest only and is marked as awaiting
lockfile regeneration: until a lockfile is regenerated it still records the
vulnerable version. A fix that can only be made in a lockfile (a Cargo
transitive dependency, for example) needs `lock: true` and is listed under
"Needs attention" meanwhile, and so is a dependency whose manifest already
requires the fix while its lockfile still records the vulnerable version.
With `lock: true`, a fix a manifest requirement excludes is listed there too,
with the requirement that blocks it, as is a fix upd did not write, with the
reason: the configuration ignores or pins the package, for example. Advisories
no release resolves have their own section. None of these stop the run; a
security fix that fails, or a security report with errors, stops it before the
update, since publishing a partial result would hide the gap.

The security report is kept beside the update report as
`.upd-ci/upd-security-report.json`, and the audit after the update as
`.upd-ci/upd-security-recheck.json`. Set `security_remediation: false` to leave
advisories to another process.

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

GitLab records a push and rechecks the merge request in background jobs, so
right after pushing, the job waits for the merge request to head the pushed
commit and for its mergeability check to finish before it requests
auto-merge, and retries opening a merge request while GitLab does not list the
new branch yet. Each wait gives up after at most about a minute and a half; a
job that gives up fails with a network error (exit code 3), and the next run
picks up where it stopped.

The `auto_merge` API option requires GitLab 17.11 or newer. Creating and updating
merge requests works on older supported versions without that option.

## Major upgrades

`max_bump` keeps the rolling merge request small enough to merge routinely, which
leaves major-version upgrades waiting. Setting `major_mr` proposes them in a
second rolling merge request of their own:

```yaml
include:
  - remote: "https://raw.githubusercontent.com/rvben/upd/<FULL_COMMIT_SHA>/ci/gitlab-dependency-update.yml"
    inputs:
      max_bump: "minor"
      auto_merge: true
      major_mr: true

upd-dependency-update:
  extends: .upd-dependency-update
```

After the usual merge request, the same job starts again from the default
branch on `major_branch` and asks `upd` for major-version upgrades alone
(`--only-bump major --strict-bump`). The two merge requests never carry the
same change:

- The major merge request holds only version upgrades that cross a major
  version. Configured pins, Nix flake revisions, and other rewrites that are
  not a major upgrade stay in the ordinary merge request, even when no major
  upgrade is available.
- The ordinary merge request still lists the minor and patch releases its
  ceiling held, and replaces the held majors with a link to the open major
  merge request.
- `major_mr` needs `max_bump` set to `minor` or `patch`; with no lower ceiling
  the ordinary merge request already carries major upgrades, and the job
  refuses to start. `major_branch` must differ from `branch` and from the
  default branch, and neither may be nested under the other's name (`deps`
  and `deps/major`), since git cannot hold both.

upd never merges the major merge request. Its description says so, `auto_merge`
does not apply to it, and a run that publishes to it and finds auto-merge armed
(enabled by hand, say) cancels it. It is meant for a person to review the
breaking changes and merge by hand.

Everything else works as in the ordinary lane, per branch:

- `prepare_command` and `validation_command` run again for the major lane, in a
  checkout reset to the default branch, so nothing from the ordinary lane leaks
  into it.
- A commit someone pushes to one lane's branch pauses only that lane.
- When no major upgrade remains, the branch is deleted and its merge request
  closed.
- `mr_title` applies to the ordinary merge request only; the major merge
  request's title is always derived from its updates.
- Its evidence is kept beside the ordinary lane's in the `.upd-ci` artifact,
  as `upd-major-report.json`, `upd-major-presentation.json`, and
  `upd-major-mr-description.md`.

Both merge requests start from the same default branch, so merging one can
leave the other conflicting, for example when both change the same lockfile.
The next scheduled run rebuilds each branch from the new default branch, which
clears the conflict; there is no need to resolve it by hand.

The lanes fail independently: a failed ordinary lane still lets the major lane
run, and the job fails if either lane failed.

## Running `upd gitlab run` directly

The template is a thin wrapper: it installs and verifies `upd`, then runs
`upd gitlab run`. A project that vendors `upd` in its own image can call the
command from its own job instead. It reads the GitLab CI job environment:

| Variable | Required | Meaning |
|----------|----------|---------|
| `UPD_GITLAB_TOKEN` | yes | Token described under authentication |
| `CI_API_V4_URL`, `CI_DEFAULT_BRANCH`, `CI_PROJECT_DIR`, `CI_PROJECT_ID`, `CI_PROJECT_PATH`, `CI_SERVER_URL` | yes | Predefined by GitLab CI |
| `UPD_BRANCH` | no | Rolling branch (`automation/upd-dependencies`) |
| `UPD_PATHS` | no | Whitespace-separated paths (`.`) |
| `UPD_LANGS`, `UPD_PACKAGES`, `UPD_MIN_AGE`, `UPD_MAX_BUMP` | no | Update filters and policy; empty defers to project configuration |
| `UPD_LOCK`, `UPD_AUTO_MERGE` | no | `true` or `false` (`false`) |
| `UPD_SECURITY_REMEDIATION` | no | `true` or `false` (`true`); applies [security fixes](#security-fixes) first |
| `UPD_PREPARE_COMMAND`, `UPD_VALIDATION_COMMAND` | no | Bash commands run with `set -euo pipefail` in the checkout |
| `UPD_COMMIT_MESSAGE`, `UPD_MR_TITLE` | no | Commit message and title override |
| `UPD_GIT_NAME`, `UPD_GIT_EMAIL` | no | Automation commit identity (`upd automation`, `upd-automation@noreply.invalid`) |
| `UPD_MAJOR_MR` | no | `true` or `false` (`false`); enables the [major lane](#major-upgrades) |
| `UPD_MAJOR_BRANCH`, `UPD_MAJOR_COMMIT_MESSAGE` | no | Major lane branch (`automation/upd-dependencies-major`) and commit message (`chore(deps): update major dependencies with upd`) |
| `UPD_EXECUTABLE` | no | `upd` binary that performs the update (the running `upd`) |

The run switches the checkout between branches and resets it, so it needs a
clean checkout: one with uncommitted changes or untracked files is refused
(exit code 2) before either lane starts.

Progress goes to stderr. The outcome goes to stdout, as one line of text or, with
`--output json`, an object whose `outcome` is `clean`, `closed`, `published`, or
`paused`. A `published` outcome carries `pushed: false` when the branch already
held the update and only the merge request was checked. With `--dry-run` the
update, validation and ownership checks still run, but nothing is pushed and
nothing is written to GitLab; the outcome is then `would_publish` (with `push`
saying whether the branch would change), `would_close`, or `would_pause` in
place of the last three. When the security step ran, the JSON object also
carries `security` with the number of `fixes`, `pending_relock`, `blocked`,
`skipped`, `not_applied` and `unfixable` entries and of `advisories` resolved,
plus `young`, the releases a fix's relock locked inside the freshness window,
`reintroduced`, the fixed dependencies the update moved back to a
vulnerable release, and `audit_warnings`, the warnings the security audits
reported about what they could not check, each when there are any.
With the major lane enabled, its result follows on a second line of text, or
under `major` in the JSON object with the same fields. A lane that failed
reports the outcome `failed` with its `error` (`kind`, `message`, `exit_code`)
while the other lane's result is still reported.
Failures print a JSON error to stderr and exit with the code listed in
`upd schema`: 4 for missing or invalid settings, 3 for network and GitLab
server errors (retryable), 5 when the branch moved during the run, and 2 for
everything else, including API rejections and states the run refuses to act on.

## Organization mode

One central project can keep the rolling merge request in every project of a
group, with each project deciding for itself whether it takes part. The
[`ci/gitlab-organization-update.yml`](../ci/gitlab-organization-update.yml)
template runs `upd gitlab org run`, which lists the group's projects (subgroups
included) and, for each one that opted in, does exactly what `upd gitlab run`
does in a single project: same branch ownership rules, same lease-protected
push, same merge request.

### Opt in per project

A project takes part only when the configuration file at the root of its
default branch says so:

```toml
# .updrc.toml
[automation]
dependency_updates = true
auto_merge = true   # optional; the central job must allow it too
major_mr = true     # optional; the central job must enable it too
```

The file is found the way `upd` finds configuration (`.updrc.toml`, then
`upd.toml`, then `.updrc`). A quick read through the API skips projects that
have not opted in without cloning them; for the rest, the copy in the
default-branch commit the update starts from makes the final decision. A project without the file,
or without `dependency_updates = true`, is reported as not opted in. A file that
cannot be parsed, or that is not a regular file, is reported as invalid and
fails the job, so a broken opt-in is visible instead of silently ignored. The
update then runs with that file as its configuration, so the project's own
policy (cooldowns, ignores, pins) applies.

Turning `dependency_updates` off stops future runs from touching the project; an
existing merge request is left open for the project to close.

### Set up the central project

Create a group access token (or, on GitLab.com Free, a personal access token of
a dedicated service user) with `api` and `write_repository` scopes and a role
that can push branches and create merge requests in every project of the group,
normally **Developer**. When auto-merge is used, the role must also be allowed to
merge into the protected target branches. Store it as the masked, protected
`UPD_GITLAB_TOKEN` variable of the central project. GitLab grants a new or
rotated token its access to the group's projects in the background, so for up
to a few minutes it can list only some of them; the first run after creating
or rotating the token may report fewer projects than the group holds.

```yaml
include:
  - remote: "https://raw.githubusercontent.com/rvben/upd/<FULL_COMMIT_SHA>/ci/gitlab-organization-update.yml"
    inputs:
      group: "my-group"
      exclude: "my-group/legacy-* my-group/sandbox/*"

upd-organization-update:
  extends: .upd-organization-update
```

Schedule a pipeline on the central project's default branch. The central project
itself is always skipped; give it its own `upd gitlab run` job if it needs
updates. The template requires GitLab 16.11 or newer, because it restricts its
report artifact to project members with the Developer role or higher. As with
the single-project template, include the pin-refresh commit that follows a
release: `upd_version` must name a release that provides `upd gitlab org run`.

### Organization inputs

| Input | Default | Purpose |
|-------|---------|---------|
| `group` | required | Full path or numeric ID of the group, subgroups included |
| `stage`, `image`, `upd_version`, `upd_sha256`, `upd_target` | as above | Job placement and the pinned `upd` release |
| `exclude` | empty | Whitespace-separated globs of project paths to leave alone |
| `langs` | empty | Comma-separated ecosystem filter applied to every project |
| `min_age` | `7d` | Shortest release age any project accepts; longer project cooldowns still apply |
| `max_bump` | `minor` | Highest applied bump; empty uses each project's configuration |
| `branch`, `commit_message` | as above | Rolling branch and generated commit message in every project |
| `auto_merge` | `false` | Allow auto-merge in projects that also set `auto_merge = true` |
| `major_mr` | `false` | Keep a [major-upgrade merge request](#major-upgrades) in projects that also set `major_mr = true`; needs `max_bump` set to `minor` or `patch` |
| `major_branch`, `major_commit_message` | as above | Rolling branch and generated commit message of the major merge request in every project |
| `concurrency` | `4` | Projects processed at the same time (1 to 16) |
| `dry_run` | `false` | Report what each project would get without pushing or writing to GitLab |

`min_age` is a floor rather than an override: a project that configures a
longer cooldown keeps it. Organization mode never runs `nix flake update`, which
would evaluate repository content in a job holding a group-wide token, so Nix
is always left out and `langs` cannot select it. Lockfile regeneration,
preparation and validation commands are single-project features and are not
offered here: one job image cannot carry every project's toolchain.

### Results

Every listed project gets one line in the job log: skipped (archived, empty,
excluded, pending deletion, repository disabled, or the central project), not
opted in, invalid configuration, processed with the same outcome `upd gitlab
run` reports, or failed. A failing project does not stop the others. The job
fails when any project failed or had an invalid opt-in, so a scheduled run
surfaces problems without hiding the projects that succeeded.

With `major_mr` enabled, a project that also set `major_mr = true` gets a second
line for its major lane. That lane runs even when the project's ordinary lane
failed, and a failed major lane fails the job too; the report counts those
projects separately as `major_failed`.

The full JSON report, including each project's merge request URL or error, is
kept for one week as the `.upd-ci/upd-org-report.json` artifact. Its shape is
described by `upd schema` under `gitlab org run`.

## Scope

This integration intentionally produces one policy-constrained rolling merge
request, plus the optional major-upgrade merge request. It does not provide Renovate-style per-package branches, dependency
dashboards, reviewer assignment, conflict resolution, or automatic rebasing.
