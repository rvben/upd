<p><a href="https://github.com/rvben/upd"><img src="https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg" alt="upd" width="96"></a></p>

> **A careful upgrade, with follow-up.** upd prepared 1 security fix across 1 file and passed proposal-integrity checks. It stopped short of 1 dependency it could not change safely.

**1 security fix** · **1 needs attention**

### Security fixes

upd moved these dependencies to the lowest release that resolves their advisories, outside the freshness and bump policy.

| Dependency | Change | Advisories | Severity | File |
|---|---|---|---|---|
| <code>time</code> | <code>0.3.20</code> → <code>0.3.36</code> | <code>RUSTSEC-2024-0010</code> | Low | <code>Cargo.lock</code> |

### What upd verified

- **Validation:** No project-specific command was configured; proposal integrity passed.
- **Scope:** <code>Cargo.lock</code>.
- **Version boundary:** Security fixes move to the lowest release that resolves their advisories, whatever the freshness and bump policy.
- **Policy:** Freshness <code>7d</code>; maximum bump <code>minor</code>; lockfile regeneration on.
- **Security:** 1 advisory resolved; security fixes follow the advisories, not the update policy.

### Needs attention

> These dependencies were not changed because upd could not do so safely.

| Dependency | Current | Reason | File |
|---|---:|---|---|
| <code>lru</code> | <code>0.16.0</code> | security fix blocked: ratatui-core requires lru ^0.16.0, &lt;0.16.2 &#124; see &lt;b&gt;Cargo.toml&lt;/b&gt; | <code>Cargo.lock</code> |

<details>
<summary><strong>Proof and provenance</strong></summary>

- Security fixes: 1 (0 awaiting lockfile regeneration)
- Advisories resolved: 1
- Advisories without a fix: 0
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
