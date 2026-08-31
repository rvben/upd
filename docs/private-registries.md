# Private registries

`upd` supports authenticated private package registries for the package
ecosystems listed below. Credentials are automatically detected from
environment variables and configuration files.

Run `upd --verbose` to see when authenticated access is being used:

```bash
upd --verbose
# Output: Using authenticated PyPI access
# Output: Using authenticated npm access
# Output: Using authenticated GitHub access
```

Docker image updates currently support public Docker Hub and public OCI
registries, including their anonymous bearer-token challenge. A registry that
requires user credentials is reported as unsupported instead of silently
falling back or leaking credentials to an untrusted endpoint.

## PyPI / private Python index

```bash
# Option 1: Environment variables
export UV_INDEX_URL=https://my-private-pypi.com/simple
export UV_INDEX_USERNAME=myuser
export UV_INDEX_PASSWORD=mypassword

# Option 2: PIP-style environment variables
export PIP_INDEX_URL=https://my-private-pypi.com/simple
export PIP_INDEX_USERNAME=myuser
export PIP_INDEX_PASSWORD=mypassword

# Option 3: ~/.netrc file
# machine my-private-pypi.com
# login myuser
# password mypassword

# Option 4: pip.conf / pip.ini
# ~/.config/pip/pip.conf (Linux/macOS)
# %APPDATA%\pip\pip.ini (Windows)
[global]
index-url = https://my-private-pypi.com/simple
extra-index-url = https://pypi.org/simple

# Option 5: Inline in requirements.txt (with credentials)
# --index-url https://user:pass@my-private-pypi.com/simple
# or just the URL (credentials from netrc):
# --index-url https://my-private-pypi.com/simple
```

**pip.conf locations** (searched in order):

1. `$PIP_CONFIG_FILE` environment variable
2. `$VIRTUAL_ENV/pip.conf` (if in a virtual environment)
3. `$XDG_CONFIG_HOME/pip/pip.conf` or `~/.config/pip/pip.conf`
4. `~/.pip/pip.conf`
5. `/etc/pip.conf` (system-wide)

**Inline index URLs**: When a `requirements.txt` file contains `--index-url` or `-i`,
`upd` automatically uses that index instead of the default PyPI. An `--extra-index-url`
on its own is added in front of the default index, which is still consulted for
anything the extra index does not carry. Credentials can be embedded in the URL
(`https://user:pass@host/simple`) or looked up from `~/.netrc`.

**Indexes declared in pyproject.toml**: `[[tool.uv.index]]`, `[[tool.poetry.source]]`
and `[[tool.pdm.source]]` entries are honoured with each tool's own semantics.
Declared indexes are consulted in the tool's own order (uv and Poetry: declared
sources before the default index; PDM: the default index first) and only replace the
default where the tool would (uv `default = true`, a Poetry primary source, a PDM
source named `pypi`).
uv `explicit = true` indexes are only used for packages pinned to them in
`[tool.uv.sources]`. PDM `include_packages` / `exclude_packages` globs scope a source
to the packages it names. Poetry `priority = "explicit"` sources are not consulted.
The first index in the order that knows the package answers, as with uv's default
`first-index` strategy: a package that lives on your private index is never bumped to
a version that only exists on a public one, even if that version number is higher.

## npm / private registry

```bash
# Option 1: Environment variables
export NPM_REGISTRY=https://npm.mycompany.com
export NPM_TOKEN=your-auth-token

# Option 2: NODE_AUTH_TOKEN (GitHub Actions)
export NODE_AUTH_TOKEN=your-auth-token

# Option 3: ~/.npmrc file (global registry)
registry=https://npm.mycompany.com
//npm.mycompany.com/:_authToken=your-auth-token
# Or for environment variable reference:
//npm.mycompany.com/:_authToken=${NPM_TOKEN}

# Option 4: ~/.npmrc file (scoped registries)
@mycompany:registry=https://npm.mycompany.com
//npm.mycompany.com/:_authToken=your-auth-token
@another-scope:registry=https://another.registry.com
```

**Scoped registries**: Packages with scopes (e.g., `@mycompany/package`) will use the
registry configured for that scope in `.npmrc`. This allows mixing public and private
packages in the same project.

## Cargo / private registry

```bash
# Option 1: Environment variables
export CARGO_REGISTRY_TOKEN=your-token  # For crates.io default
export CARGO_REGISTRIES_MY_REGISTRY_TOKEN=your-token  # For named registry

# Option 2: ~/.cargo/credentials.toml
[registry]
token = "your-crates-io-token"

[registries.my-private-registry]
token = "your-private-token"

# Option 3: ~/.cargo/config.toml (registry URLs)
[registries.my-private-registry]
index = "https://my-registry.com/git/index"
# or sparse registry:
index = "sparse+https://my-registry.com/index/"
```

**Custom registries**: `upd` reads `~/.cargo/config.toml` to discover custom registry
URLs. Combine with `credentials.toml` for authenticated access.

## Go / private module proxy

```bash
# Option 1: Environment variables
export GOPROXY=https://proxy.mycompany.com
export GOPROXY_USERNAME=myuser
export GOPROXY_PASSWORD=mypassword

# Option 2: Private module patterns
export GOPRIVATE=github.com/mycompany/*,gitlab.mycompany.com/*
export GONOPROXY=github.com/mycompany/*
export GONOSUMDB=github.com/mycompany/*

# Option 3: ~/.netrc file (commonly used with go modules)
# machine github.com
# login myuser
# password mytoken
```

**Private modules**: Set `GOPRIVATE` to specify module patterns that should bypass
the public proxy. `upd` respects these patterns and will attempt direct access
for matching modules.

## GitHub Actions and pre-commit

```bash
# Option 1: GITHUB_TOKEN (automatically available in GitHub Actions)
export GITHUB_TOKEN=ghp_your-token-here

# Option 2: GH_TOKEN (used by the gh CLI)
export GH_TOKEN=ghp_your-token-here
```

Without a token, the GitHub API rate limit is 60 requests/hour. With a token,
it's 5,000 requests/hour.

## See also

- [Configuration](configuration.md#environment-variables) for the full environment variable table
- [Ecosystems](ecosystems.md) for which files each registry backs
