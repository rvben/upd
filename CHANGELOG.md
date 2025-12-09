# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.0.5] - 2025-12-09

### Added

- `--full-precision` flag to output full version numbers instead of matching original precision
- Clickable `file:line:` output format for update messages (recognized by VS Code, iTerm2, and modern terminals)
- Support for Rust `Cargo.toml` dependencies
- Support for Go `go.mod` dependencies
- HTTP retry logic with exponential backoff for transient network errors

### Changed

- Version precision now preserved by default (e.g., `flask>=2.0` stays `2.x` format, not `2.x.y`)
- Removed unused `--verify` flag from CLI

### Fixed

- Output now includes line numbers for each updated dependency

### Testing

- Comprehensive test coverage for all updaters (requirements.txt, pyproject.toml, package.json, Cargo.toml, go.mod)
- Integration tests with MockRegistry for offline testing
- Tests for HTTP retry logic, CLI argument parsing, version classification

## [0.0.4] - 2025-12-08

### Fixed

- Use rustls-tls instead of native-tls for better cross-compilation support
- Update to Rust 1.91.1

## [0.0.3] - 2025-12-08

### Fixed

- CI workflow improvements for cross-platform builds

## [0.0.2] - 2025-12-08

### Added

- Support for Poetry-style `[tool.poetry.dependencies]` in pyproject.toml
- Pre-release version handling (packages with alpha/beta versions update to newer pre-releases)

## [0.0.1] - 2025-12-08

### Added

- Initial release of `upd` - a fast dependency updater written in Rust
- Support for Python dependency files:
  - `requirements.txt` and `requirements-*.txt` patterns
  - `requirements.in` and `requirements-*.in` patterns
  - `pyproject.toml` with `[project.dependencies]` and `[project.optional-dependencies]`
- Support for Node.js dependency files:
  - `package.json` with `dependencies` and `devDependencies`
- Version constraint handling:
  - Respects upper bounds (e.g., `>=2.0,<3` won't update to v3.x)
  - PEP 440 version specifier support for Python
  - Semver range support for npm packages
- Major version bump warnings with `(MAJOR)` indicator
- Pre-release version filtering (excludes alpha, beta, rc versions)
- Dry-run mode (`-n`) to preview changes without modifying files
- Format-preserving updates using `toml_edit` for pyproject.toml
- Gitignore-aware file discovery (respects `.gitignore` patterns)
- Version caching for faster subsequent runs
- Colored terminal output with `--no-color` option
- Self-update command (`upd self-update`)
- Cache management (`upd clean-cache`)

### Performance

- Async HTTP requests with `reqwest`
- Concurrent dependency lookups
- Release binary with LTO optimization

[0.0.5]: https://github.com/rvben/upd/releases/tag/v0.0.5
[0.0.4]: https://github.com/rvben/upd/releases/tag/v0.0.4
[0.0.3]: https://github.com/rvben/upd/releases/tag/v0.0.3
[0.0.2]: https://github.com/rvben/upd/releases/tag/v0.0.2
[0.0.1]: https://github.com/rvben/upd/releases/tag/v0.0.1
