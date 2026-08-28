# upd documentation

Reference material for [`upd`](../README.md). The README covers installing it
and the common invocations; these pages cover everything you look up rather
than read.

| Page | What is in it |
|------|---------------|
| [Ecosystems](ecosystems.md) | Every file `upd` discovers, per ecosystem, plus annotated version pins in files it does not otherwise understand |
| [Comparison](comparison.md) | Dated feature matrix and reproducible, workload-based benchmarks against related tools |
| [GitHub pull requests](github-actions.md) | Rolling freshness and security-remediation PRs, immutable Action SHA pins, validation, and credentials |
| [GitLab merge requests](gitlab.md) | Scheduled rolling dependency MRs, token setup, validation, and opt-in native auto-merge |
| [Configuration](configuration.md) | `.updrc.toml` discovery and keys, cooldown, caching, environment variables |
| [Private registries](private-registries.md) | Credential detection for PyPI, npm, Cargo, Go, and GitHub |
| [Security auditing](audit.md) | OSV scanning, `--fix-audit`, SARIF, CI integration |
| [Security policy](../SECURITY.md) | Supported versions, private vulnerability reporting, trust boundaries, release integrity |
| [Stability](stability.md) | The stable CLI, exit codes, output and configuration guarantees |

For the authoritative machine-readable contract, run `upd schema`.
