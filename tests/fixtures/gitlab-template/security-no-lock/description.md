<p><a href="https://github.com/rvben/upd"><img src="https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg" alt="upd" width="96"></a></p>

> **A careful upgrade, with follow-up.** upd prepared 1 security fix across 1 file and passed proposal-integrity checks. It stopped short of 1 dependency it could not change safely.

**1 security fix** · **1 needs attention** · **1 without a fix**

### Security fixes

upd moved these dependencies to the lowest release that resolves their advisories, outside the freshness and bump policy.

| Dependency | Change | Advisories | Severity | File |
|---|---|---|---|---|
| <code>serde&#95;yaml</code> | <code>0.8</code> → <code>0.8.4</code> | <code>RUSTSEC-2024-0001</code> | High | <code>Cargo.toml</code> |

> Lockfile regeneration is off, so 1 fix changes the manifest only: until a lockfile is regenerated it still records the vulnerable version.

### What upd verified

- **Validation:** No project-specific command was configured; proposal integrity passed.
- **Scope:** <code>Cargo.toml</code>.
- **Version boundary:** Security fixes move to the lowest release that resolves their advisories, whatever the freshness and bump policy.
- **Policy:** Freshness <code>7d</code>; maximum bump <code>minor</code>; lockfile regeneration off.
- **Security:** 1 advisory resolved; security fixes follow the advisories, not the update policy.

### Needs attention

> These dependencies were not changed because upd could not do so safely.

| Dependency | Current | Reason | File |
|---|---:|---|---|
| <code>lru</code> | <code>0.12.0</code> | security fix needs lockfile regeneration (lock: true): it pins a lockfile entry | <code>Cargo.lock</code> |

### Advisories without a fix

> upd found no release that resolves these advisories, so the dependencies are unchanged.

| Dependency | Version | Advisories | Severity | Reason |
|---|---:|---|---|---|
| <code>abandoned</code> | <code>1.0.0</code> | <code>RUSTSEC-2024-0003</code> | unknown | no fixed version is published |

<details>
<summary><strong>Proof and provenance</strong></summary>

- Security fixes: 1 (1 awaiting lockfile regeneration)
- Advisories resolved: 1
- Advisories without a fix: 1
- Applied updates: 0
- Change mix: 0 major, 0 minor, 0 patch, 0 revision
- Normalized specifiers: 0
- Dependency annotations: 0
- Held back by policy: 0
- Blocked: 1
- Not examined: 0
- Warnings: 0
- Auto-merge: off
- Full update report, security report and presentation model retained in the pipeline artifact

</details>

> Rebuilt from the latest default branch. The project pipeline remains the final merge boundary.

---
Prepared by [upd](https://github.com/rvben/upd).

<!-- upd-commit: <tip> -->
