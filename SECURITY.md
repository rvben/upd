# Security

## Supported versions

`upd` is a pre-1.0 project. Security fixes are applied to the latest released
version; older `0.x` releases do not receive backports unless a separate support
policy is announced.

## Report a vulnerability privately

Do not open a public issue for a suspected vulnerability.

Use [GitHub private vulnerability reporting](https://github.com/rvben/upd/security/advisories/new)
or email the maintainer at <ruben.jongejan@gmail.com>. Include the affected
version, operating system, smallest safe reproduction, expected impact, and any
relevant logs with credentials and private registry URLs redacted.

Reports are handled privately until a fix or mitigation is available. A
disclosure timeline and credit will be coordinated with the reporter.

## Security model

`upd` reads dependency manifests, queries upstream registries, and can rewrite
project files. Its security boundaries are:

- A normal `upd` run is read-only. File changes require the explicit `--apply`
  flag, and the default scan is scoped to the nearest Git repository root.
- `--lock` invokes package managers already installed on the machine. Those
  tools may interpret project configuration, so do not use `--lock` on an
  untrusted checkout. The exact commands are documented under
  [Commands run by `--lock`](docs/stability.md#commands-run-by---lock).
- Private-registry credentials may be read from environment variables and
  ecosystem configuration. Never attach credentials, authenticated URLs, or
  private package names to a public report. See
  [Private registries](docs/private-registries.md) for the supported sources.
- `upd audit` reports advisories from OSV and the dependency information it can
  discover. It complements, rather than replaces, ecosystem-native security
  tooling and review of transitive dependency coverage.
- GitHub release archives include SHA-256 sidecars and build provenance
  attestations. Consumers should verify the checksum or attestation when
  installing release binaries directly.

Potential credential disclosure, writes outside the intended project, unsafe
lock-command execution, path traversal, dependency-confusion behavior, forged
GitHub Action pin verification, and release-integrity failures are all treated
as security issues.
