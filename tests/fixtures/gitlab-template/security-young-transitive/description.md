<p><a href="https://github.com/rvben/upd"><img src="https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg" alt="upd" width="96"></a></p>

> **A security fix, already prepared.** upd prepared 1 security fix across 1 file and passed proposal-integrity checks. Security fixes move to the lowest release that resolves their advisories, whatever the freshness and bump policy. A fix's relock also locked 1 release inside the freshness window, listed under Needs attention.

**1 security fix** · **1 inside the freshness window**

### Security fixes

upd moved these dependencies to the lowest release that resolves their advisories, outside the freshness and bump policy.

| Dependency | Change | Advisories | Severity | File |
|---|---|---|---|---|
| <code>lodash</code> | <code>4.17.20</code> → <code>4.17.21</code> | <code>GHSA-29mw-wpgm-hmr9</code>, <code>GHSA-35jh-r3h4-6jhm</code> | High | <code>dependency.txt</code> |

### What upd verified

- **Validation:** No project-specific command was configured; proposal integrity passed.
- **Scope:** <code>dependency.txt</code>.
- **Version boundary:** Security fixes move to the lowest release that resolves their advisories, whatever the freshness and bump policy.
- **Policy:** Freshness <code>7d</code>; maximum bump <code>minor</code>; lockfile regeneration on.
- **Security:** 2 advisories resolved; security fixes follow the advisories, not the update policy.

### Needs attention

> A security fix's relock also locked these releases, published inside the freshness window. upd leaves them locked, since holding them back could undo the fix; review them before merging.

| Dependency | Version | Released | Cooldown | File |
|---|---:|---|---|---|
| <code>lodash-helper</code> | <code>2.1.0</code> | <code>2026-09-28T14:02:11Z</code> | 7d | <code>package-lock.json</code> |

<details>
<summary><strong>Proof and provenance</strong></summary>

- Security fixes: 1 (0 awaiting lockfile regeneration)
- Advisories resolved: 2
- Advisories without a fix: 0
- Released inside the freshness window, locked by a fix's relock: 1
- Applied updates: 0
- Change mix: 0 major, 0 minor, 0 patch, 0 revision
- Normalized specifiers: 0
- Dependency annotations: 0
- Held back by policy: 0
- Blocked: 0
- Not examined: 0
- Warnings: 0
- Auto-merge: off
- Full update report, security report and presentation model retained in the pipeline artifact

</details>

> Rebuilt from the latest default branch. The project pipeline remains the final merge boundary.

---
Prepared by [upd](https://github.com/rvben/upd).

<!-- upd-commit: <tip> -->
