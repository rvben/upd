<p><a href="https://github.com/rvben/upd"><img src="https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg" alt="upd" width="96"></a></p>

> **A security fix, already prepared.** upd prepared 1 security fix and 1 dependency update across 2 files and passed proposal-integrity checks. Security fixes move to the lowest release that resolves their advisories, whatever the freshness and bump policy. No major-version jumps among ordinary updates.

**1 security fix** · **1 moved forward** · **1 worth a look**

### Security fixes

upd moved these dependencies to the lowest release that resolves their advisories, outside the freshness and bump policy.

| Dependency | Change | Advisories | Severity | File |
|---|---|---|---|---|
| <code>requests</code> | <code>==2.31.0</code> → <code>2.32.0</code> | <code>GHSA-9wx4-h78v-vm56</code> | Medium | <code>requirements.txt</code> |

### Worth a look

Your attention is best spent on these non-patch updates.

| Dependency | Before | After | Change | File |
|---|---:|---:|---|---|
| <code>example</code> | <code>1.0.0</code> | <code>1.1.0</code> | minor | <code>dependency.txt</code> |

### What upd verified

- **Validation:** No project-specific command was configured; proposal integrity passed.
- **Scope:** <code>dependency.txt</code>, <code>requirements.txt</code>.
- **Version boundary:** Security fixes move to the lowest release that resolves their advisories, whatever the freshness and bump policy. No major-version jumps among ordinary updates.
- **Policy:** Freshness <code>7d</code>; maximum bump <code>minor</code>; lockfile regeneration off.
- **Security:** 1 advisory resolved; security fixes follow the advisories, not the update policy.

<details>
<summary><strong>Proof and provenance</strong></summary>

- Security fixes: 1 (0 awaiting lockfile regeneration)
- Advisories resolved: 1
- Advisories without a fix: 0
- Applied updates: 1
- Change mix: 0 major, 1 minor, 0 patch, 0 revision
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
