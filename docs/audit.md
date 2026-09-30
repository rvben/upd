# Security auditing

Check your dependencies for known security vulnerabilities using the
[OSV (Open Source Vulnerabilities)](https://osv.dev/) database.

```bash
upd audit              # Scan all dependency files (exit 6 if vulnerabilities found)
upd audit --dry-run    # Same as audit (read-only operation)
upd audit --no-fail    # Report vulnerabilities but exit 0
upd audit --lang python # Audit only Python packages
upd audit ./services   # Audit specific directory

# Auto-fix: bump each vulnerable package to the minimum safe version
# (max of fixed_version across all its vulnerabilities). Packages with
# no fixed_version are reported but left untouched.
upd audit --fix-audit --apply

# Auto-fix and refresh the affected lockfiles (e.g. go.sum, Cargo.lock)
upd audit --fix-audit --apply --lock

# Offline mode: use only cached OSV responses; cache misses are errors
upd audit --offline

# SARIF 2.1.0 output for GitHub Code Scanning
upd audit --format sarif > results.sarif
```

**Supported ecosystems for auditing:** PyPI, npm, crates.io, Go, RubyGems, NuGet,
and Maven through Gradle lockfiles.

Gradle auditing reads adjacent `gradle.lockfile` and `buildscript-gradle.lockfile`
files and reports coverage limitations. Automatic Maven fixes are not supported;
see [Maven audit coverage](ecosystems.md#maven-audit-coverage).

An advisory's fixed version is the edge of its affected range, which is not
always a release. RustSec's notices for unmaintained crates, for one, name a
version one past the last release (`0.4.21-0` for a crate whose last release is
`0.4.20`). An advisory can also cover several branches, so the release just
above the first fixed version may still sit inside a later affected range.

Before `--fix-audit` writes a fix for a PyPI, npm or crates.io package, it
asks the registry the package resolves from which releases exist: the index
or registry its lockfile records, when that is one upd is configured with
(`UV_INDEX_URL` and `UV_EXTRA_INDEX_URL` or their pip equivalents, the npm
registry or the one `.npmrc` names for the package's scope, and crates.io or
the index `CARGO_REGISTRIES_CRATES_IO_INDEX` or Cargo's `registry.default`
names).
The fix moves to the lowest release, at or above the advisory's fixed
version, that none of the package's advisories still covers: the fixed
version itself when it is published, else a stable release above it. For
crates.io and PyPI the release must also be installable, so a yanked release
is passed over, and so is a PyPI release whose files have all been deleted.
An advisory range's `limit` ends every affected window in that range.

- When every such release is still affected, the package is reported
  `unfixable`, and the reason names the lowest release and the advisory that
  covers it.
- When no such release is published, the package is reported `unfixable`,
  the same as an advisory with no fixed version. Writing the unpublished
  version would make the directory's relock fail, which rolls back every
  other fix in that directory too.

The fix goes to the version the advisory names, with a `note:` on stderr
saying it was not confirmed, when no configured registry can vouch for the
package's releases:

- the lockfile records an index or registry upd is not configured with;
- the lockfiles record the same package and version from more than one
  registry;
- a pin no lockfile records sits in a `pyproject.toml` or requirements file
  that declares its own package index, or upd is configured with more than
  one Python index, so it could resolve from any of them;
- the registry cannot be reached, or lists no release of the package;
- the run is `--offline`, which asks no registry at all.

Go modules and the other ecosystems use the advisory's version as named.

## Example output

```text
Checking 42 unique package(s) for vulnerabilities...

⚠ Found 3 vulnerability/ies in 2 package(s):

  ● requests@2.19.0 (PyPI)
    ├── GHSA-j8r2-6x86-q33q [CVSS:3.1/AV:N/AC:H/PR:N/UI:R/S:C/C:H/I:N/A:N] Unintended leak of Proxy-Authorization header
    │   Fixed in: 2.31.0
    │   https://github.com/psf/requests/security/advisories/GHSA-j8r2-6x86-q33q

  ● flask@0.12.2 (PyPI)
    ├── GHSA-562c-5r94-xh97 [CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H] Denial of Service vulnerability
    │   Fixed in: 0.12.3
    │   https://nvd.nist.gov/vuln/detail/CVE-2018-1000656
    ├── GHSA-m2qf-hxjv-5gpq [CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N] Session cookie disclosure
    │   Fixed in: 2.3.2
    │   https://github.com/pallets/flask/security/advisories/GHSA-m2qf-hxjv-5gpq

Summary: 2 vulnerable package(s), 3 total vulnerability/ies
```

## CI integration

```yaml
# GitHub Actions example: fail the build on vulnerabilities
- name: Check for vulnerabilities
  run: upd audit   # non-zero exit (6) fails the build when vulnerabilities are found

# Capture the audit status so SARIF is uploaded even when findings make upd exit 6
- name: Audit dependencies (SARIF)
  id: audit
  shell: bash
  run: |
    set +e
    upd audit --format sarif > results.sarif
    audit_exit=$?
    set -e
    test -s results.sarif
    echo "exit-code=$audit_exit" >> "$GITHUB_OUTPUT"
- name: Upload to Code Scanning
  if: always()
  uses: github/codeql-action/upload-sarif@v4
  with:
    sarif_file: results.sarif
- name: Enforce audit result
  if: always()
  env:
    AUDIT_EXIT: ${{ steps.audit.outputs.exit-code }}
  run: test "$AUDIT_EXIT" = 0
```

Grant the job `contents: read` and `security-events: write`. In production,
pin third-party Actions to an immutable commit SHA, as this repository does in
its own security workflow. Fork pull requests receive a read-only token, so
their SARIF upload step should be skipped while the audit itself still runs.

## See also

- [Stability](stability.md#stable-exit-codes) for the exit-code contract, including `6`
- [Stability](stability.md#commands-run-by---lock) for what `--lock` runs per ecosystem
- [GitHub security remediation](github-actions.md#security-remediation-pull-requests) for validated, rolling vulnerability-fix pull requests
- [GitHub pull requests](github-actions.md#quick-start) for scheduled dependency freshness updates
