# Lockfile maintenance

`upd lock-refresh --apply` asks the package manager to move resolved packages
forward within the manifest's existing constraints. It does not rewrite
dependency declarations. This differs from `upd update --apply --lock`, which
relocks after `upd` changes a declaration and often targets only that change.

```console
upd lock-refresh                 # List supported lockfiles; write nothing
upd lock-refresh --apply         # Refresh them and report package changes
upd lock-refresh . --apply -o json
```

With no path, `upd` starts at the nearest Git repository root. An explicit
directory or lockfile path also works. The current implementation handles:

| Lockfile | Command |
| --- | --- |
| `uv.lock` | `uv lock --upgrade`, or repeated `--upgrade-package` for unprotected packages |
| `package-lock.json`, `npm-shrinkwrap.json` | `npm update --package-lock-only --ignore-scripts --no-audit --no-fund` |
| `Cargo.lock` | `cargo update` |

The command reads the resolved packages before and after refresh. It rolls
back the lockfile if an ignored or pinned package changes version or recorded
source, if a single-version package is downgraded, if a `--max-bump` ceiling is exceeded, or
if the package manager or lockfile parser fails. It also checks that the
adjacent manifest was not edited. Failures in one lockfile do not stop other
lockfiles from being attempted, and the command exits nonzero if any failed.

uv locks a whole workspace at its root, so a `uv.lock` inside a workspace
member is not one uv would write: `uv lock` run there rewrites, or creates,
the root's lockfile instead. Before refreshing a `uv.lock`, `upd` asks
`uv workspace dir` for the workspace root and refuses the lockfile, changing
nothing, unless it sits at that root. If uv cannot answer (a release without
that command, or a broken project), the refresh is refused as well.
`UV_PROJECT` and `UV_WORKING_DIR` are removed from every uv invocation, so the
project refreshed is always the one containing the selected lockfile.

The no-write mode lists candidates; it does not predict the package-manager
resolution. Use `--apply` in a clean Git checkout to review the resulting diff.
An active `[cooldown]`, `--min-age` or `--min-age-floor` is currently refused before invoking a
package manager. Cooldown-aware maintenance needs its own release-age checks
for the full resolved graph. `--only-bump` and `--package` are not yet supported
for maintenance (nor, with them, `--strict-bump`); `--lang` and `--max-bump`
are supported.
`--check`, `--interactive`, and `--no-ignore` are also unsupported by this
command; it rejects them rather than silently changing their meaning.
`--limit` and `--offset` select which discovered lockfiles to process;
`--fields` narrows JSON output fields.
