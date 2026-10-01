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

## Under a cooldown

With an active `[cooldown]`, `--min-age` or `--min-age-floor`, a `uv.lock` is
refreshed the way `upd update --lock` gates uv (see
[Lockfiles](configuration.md#lockfiles)): `uv lock --upgrade`, or the
`--upgrade-package` form, runs with `--exclude-newer` at the cooldown's cutoff
and every locked package younger than it exempted at its own upload time, so
nothing is moved back. The cutoff uv records in `uv.lock` is removed and a
plain `uv lock` confirms the result.

Maintenance never drops the cooldown to finish a refresh. The lockfile is put
back and the refresh fails, naming the reason, when:

- uv predates `--exclude-newer-package`, or the project or user configuration
  (or `UV_EXCLUDE_NEWER`) sets its own `exclude-newer`, which the gate would
  override;
- the gated resolution fails, for example because an index lists no upload
  times, or would move a locked package back;
- the confirming plain `uv lock` moves anything;
- reading the result back finds an entry the refresh introduced that was
  published inside the cooldown (an exemption admits every release of its
  package up to the exempted time, so `--upgrade` can reach a young one), or
  one whose publish date upd cannot establish.

Publish dates are read as for `upd update --lock`: from the indexes upd reads,
through their JSON API. An entry from an index that only serves the simple API,
from `find-links`, or from any index upd does not read cannot be dated, so a
refresh that introduces one fails.

The cooldown selects versions by when they were released. A file uploaded
later to a release that is already outside the cooldown, such as a new wheel,
can still be locked, as with upd's release-age rule for manifests.

npm and Cargo lockfiles are refused under a cooldown before the package
manager runs.

## Options

The no-write mode lists candidates; it does not predict the package-manager
resolution. Use `--apply` in a clean Git checkout to review the resulting diff.
`--only-bump` and `--package` are not yet supported
for maintenance (nor, with them, `--strict-bump`); `--lang` and `--max-bump`
are supported.
`--check`, `--interactive`, and `--no-ignore` are also unsupported by this
command; it rejects them rather than silently changing their meaning.
`--limit` and `--offset` select which discovered lockfiles to process;
`--fields` narrows JSON output fields.
