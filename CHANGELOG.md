# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).






## [0.1.0](https://github.com/rvben/upd/compare/v0.0.28...v0.1.0) - 2026-04-21

### Breaking Changes

- **cli**: rename --bump to --only-bump and add --max-bump ([eb63589](https://github.com/rvben/upd/commit/eb63589867bac483b5de313d413d7c8e22a00a5f))
- **cli**: lock CLI surface for 0.1.0 ([d7a3ea4](https://github.com/rvben/upd/commit/d7a3ea441836e266c9ca3c3b772026246ba07d2f))

### Added

- **audit**: add SARIF 2.1.0 output for audit results ([d6b0118](https://github.com/rvben/upd/commit/d6b01188862bef90550814269df21c32f1588a50))
- **audit**: cache OSV responses and add --offline mode ([5a3058b](https://github.com/rvben/upd/commit/5a3058b39d97c4a116eefde65265bdfe354d263d))
- **audit**: add --fix-audit to bump packages to minimum safe version ([5292ae2](https://github.com/rvben/upd/commit/5292ae264b8f076c6b170f5eba5788e9d7eb56da))
- **cli**: rename --bump to --only-bump and add --max-bump ([eb63589](https://github.com/rvben/upd/commit/eb63589867bac483b5de313d413d7c8e22a00a5f))
- **cli**: scope no-args to VCS root and require --apply to mutate ([fe99418](https://github.com/rvben/upd/commit/fe99418b4844fa6c6944644e47982518a3f8616b))
- **audit**: normalize severity labels and sort by severity ([940f25c](https://github.com/rvben/upd/commit/940f25c0286deb5bb72d59cd08bec5ec6a34577e))
- **cli**: route errors to stderr and add --quiet flag ([0cbc19c](https://github.com/rvben/upd/commit/0cbc19c30f0c98a2683434c2f6b6f9f1cb9be615))
- **cli**: add --package filter to restrict updates by name ([f7962c8](https://github.com/rvben/upd/commit/f7962c8b1333a2da2133aacdc89f6f8318d0eb4e))
- **config**: warn on unknown keys and add --show-config ([cab49c1](https://github.com/rvben/upd/commit/cab49c18eb0ff1fd19f1e579959dc9ca3a555617))
- **lock**: regenerate packages.lock.json and .terraform.lock.hcl ([87d8e4e](https://github.com/rvben/upd/commit/87d8e4e9f7ea4e13ad0a5d4e4244384eae48b779))
- **audit**: include .NET packages via OSV NuGet ecosystem ([caec69d](https://github.com/rvben/upd/commit/caec69de65ae61f0923e19f1ba264031cc512365))
- **cli**: add --format json for machine-readable output ([f9c867f](https://github.com/rvben/upd/commit/f9c867fc497ed53e6d6997bb84660b40d851469a))

### Fixed

- **cli**: reject unknown subcommands instead of silent no-op ([e28aea4](https://github.com/rvben/upd/commit/e28aea44b783190f002a3453a1fc21ceff23c882))
- **terraform**: handle registry.terraform.io prefixed sources ([6d90d11](https://github.com/rvben/upd/commit/6d90d1175ab25b35d81dfff791329d5da8b34d8d))
- **cli**: print revert tip in --help and post-run summary ([05cdd14](https://github.com/rvben/upd/commit/05cdd14a5de31fc0a9533f6d6454bb5cb5b8c6d4))
- **lockfile**: error on missing tool, skip when no lockfile exists ([f8cca78](https://github.com/rvben/upd/commit/f8cca785f8a365ee7240cc60236b92387253afdb))
- **cli**: accept comma-separated values for --lang ([c7f8b11](https://github.com/rvben/upd/commit/c7f8b11564b872270747f1cb88b2dbb988060bf3))
- **main**: exit 1 on --dry-run with pending updates ([eb3cadc](https://github.com/rvben/upd/commit/eb3cadc79f03f33f5a9ce5cc26ecec74c804b103))
- **audit**: exit 3 on vulnerabilities, add --no-fail ([28e8b75](https://github.com/rvben/upd/commit/28e8b75ad7b9ff15f33dfd56c5a8270e3dc1696b))
- **main**: exit 2 on errors, structure JSON error objects ([353e013](https://github.com/rvben/upd/commit/353e013988cb43bd66544246e1dca0a5132d4263))
- **version**: keep pre-releases on pre-release-pinned packages ([a95d2f8](https://github.com/rvben/upd/commit/a95d2f85c4143cc913266df774bda3fe35a0a4d3))
- **terraform**: keep ~> constraint when latest still satisfies ([e869e40](https://github.com/rvben/upd/commit/e869e40f99ca88cda873556cfdaff06c44b8de53))
- **audit**: include Go pseudoversion dependencies ([e051f06](https://github.com/rvben/upd/commit/e051f0621a88751059f83707bc415df359b15905))
- **interactive**: require TTY for --interactive mode ([ba0d0b2](https://github.com/rvben/upd/commit/ba0d0b2e2bb7d021ea557bf547bade3be5953379))
- **updater**: refuse to write version downgrades ([41bd7e6](https://github.com/rvben/upd/commit/41bd7e67d03d48cb2f948770abc7ee4979205f9e))
- **requirements**: skip update when current is not valid PEP 440 ([4e6f3ea](https://github.com/rvben/upd/commit/4e6f3ea755d974392915e3fe211b6e0f9e6c3121))
- **audit**: preserve package-name case for OSV queries ([8bde8b1](https://github.com/rvben/upd/commit/8bde8b1bc81aba56a43049d5fac46016195d7eac))
- **rubygems**: skip yanked versions when selecting latest ([2d48a0e](https://github.com/rvben/upd/commit/2d48a0ebcce2c576ca0169f661f27bd4a268a18c))

## [0.0.28](https://github.com/rvben/upd/compare/v0.0.27...v0.0.28) - 2026-04-17

### Added

- **updater**: recursive hidden-file discovery, precise line numbers, scoped npm ([5fcc5d8](https://github.com/rvben/upd/commit/5fcc5d818d349abd109ae7cac001972a6a9cadea))

### Fixed

- **package_json**: index dependencies when opening brace starts on its own line ([e40c3f1](https://github.com/rvben/upd/commit/e40c3f1bf736ef3ea0c565047d886ff7543d37c9))
- **update**: check mode exits 1 when only configured pins differ ([33a69f5](https://github.com/rvben/upd/commit/33a69f5a16ee03247a13c41bfabe1935d09bfa64))
- **updater**: classify configured pins as pins, not updates ([571a96b](https://github.com/rvben/upd/commit/571a96b9de72fe283c5114e594da72687a67efab))

## [0.0.27](https://github.com/rvben/upd/compare/v0.0.26...v0.0.27) - 2026-04-15

### Fixed

- **align**: use pep440_rs for Python stable-version check ([7f132b3](https://github.com/rvben/upd/commit/7f132b351cdd9225a31df96d2a421c8c42926987))
- **version**: use PEP 440 release segments for precision matching ([fff041d](https://github.com/rvben/upd/commit/fff041d2d2e9508f117012a6bfc857ee57e5cd20))

## [0.0.26](https://github.com/rvben/upd/compare/v0.0.25...v0.0.26) - 2026-04-15

### Fixed

- **pypi**: rewrite HTML Simple API parser to handle multi-line anchor tags ([f9c937b](https://github.com/rvben/upd/commit/f9c937be297112e0556cae205f8c0f3ce54997f4))

## [0.0.25](https://github.com/rvben/upd/compare/v0.0.24...v0.0.25) - 2026-04-15

### Fixed

- **pypi**: handle string-valued yanked field in PEP 691 JSON Simple API ([b17034b](https://github.com/rvben/upd/commit/b17034b540f6a5b62e446131c6e87f43695bbd9b))

## [0.0.24] - 2026-03-23

### Added

- **NuGet/.NET support**: Update `PackageReference` and `PackageVersion` elements in `.csproj` and `Directory.Packages.props` files via the NuGet v3 API
- **Gemfile.lock regeneration**: `--lock` flag now supports Ruby projects (runs `bundle install`)

## [0.0.23] - 2026-03-23

### Added

- **Pre-commit support**: Update hook versions in `.pre-commit-config.yaml` via GitHub releases
- **Ruby Gemfile support**: Update gem versions with RubyGems registry and pessimistic constraint (`~>`) support
- **Mise/asdf support**: Update tool versions in `.mise.toml` and `.tool-versions` for 24+ mapped dev tools
- **Terraform/OpenTofu support**: Update provider and module versions in `.tf` files via the Terraform Registry API

### Fixed

- All updaters now use safe HashMap lookups (no panics on edge cases)
- Version replacement no longer clobbers inline comments
- Duplicate registry lookups deduplicated across all updaters

## [0.0.22] - 2026-03-23

### Added

- **Pre-commit support**: Update hook versions in `.pre-commit-config.yaml`
  - Reuses GitHub releases API for version lookups
  - Skips local, meta, and non-GitHub repos
  - Filter with `--lang pre-commit`
- **Ruby Gemfile support**: Update gem versions in `Gemfile`
  - New RubyGems registry with pessimistic constraint (`~>`) support
  - Preserves version operators (`~>`, `>=`, exact)
  - Filter with `--lang ruby`
- **Mise/asdf support**: Update tool versions in `.mise.toml` and `.tool-versions`
  - Maps 24+ common dev tools to GitHub releases (node, python, go, rust, zig, deno, bun, uv, ruff, etc.)
  - Skips `latest` and `cargo:*` entries
  - Filter with `--lang mise`

### Fixed

- All updaters now use safe HashMap lookups (no panics on edge cases)
- Version replacement no longer clobbers inline comments
- Duplicate registry lookups deduplicated across all updaters

## [0.0.21] - 2026-03-23

### Added

- **GitHub Actions support**: Update action version references in `.github/workflows/*.yml` files
  - Preserves version precision (`@v4` stays major-only, `@v4.1.0` stays exact)
  - Skips SHA-pinned actions, branch refs, local actions, and Docker references
  - Authentication via `GITHUB_TOKEN` or `GH_TOKEN` for higher rate limits
  - Filter with `--lang actions`, works with all existing flags

### Fixed

- Rate limit and access denied errors now include hints about setting authentication tokens
- Fixed potential panic in align command for path-based file types

## [0.0.20] - 2025-12-19

### Fixed

- **PyPI registry URL format**: Corrected default PyPI base URL from `https://pypi.org/pypi` to `https://pypi.org`
  - Fixed Simple API URL construction: now correctly uses `https://pypi.org/simple/{package}/` instead of malformed `https://pypi.org/pypi/simple/{package}/`
  - Fixed "Package exists but has no suitable versions" errors for valid packages
  - Resolves CloudFlare challenge page responses that prevented package lookups
  - Particularly affects packages in PEP 735 dependency-groups sections

## [0.0.19] - 2025-12-19

### Fixed

- **Improved error messages for common failures**:
  - HTTP client creation failures now explain TLS/SSL configuration issues
  - HTTP errors categorized by status code (401, 403, 404, 429, 5xx)
  - Registry-specific credential hints for authentication errors (PyPI, npm, crates.io, Go)
  - TOML parsing errors now include file path and line numbers
  - "Package not found" (404) distinguished from "no versions available" (yanked/pre-release only)
  - Config file errors (`--config` flag) now show detailed messages instead of silent failure

## [0.0.18] - 2025-12-18

### Added

- **Configuration file support** (`.updrc.toml`, `upd.toml`, `.updrc`)
  - `ignore`: List of packages to skip during updates
  - `pin`: Map of packages to pinned versions (bypasses registry lookup)
  - Config file discovery walks up directory tree to find project root config
  - Use `--config` flag to specify a custom config file path
- **Enhanced private registry authentication**:
  - PyPI: Read `pip.conf` / `pip.ini` for `index-url` and `extra-index-url`
  - npm: Support for scoped registries in `.npmrc` (`@scope:registry=...`)
  - Cargo: Read `~/.cargo/config.toml` for custom registry URLs
  - Go: Support `GOPRIVATE`, `GONOPROXY`, `GONOSUMDB` environment variables

### Changed

- Verbose output now shows ignored and pinned packages
- Summary output shows counts of pinned and ignored packages

## [0.0.17] - 2025-12-17

### Added

- Pre-commit hook support via `.pre-commit-hooks.yaml`
  - `upd-check`: Fail if any dependencies are outdated
  - `upd-check-major`: Fail only on major (breaking) updates
- Lockfile regeneration with `--lock` flag:
  - `Cargo.lock` via `cargo generate-lockfile`
  - `go.sum` via `go mod tidy`
  - `bun.lockb` via `bun install`
  - `package-lock.json` via `npm install`
  - `poetry.lock` via `poetry lock`

## [0.0.16] - 2025-12-17

### Added

- Parallel file processing for faster updates across multiple files
- `--lock` flag to regenerate lockfiles after updating dependencies

### Fixed

- CLI description now mentions Rust and Go ecosystems

## [0.0.15] - 2025-12-17

### Added

- Private registry compatibility improvements for enterprise PyPI servers
- Better handling of non-standard Simple API responses

## [0.0.14] - 2025-12-17

### Fixed

- Skip yanked packages when fetching versions from Simple API responses
- Prevents updates to withdrawn/yanked package versions

## [0.0.13] - 2025-12-17

### Added

- Simple API fallback for private PyPI servers that don't support JSON API
- Automatic detection and parsing of HTML Simple API responses

## [0.0.12] - 2025-12-17

### Fixed

- Normalize Simple API URLs to JSON API format for consistent handling
- Better URL handling for various private PyPI server configurations

## [0.0.11] - 2025-12-17

### Added

- `UV_EXTRA_INDEX_URL` and `PIP_EXTRA_INDEX_URL` environment variable support
- Query multiple package indexes when primary index doesn't have the package

## [0.0.10] - 2025-12-17

### Added

- `audit` subcommand for security vulnerability scanning via OSV (Open Source Vulnerabilities) API
  - Scans all dependency files for known vulnerabilities
  - Supports all ecosystems: PyPI, npm, crates.io, Go
  - Shows CVSS severity scores, descriptions, and fixed versions
  - Batch queries for efficiency (up to 1000 packages per request)
  - Parallel fetching of vulnerability details for performance
  - Use `--check` flag for CI integration (exit 1 if vulnerabilities found)
  - Use `--lang` to filter by ecosystem

## [0.0.9] - 2025-12-11

### Added

- Private repository authentication support for all registries:
  - PyPI: Basic Auth via environment variables, `~/.netrc`, or inline URL credentials
  - npm: Bearer token via `NPM_TOKEN`, `NODE_AUTH_TOKEN`, or `.npmrc`
  - Cargo: Token via `CARGO_REGISTRY_TOKEN` or `~/.cargo/credentials.toml`
  - Go: Basic Auth via `GOPROXY_USERNAME`/`GOPROXY_PASSWORD` or `~/.netrc`
- Inline index URL support in `requirements.txt` (`--index-url`, `-i`)

### Fixed

- Upper-bound-only constraints like `django<6` are now skipped (not incorrectly narrowed)
- Constraints with upper bounds now preserve the upper bound during updates
  - `django>=4.0,<6` → `django>=5.2,<6` (previously dropped the `<6`)

## [0.0.8] - 2025-12-10

### Added

- `--check` / `-c` flag for CI integration (exit code 1 if updates available)
- Interactive mode (`-i`) for approving updates one by one

## [0.0.7] - 2025-12-10

### Added

- `align` subcommand to align package versions across multiple files in a monorepo

## [0.0.6] - 2025-12-09

### Added

- `--lang` / `-l` flag to filter updates by language/ecosystem
- Update type filters: `--major`, `--minor`, `--patch`

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

[0.0.18]: https://github.com/rvben/upd/releases/tag/v0.0.18
[0.0.17]: https://github.com/rvben/upd/releases/tag/v0.0.17
[0.0.16]: https://github.com/rvben/upd/releases/tag/v0.0.16
[0.0.15]: https://github.com/rvben/upd/releases/tag/v0.0.15
[0.0.14]: https://github.com/rvben/upd/releases/tag/v0.0.14
[0.0.13]: https://github.com/rvben/upd/releases/tag/v0.0.13
[0.0.12]: https://github.com/rvben/upd/releases/tag/v0.0.12
[0.0.11]: https://github.com/rvben/upd/releases/tag/v0.0.11
[0.0.10]: https://github.com/rvben/upd/releases/tag/v0.0.10
[0.0.9]: https://github.com/rvben/upd/releases/tag/v0.0.9
[0.0.8]: https://github.com/rvben/upd/releases/tag/v0.0.8
[0.0.7]: https://github.com/rvben/upd/releases/tag/v0.0.7
[0.0.6]: https://github.com/rvben/upd/releases/tag/v0.0.6
[0.0.5]: https://github.com/rvben/upd/releases/tag/v0.0.5
[0.0.4]: https://github.com/rvben/upd/releases/tag/v0.0.4
[0.0.3]: https://github.com/rvben/upd/releases/tag/v0.0.3
[0.0.2]: https://github.com/rvben/upd/releases/tag/v0.0.2
[0.0.1]: https://github.com/rvben/upd/releases/tag/v0.0.1
