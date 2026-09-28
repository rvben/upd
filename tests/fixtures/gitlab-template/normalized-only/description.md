<p><a href="https://github.com/rvben/upd"><img src="https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg" alt="upd" width="96"></a></p>

> **A tidy upgrade, already prepared.** upd prepared 2 normalized specifiers across 1 file and passed proposal-integrity checks. Normalized specifiers are replaced as a whole; see the Normalized specifiers section for the versions written.

**2 normalized** · **1 saved for later**

### What changed

upd rewrote dependency specifiers into the configured shape. See the Normalized specifiers section for the versions written.

### Normalized specifiers

Each specifier below was replaced as a whole, including any range or ceiling it carried.

| Dependency | Before | After | Version | File |
|---|---:|---:|---:|---|
| <code>click</code> | <code>(no specifier)</code> | <code>&gt;=8.5.0</code> | <code>8.5.0</code> | <code>pyproject.toml</code> |
| <code>urllib3</code> | <code>&lt;= 2.0.0</code> | <code>&gt;=2.7.0</code> | <code>2.7.0</code> | <code>pyproject.toml</code> |

### What upd verified

- **Validation:** No project-specific command was configured; proposal integrity passed.
- **Scope:** <code>dependency.txt</code>.
- **Version boundary:** Normalized specifiers are replaced as a whole; see the Normalized specifiers section for the versions written.
- **Policy:** Freshness <code>7d</code>; maximum bump <code>minor</code>; lockfile regeneration off.

<details>
<summary><strong>Saved for a deliberate upgrade (1)</strong></summary>

upd left these releases unchanged because they sit outside this project’s current update policy.

| Dependency | Selected | Available | Policy | File |
|---|---:|---:|---|---|
| <code>urllib3</code> | <code>2.7.0</code> | <code>2.8.0</code> | cooldown | <code>pyproject.toml</code> |

</details>

<details>
<summary><strong>Proof and provenance</strong></summary>

- Applied updates: 0
- Change mix: 0 major, 0 minor, 0 patch, 0 revision
- Normalized specifiers: 2
- Dependency annotations: 0
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
