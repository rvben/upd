<p><a href="https://github.com/rvben/upd"><img src="https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg" alt="upd" width="96"></a></p>

> **A tidy upgrade, already prepared.** upd prepared 1 dependency metadata annotation across 1 file and passed proposal-integrity checks. No major-version jumps.

**1 saved for later**

### What changed

upd added or refreshed dependency metadata without changing selected versions.

<details>
<summary><strong>Dependency metadata (1)</strong></summary>

| Dependency | Version | File |
|---|---:|---|
| <code>actions/checkout</code> | <code>v5.0.0</code> | <code>.github/workflows/ci.yml</code> |

</details>

### What upd verified

- **Validation:** No project-specific command was configured; proposal integrity passed.
- **Scope:** <code>.github/workflows/ci.yml</code>.
- **Version boundary:** No major-version jumps.
- **Policy:** Freshness <code>7d</code>; maximum bump <code>minor</code>; lockfile regeneration off.

<details>
<summary><strong>Saved for a deliberate upgrade (1)</strong></summary>

upd left these releases unchanged because they sit outside this project’s current update policy.

| Dependency | Selected | Available | Policy | File |
|---|---:|---:|---|---|
| <code>actions/checkout</code> | <code>v5.0.0</code> | <code>v7.0.1</code> | bump ceiling | <code>.github/workflows/ci.yml</code> |

</details>

<details>
<summary><strong>Proof and provenance</strong></summary>

- Applied updates: 0
- Change mix: 0 major, 0 minor, 0 patch, 0 revision
- Normalized specifiers: 0
- Dependency annotations: 1
- Held back by policy: 1
- Blocked: 0
- Not examined: 0
- Warnings: 0
- Auto-merge: off
- Full update report and presentation model retained in the pipeline artifact

</details>

> Rebuilt from the latest default branch. The project pipeline remains the final merge boundary.

---
Prepared by [upd](https://github.com/rvben/upd).
