<p><a href="https://github.com/rvben/upd"><img src="https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg" alt="upd" width="96"></a></p>

> **A careful upgrade, with follow-up.** upd prepared 1 dependency update across 1 file and passed proposal-integrity checks. It stopped short of 1 dependency it could not change safely.

**1 moved forward** · **1 worth a look** · **2 saved for later** · **1 needs attention**

### Worth a look

Your attention is best spent on these non-patch updates.

| Dependency | Before | After | Change | File |
|---|---:|---:|---|---|
| <code>bad&#124;pkg&lt;/code&gt;</code> | <code>1.0.0&#96;</code> | <code>1.1.0</code> | minor | <code>dependency.txt</code> |

<details>
<summary><strong>Dependency metadata (1)</strong></summary>

| Dependency | Version | File |
|---|---:|---|
| <code>annotated</code> | <code>2.0.0</code> | <code>dependency.txt</code> |

</details>

### What upd verified

- **Validation:** No project-specific command was configured; proposal integrity passed.
- **Scope:** <code>dependency.txt</code>.
- **Version boundary:** No major-version jumps.
- **Policy:** Freshness <code>7d</code>; maximum bump <code>minor</code>; lockfile regeneration off.

<details>
<summary><strong>Saved for a deliberate upgrade (2)</strong></summary>

upd left these releases unchanged because they sit outside this project’s current update policy.

| Dependency | Selected | Available | Policy | File |
|---|---:|---:|---|---|
| <code>major&#95;pkg</code> | <code>1.0.0</code> | <code>2.0.0</code> | bump ceiling | <code>dependency.txt</code> |
| <code>fresh&#95;pkg</code> | <code>1.0.1</code> | <code>1.1.0</code> | cooldown | <code>dependency.txt</code> |

</details>

### Needs attention

> These dependencies were not changed because upd could not do so safely.

| Dependency | Current | Reason | File |
|---|---:|---|---|
| <code>blocked&lt;script&gt;</code> | <code>3.0.0</code> | Add &#42;trusted&#42; metadata &#124; before updating this pin | <code>dependency.txt</code> |

<details>
<summary><strong>Proof and provenance</strong></summary>

- Applied updates: 1
- Change mix: 0 major, 1 minor, 0 patch, 0 revision
- Normalized specifiers: 0
- Dependency annotations: 1
- Held back by policy: 2
- Blocked: 1
- Not examined: 1
- Warnings: 2
- Auto-merge: off
- Full update report and presentation model retained in the pipeline artifact

</details>

> Rebuilt from the latest default branch. The project pipeline remains the final merge boundary.

---
Prepared by [upd](https://github.com/rvben/upd).
