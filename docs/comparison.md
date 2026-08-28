# Comparing upd with dependency tools

This page positions `upd` among dependency discovery, update, package-management,
and pinning tools. It is a dated technical snapshot, not a claim that every tool
solves the same problem.

**Snapshot date:** 2026-08-28. **upd version:** 0.8.3.

The versions below are part of the comparison. Features added after those
versions are not represented until this page is refreshed.

## Choose by the job

These tools overlap, but they are not interchangeable. Start with the job you
need done:

| Your primary job | Start with | Why |
|---|---|---|
| Preview and edit dependency versions across a mixed-language repository from one local command | **upd** | Dry-run by default, format-preserving edits, cross-ecosystem filtering, and no service setup |
| Resolve, lock, install, and manage a Python environment | [uv](https://github.com/astral-sh/uv) or [PDM](https://github.com/pdm-project/pdm) | They are full Python package and project managers; `upd` deliberately delegates resolution |
| Continuously open update PRs across the broadest manager and datasource catalog | [Renovate](https://github.com/renovatebot/renovate) or Dependabot | Repository automation and ecosystem-specific resolution are their core jobs |
| Manage only pre-commit hooks or enforce immutable CI references | [prek](https://github.com/j178/prek), [pinact](https://github.com/suzuki-shunsuke/pinact), or [ratchet](https://github.com/sethvargo/ratchet) | A specialized tool offers deeper policy for that one surface |

`upd` can run in CI and publish rolling GitHub pull requests or GitLab merge
requests, but its center of gravity is the same operation developers use
locally: discover, preview, filter, and apply reviewable dependency edits. It is
best suited to polyglot repositories that value one local interface without
giving up each ecosystem's own package manager for lockfile resolution.

## How to read the comparison

- **Native** means the tool understands and operates on that input itself.
- **Delegated** means the tool invokes an ecosystem package manager to do the
  lock or installation work.
- **Sync** means it copies versions already chosen in one file into another; it
  does not discover a newer release from an upstream registry.
- **Out of scope** is deliberately different from unsupported. It means the
  feature is not part of the tool's intended job.

The most useful comparison is therefore within a workload: updating a Python
manifest, resolving a Python lockfile, updating pre-commit revisions, or
updating and pinning GitHub Actions. Comparing a local editor with a service
that clones repositories and prepares pull requests would mostly measure the
extra workflow, not dependency checking.

## Feature matrix

| Tool | Version | Primary job | Manifest constraints | Lockfiles and transitive dependencies | `pyproject.toml` / `uv.lock` | pre-commit / `prek.toml` | GitHub Actions versions / SHAs | Other ecosystems |
|---|---:|---|---|---|---|---|---|---|
| **upd** | 0.8.3 | Local multi-ecosystem checker and updater | Native; format-preserving lower-bound and pin updates | Delegates lock refresh; reads supported locks for transitive security audit and fixes | Native manifest / audit + delegated refresh | Native / out of scope | Native tags and verified SHA comments | npm, Cargo, Go, RubyGems, NuGet, Terraform, Mise/asdf, annotated pins |
| [Renovate](https://github.com/renovatebot/renovate/tree/44.49.0) | 44.49.0 | Repository automation and update PRs | Native across many managers | Native lock maintenance, including transitive lock updates | Native PEP 621, Poetry, and uv managers | Native beta manager / out of scope | Native tags and digest pinning | Broad package, container, infrastructure, and CI coverage |
| [uv](https://github.com/astral-sh/uv/tree/0.12.7) | 0.12.7 | Python package and project manager | Native project dependency management; ordinary lock upgrades stay within constraints | Native universal resolution and `uv.lock` updates | Native / native | Out of scope | Out of scope | Python only |
| [prek](https://github.com/j178/prek/tree/v0.5.0) | 0.5.0 | Git hook manager and pre-commit replacement | Out of scope except hook revisions | Hook environments, not project dependency lockfiles | Out of scope | Native update/check for both formats | Out of scope | Hook runtimes and repositories |
| [pinact](https://github.com/suzuki-shunsuke/pinact/tree/v4.1.1) | 4.1.1 | Pin and update GitHub Actions | Out of scope | Out of scope | Out of scope | Out of scope | Native tags, SHAs, verification, and minimum age | GitHub/Gitea/Forgejo workflow and composite-action files |
| [actions-up](https://github.com/azat-io/actions-up/tree/v1.18.0) | 1.18.0 | Interactive GitHub Actions updater | Out of scope | Out of scope | Out of scope | Out of scope | Native tags and SHA pinning | GitHub Actions only |
| [taze](https://github.com/antfu-collective/taze/tree/v21.1.0) | 21.1.0 | npm manifest and GitHub Actions updater | Native npm range updates | Does not update package-manager lockfiles | Out of scope | Out of scope | Native tags with optional SHA pinning | `package.json` and npm workspaces |
| [ratchet](https://github.com/sethvargo/ratchet/tree/v0.12.0) | 0.12.0 | Pin mutable CI/CD references | Updates its recorded CI reference constraints | Out of scope | Out of scope | Out of scope | Native pin, update, upgrade, and lint | Containers plus CircleCI, GitLab CI, Cloud Build, Drone, and Tekton |
| [Dependabot CLI](https://github.com/dependabot/cli/tree/v1.92.0) | 1.92.0 | Run containerized Dependabot update jobs | Native through Dependabot ecosystem updaters | Native ecosystem resolution; direct and transitive update jobs | Native pip/uv jobs | Out of scope | Native `github-actions` jobs | Broad package-manager coverage; emits PR-operation data but does not create PRs itself |
| [PDM](https://github.com/pdm-project/pdm/tree/2.28.2) | 2.28.2 | Python package and project manager | Native Python project dependency management | Native `pdm.lock`/`pylock.toml` resolution; optional uv resolver backend | Native / out of scope | Out of scope | Out of scope | Python only |
| [pin-github-action](https://github.com/mheap/pin-github-action/tree/v3.5.2) | 3.5.2 | Resolve configured Action refs to SHAs | Out of scope | Out of scope | Out of scope | Out of scope | Native SHA pin/refresh for the existing ref; not a latest-version selector | GitHub Actions only |
| [tsvikas/sync-with-uv](https://github.com/tsvikas/sync-with-uv/tree/v0.6.0) | 0.6.0 | Align hook versions with `uv.lock` | Sync only | Reads versions already resolved by uv | Reads configuration / reads lock | Syncs both formats | Out of scope | Python hook dependencies |
| [dribia/sync-with-uv](https://github.com/dribia/sync-with-uv/tree/v0.1.5) | 0.1.5 | Align pre-commit versions with `uv.lock` | Sync only | Reads versions already resolved by uv | Reads configuration / reads lock | Syncs YAML / out of scope | Out of scope | Python hook dependencies |
| [sync-pre-commit-with-uv](https://github.com/ewjoachim/sync-pre-commit-with-uv/tree/1.1.0) | 1.1.0 | Align pre-commit versions and environments with uv | Sync only | Reads or exports versions already resolved by uv | Reads configuration / reads lock | Syncs YAML / out of scope | Out of scope | Python hook dependencies; upstream marks the project inactive |
| [dlister](https://pypi.org/project/dlister/1.3.0/) | 1.3.0 | List selected Python requirements | Check/list only; never discovers or writes newer versions | Out of scope | Reads manifest / out of scope | Out of scope | Out of scope | Python only |
| [uppd](https://pypi.org/project/uppd/1.6.0/) | 1.6.0 | Update Python requirement constraints | Native for configured operators | Out of scope | Native manifest / out of scope | Out of scope | Out of scope | Python only |
| [uvu](https://github.com/rafaelsq/uvu/tree/v1.0.0) | 1.0.0 | Interactive Python dependency review using uv | Native interactive selection of direct dependencies | Delegated to uv | Native manifest / delegated | Out of scope | Out of scope | Python only |
| [uv-upx](https://pypi.org/project/uv-upx/0.4.3/) (`uv_upgrade`) | 0.4.3 | Raise Python constraints from uv's resolved versions | Native supported lower bounds; skips exact pins and unsupported constraints | Delegated to uv, including workspaces | Native manifest / delegated | Out of scope | Out of scope | Python only |

### upd format details

`upd` discovers Python requirement files and `pyproject.toml`, `package.json`,
`Cargo.toml`, `go.mod`, `Gemfile`, NuGet project files, Terraform files, GitHub
Actions workflows, `.pre-commit-config.yaml`, Mise/asdf files, and explicitly
annotated version pins. See [Ecosystems](ecosystems.md) for the exact file and
syntax coverage.

`--lock` runs the relevant package manager only after a manifest change. See
[Commands run by `--lock`](stability.md#commands-run-by---lock). Security audit
reads supported lockfiles and can propose direct-manifest floors or targeted
lockfile fixes for vulnerable transitive packages; that is distinct from the
ordinary latest-version update pass.

## Operational matrix

| Tool | Preview or check-only | Applies locally | Interactive | Configuration and filtering | CI / automation | Runtime and installation | Parallelism or performance focus |
|---|---|---|---|---|---|---|---|
| **upd** | Dry-run by default; `--check`; JSON | `--apply`; optional `--lock` | `--interactive` | `.updrc.toml`; path, ecosystem, package, bump, age, ignore, and pin filters | Stable exit codes, JSON/SARIF, pre-commit hook, rolling GitHub PR and GitLab MR workflows | Rust binary; crates.io, PyPI wheel, or release binary | Eight files concurrently, parallel registry work, 24-hour version cache |
| Renovate | Dry-run modes and dependency dashboard | Branches/commits through its repository workflow | Dashboard approval rather than terminal UI | Extensive repository/global configuration, presets, schedules, and package rules | Core use case: hosted or self-hosted update automation | Node.js CLI, container, or hosted app | Concurrent extraction/lookups; optimized for repository automation rather than one local edit |
| uv | `uv lock --check`; dry-run options on applicable commands | Project, lockfile, and environment operations | No dependency-by-dependency upgrade UI | `pyproject.toml`, `uv.toml`, CLI selectors, indexes, constraints | CI-friendly lock and sync checks | Rust binary, installer, package managers, or PyPI | Explicit performance focus, parallel downloads, resolver and artifact caches |
| prek | `prek update --check` | `prek update` | Hook execution UI, not dependency selection | YAML or `prek.toml`; update rules and cooldown | Git hooks and CI are the core use case | Rust binary or Python package | Explicit performance focus; concurrent hooks, environments, and workspace projects |
| pinact | `run -check` or `-fix=false`; SARIF | `run`, `run -update` | No | `.pinact.yaml`; include/exclude rules, ref verification, minimum age | Exit codes, SARIF, and companion GitHub Action | Go binary, package managers, release archive, or container | Network-bound GitHub API work; no comparative performance claim |
| actions-up | `--dry-run` or `--json` | `--yes` or interactive approval | Default mode | CLI directories, recursion, excludes, branches, bump mode, and output style | JSON plus companion GitHub Action | Node.js package via npm/npx | Parallel GitHub lookups; interactive responsiveness is an explicit goal |
| taze | Default report; `--json` | `--write` | `--interactive` | Config file plus include/exclude, ranges, workspaces, maturity, and action options | JSON and deterministic non-interactive mode | Node.js package via npm/npx | Concurrent checks and monorepo support |
| ratchet | `lint`; commands can write to a separate output | `pin`, `update`, `upgrade`, `unpin` | No | CLI parser selection, exclusions, original-constraint annotations, bake delay | CLI/container designed for CI pipelines | Go binary, release archive, Homebrew, container, or Nix | Network-bound resolver; no comparative performance claim |
| Dependabot CLI | Job output can be recorded without creating PRs | Produces update-job file changes inside containers | No | Job YAML, ecosystem, directory, credentials, provider, and image selection | Designed for self-hosted Dependabot jobs | Go launcher plus Docker updater/proxy images | Container startup and ecosystem-native resolution dominate local CLI overhead |
| PDM | `pdm outdated`; lock checks; `--dry-run` where supported | `add`, `update`, `lock`, and `sync` | No dependency-by-dependency upgrade UI | `pyproject.toml`, repository sources, groups, update strategies, and plugins | Lock/sync commands and stable CLI suitable for CI | Python application or standalone distribution | Resolver and package cache; optional uv backend |
| pin-github-action | No general dry-run; enforcement is delegated to another action | Default command rewrites workflow files | No | CLI paths, recursion, allow patterns, and comment template | Intended for repeatable CI use | Node.js package or container | Sequential workflow transformation and GitHub API resolution |
| tsvikas/sync-with-uv | `--diff` | Default command | No | `pyproject.toml` mappings and custom paths | Pre-commit hook or CLI | Python package via uv/PyPI | Local lock/config processing; no registry requests |
| dribia/sync-with-uv | Run under pre-commit and inspect its diff | Default hook | No | CLI skip list, package mapping database, and custom paths | Pre-commit hook | Python package | Local lock/config processing; no registry requests |
| sync-pre-commit-with-uv | Run under pre-commit and inspect its diff | Default hook | No | `pyproject.toml` mappings and uv group export options | Pre-commit hook | Python package | Local uv export/config processing; project is not actively developed |
| dlister | Its only operation is read-only listing | No | No | Input, output, requirement group, operator, and skip selectors | Scriptable text output | Python package | Local TOML parsing; no registry requests |
| uppd | `--dry-run` | Default command or separate output file | No | `[tool.uppd]`, operator, package, prerelease, and index selectors | Scriptable CLI | Python package | Registry-bound Python CLI; no explicit parallelism claim |
| uvu | Review is shown before each accepted change | Through interactive flow | Required | Direct-dependency filtering | Primarily local, human-driven use | Python package installed from Git | Delegates resolution and locking performance to uv |
| uv-upx | Command offers normal CLI diagnostics; no separate comparison-grade check mode documented | Default upgrade command | No | CLI and workspace discovery; constraint shapes determine eligibility | Scriptable CLI | Python package via uv/PyPI | Delegates resolution and locking performance to uv |

## Benchmarks

Benchmarks are grouped by equivalent workload. A dash means the tool is outside
that workload, not that it failed.

The committed harness lives in [`benchmarks/`](../benchmarks/README.md). Before
timing, it rejects outputs that do not perform every intended update or leave
parseable files. It then restores every fixture before every measured command,
pins tool versions, uses non-interactive commands, and records raw Hyperfine
JSON. Registry and GitHub requests are live, so the numbers are a reproducible
procedure and a dated observation rather than a permanent speed guarantee.

### Results

The synthetic dataset contains 18 dependency references across two files: 12
Python requirements and 6 GitHub Actions, totaling 49 lines and 980 bytes.
These are five-run means from the environment and commands recorded in the
[detailed 2026-08-28 report](../benchmarks/results/2026-08-28.md); standard
deviations, ranges, limitations, and raw JSON are available there.

| Workload | Tool | Check mean | Update mean |
|---|---|---:|---:|
| Python manifest | **upd** | **134.7 ms** | **138.7 ms** |
| Python manifest | uppd | 269.7 ms | 250.8 ms |
| GitHub Actions | **upd** | **758.7 ms** | 746.3 ms |
| GitHub Actions | pinact | 4,132.8 ms | 4,105.4 ms |
| GitHub Actions | actions-up | 3,529.7 ms | 3,864.0 ms |
| GitHub Actions | taze | 1,019.0 ms | **566.3 ms** |
| GitHub Actions | ratchet | 1,029.1 ms | 1,066.8 ms |

What this particular run shows:

- In the Python-manifest cohort, `upd` recorded a 50% lower check mean and a 45%
  lower update mean than `uppd` on this five-run sample.
- In the GitHub Actions cohort, `upd` recorded the lowest check mean; taze
  recorded the lowest update mean, with `upd` second. Both preserve tag style.
  The other tools write SHA-pinned output, so latency is not the only selection
  criterion.

The benchmark contains two cohorts:

1. **Python manifest constraints:** `upd` and `uppd` inspect and update the same
   exact direct requirements in `pyproject.toml`. uv, PDM, uvu, and uv-upx are
   excluded because the comparable command also resolves a lockfile, installs
   an environment, requires interaction, or intentionally skips exact pins.
   `uppd` uses its separate-output mode because its default in-place command
   produced malformed TOML for this fixture on the recorded environment.
2. **GitHub Actions release checks:** `upd`, pinact, actions-up, taze, and ratchet
   inspect the same workflow references. Update output styles differ—tag
   preservation versus SHA pinning—so results are reported as a cohort, not a
   claim of byte-for-byte equivalent output.

Renovate and Dependabot CLI are excluded from timing because their normal job
includes repository checkout, configuration, container, branch, and PR-operation
work. The synchronization tools and dlister are excluded because they perform
no upstream release lookup.

## Maintenance

When updating this page:

1. Pin new tool versions in [`benchmarks/versions.env`](../benchmarks/versions.env).
2. Verify every changed capability against the linked project's documentation.
3. Run the harness from a clean checkout and commit its raw JSON plus a dated
   result summary.
4. Update the snapshot date and `upd` version above.
