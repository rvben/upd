# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.0.1]: https://github.com/rvben/upd/releases/tag/v0.0.1
