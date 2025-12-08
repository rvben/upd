# upd

A fast dependency updater for Python and Node.js projects, written in Rust.

## Features

- **Multi-format support**: Updates `requirements.txt`, `pyproject.toml`, and `package.json`
- **Constraint-aware**: Respects version constraints (e.g., `>=2.0,<3` won't update to v3.x)
- **Major version warnings**: Highlights breaking changes with `(MAJOR)` indicator
- **Format-preserving**: Keeps your file formatting, comments, and structure intact
- **Pre-release filtering**: Excludes alpha, beta, and release candidate versions
- **Gitignore-aware**: Respects `.gitignore` patterns when discovering files
- **Fast**: Async HTTP requests with caching for quick subsequent runs

## Installation

### From PyPI

```bash
pip install upd
# or with uv
uv pip install upd
```

### From crates.io

```bash
cargo install upd
```

### From source

```bash
git clone https://github.com/rvben/upd
cd upd
cargo install --path .
```

## Usage

```bash
# Update all dependency files in current directory
upd

# Update specific files or directories
upd requirements.txt pyproject.toml

# Dry-run mode (preview changes without writing)
upd -n
upd --dry-run

# Verbose output
upd -v
upd --verbose

# Disable colored output
upd --no-color

# Disable caching
upd --no-cache
```

### Commands

```bash
# Show version
upd version

# Check for upd updates
upd self-update

# Clear version cache
upd clean-cache
```

## Supported Files

### Python

- `requirements.txt`, `requirements-dev.txt`, `requirements-*.txt`
- `requirements.in`, `requirements-dev.in`, `requirements-*.in`
- `dev-requirements.txt`, `*-requirements.txt`, `*_requirements.txt`
- `pyproject.toml` (`[project.dependencies]` and `[project.optional-dependencies]`)

### Node.js

- `package.json` (`dependencies` and `devDependencies`)

## Example Output

```
pyproject.toml
  Would update requests 2.28.0 → 2.31.0
  Would update flask 2.2.0 → 3.0.0 (MAJOR)

requirements.txt
  Would update pytest 7.2.0 → 7.4.3
  Would update black 23.1.0 → 23.12.1

Would update 4 package(s) (1 major, 2 minor, 1 patch) in 2 file(s), 15 up to date
```

## Version Constraints

`upd` respects version constraints in your dependency files:

| Constraint | Behavior |
|------------|----------|
| `>=2.0,<3` | Updates within 2.x range only |
| `^2.0.0` | Updates within 2.x range (npm) |
| `~2.0.0` | Updates within 2.0.x range (npm) |
| `>=2.0` | Updates to any version >= 2.0 |
| `==2.0.0` | No updates (pinned) |

## Development

```bash
# Build
make build

# Run tests
make test

# Lint
make lint

# Format
make fmt

# All checks
make check
```

## License

MIT
