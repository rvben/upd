<p><a href="https://github.com/rvben/upd"><img src="https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg" alt="upd" width="96"></a></p>

> **A tidy upgrade, already prepared.** upd prepared 1 dependency update across 1 file and passed proposal-integrity checks. No major-version jumps.

**1 moved forward** · **1 quiet patch** · **3 saved for later**

### What changed

| Dependency | Before | After | Change | File |
|---|---:|---:|---|---|
| <code>example</code> | <code>1.0.0</code> | <code>1.0.1</code> | patch | <code>dependency.txt</code> |

### What upd verified

- **Validation:** No project-specific command was configured; proposal integrity passed.
- **Scope:** <code>dependency.txt</code>.
- **Version boundary:** No major-version jumps.
- **Policy:** Freshness <code>7d</code>; maximum bump <code>patch</code>; lockfile regeneration off.

<details>
<summary><strong>Saved for a deliberate upgrade (3)</strong></summary>

1 major-version release above this merge request’s bump ceiling is left to the major-upgrade lane on <code>automation/upd-dependencies-major</code>, whose merge request upd never merges: [open major-upgrade merge requests](<server>/remote/-/merge_requests?state=opened&source_branch=automation%2Fupd-dependencies-major).

upd left these releases unchanged because they sit outside this project’s current update policy.

| Dependency | Selected | Available | Policy | File |
|---|---:|---:|---|---|
| <code>featureful</code> | <code>3.1.0</code> | <code>3.2.0</code> | bump ceiling | <code>dependency.txt</code> |
| <code>fresh</code> | <code>1.0.0</code> | <code>1.1.0</code> | cooldown | <code>dependency.txt</code> |

</details>

<details>
<summary><strong>Proof and provenance</strong></summary>

- Applied updates: 1
- Change mix: 0 major, 0 minor, 1 patch, 0 revision
- Normalized specifiers: 0
- Dependency annotations: 0
- Held back by policy: 3
- Blocked: 0
- Not examined: 0
- Warnings: 0
- Auto-merge: off
- Full update report and presentation model retained in the pipeline artifact

</details>

> Rebuilt from the latest default branch. The project pipeline remains the final merge boundary.

---
Prepared by [upd](https://github.com/rvben/upd).

<!-- upd-commit: <tip> -->
