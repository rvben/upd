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

# Upload SARIF results to GitHub Code Scanning
- name: Audit dependencies (SARIF)
  run: upd audit --format sarif > results.sarif
- name: Upload to Code Scanning
  uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: results.sarif
```

## See also

- [Stability](stability.md#stable-exit-codes) for the exit-code contract, including `6`
- [Stability](stability.md#commands-run-by---lock) for what `--lock` runs per ecosystem
- [GitHub Actions](github-actions.md#automated-pull-requests) for scheduled update pull requests
