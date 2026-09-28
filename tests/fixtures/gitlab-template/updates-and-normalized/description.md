<p><a href="https://github.com/rvben/upd"><img src="https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg" alt="upd" width="96"></a></p>

> **A tidy upgrade, already prepared.** upd prepared 2 dependency updates and 1 normalized specifier across 1 file and passed proposal-integrity checks. No major-version jumps among ordinary updates. Normalized specifiers are replaced as a whole; see the Normalized specifiers section for the versions written.

**2 moved forward** · **2 worth a look** · **1 normalized**

### Worth a look

Your attention is best spent on these non-patch updates.

| Dependency | Before | After | Change | File |
|---|---:|---:|---|---|
| <code>idna</code> | <code>3.6</code> | <code>3.7</code> | minor | <code>pyproject.toml</code> |
| <code>requests</code> | <code>2.31.0</code> | <code>2.32.3</code> | minor | <code>pyproject.toml</code> |

### Normalized specifiers

Each specifier below was replaced as a whole, including any range or ceiling it carried.

| Dependency | Before | After | Version | File |
|---|---:|---:|---:|---|
| <code>click</code> | <code>~=8.1</code> | <code>&gt;=8.1.7</code> | <code>8.1.7</code> | <code>pyproject.toml</code> |

### What upd verified

- **Validation:** No project-specific command was configured; proposal integrity passed.
- **Scope:** <code>dependency.txt</code>.
- **Version boundary:** No major-version jumps among ordinary updates. Normalized specifiers are replaced as a whole; see the Normalized specifiers section for the versions written.
- **Policy:** Freshness <code>7d</code>; maximum bump <code>minor</code>; lockfile regeneration off.

<details>
<summary><strong>Proof and provenance</strong></summary>

- Applied updates: 2
- Change mix: 0 major, 2 minor, 0 patch, 0 revision
- Normalized specifiers: 1
- Dependency annotations: 0
- Held back by policy: 0
- Blocked: 0
- Not examined: 0
- Warnings: 0
- Auto-merge: off
- Full update report and presentation model retained in the pipeline artifact

</details>

> Rebuilt from the latest default branch. The project pipeline remains the final merge boundary.

---
Prepared by [upd](https://github.com/rvben/upd).
