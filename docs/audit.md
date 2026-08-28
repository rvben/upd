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

**Supported ecosystems for auditing:** PyPI, npm, crates.io, Go, RubyGems, NuGet

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
- [GitHub pull requests](github-actions.md#quick-start) for scheduled dependency updates
