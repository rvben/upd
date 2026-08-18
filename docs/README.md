# upd documentation

Reference material for [`upd`](../README.md). The README covers installing it
and the common invocations; these pages cover everything you look up rather
than read.

| Page | What is in it |
|------|---------------|
| [Ecosystems](ecosystems.md) | Every file `upd` discovers, per ecosystem, plus annotated version pins in files it does not otherwise understand |
| [GitHub Actions](github-actions.md) | SHA-pin safety rules, `blocked` vs `not-examined`, and the reusable pull-request workflow |
| [Configuration](configuration.md) | `.updrc.toml` discovery and keys, cooldown, caching, environment variables |
| [Private registries](private-registries.md) | Credential detection for PyPI, npm, Cargo, Go, and GitHub |
| [Security auditing](audit.md) | OSV scanning, `--fix-audit`, SARIF, CI integration |
| [Stability](stability.md) | The stable CLI, exit codes, output and configuration guarantees |

For the authoritative machine-readable contract, run `upd schema`.
