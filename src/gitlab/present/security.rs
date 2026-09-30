//! Security fixes: what `upd audit --fix-audit` changed before the update,
//! what it could not change, and which advisories that resolves.
//!
//! Fix rows name the manifest specifier they replaced rather than an
//! installed version, so advisories are matched to fixes by package name.
//! A package counts as resolved only when every fix for it applied: one
//! blocked, skipped, unapplied or unfixable occurrence leaves its advisories
//! open.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use super::{
    BlockedRow, Shaped, UNKNOWN, UNKNOWN_DEPENDENCY, UNKNOWN_FILE, clean, clean_or, code,
    each_or_empty, field, md, plural, table, unique_by,
};

/// Fix statuses that leave the dependency at a release resolving its
/// advisories. `pending_relock` has the manifest written and the lockfile
/// still to regenerate. `already_satisfied` wrote nothing because the
/// manifest already requires the fix, so it resolves anything only when the
/// lockfile was regenerated.
const FIXED: [&str; 2] = ["applied", "pending_relock"];

/// The release a fix row shows when its report names none.
const UNNAMED_FLOOR: &str = "resolved lockfile floor";

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Advisory ids shown in a table cell before the rest are summarized.
const SHOWN_ADVISORIES: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SecurityFixRow {
    pub package: String,
    /// The OSV ecosystem the package belongs to, which tells a package
    /// apart from a same-named one in another ecosystem.
    #[serde(skip)]
    pub ecosystem: String,
    pub from: String,
    pub to: String,
    pub path: String,
    pub status: String,
    pub advisories: Vec<String>,
    pub severity: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnfixableRow {
    pub package: String,
    pub version: String,
    pub advisories: Vec<String>,
    pub severity: String,
    pub reason: String,
}

/// A fixed dependency the audit after the update found vulnerable again.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReintroducedRow {
    pub package: String,
    /// The OSV ecosystem, which tells the package apart from a same-named
    /// one in another ecosystem.
    #[serde(skip)]
    pub ecosystem: String,
    pub version: String,
    pub advisories: Vec<String>,
    pub severity: String,
    pub reason: String,
    /// The files the security step fixed the dependency in. The audit of
    /// the updated tree names no file, so these are where to look, not
    /// where the vulnerable release is locked.
    pub fixed_in: Vec<String>,
}

/// A release a fix's relock locked besides the fix, published inside the
/// cooldown. It stays locked, since holding it back could undo the fix.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct YoungRow {
    pub package: String,
    pub version: String,
    pub published_at: String,
    pub cooldown: String,
    pub lockfile: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SecurityCounts {
    /// Every fix that moved a dependency, including those awaiting a relock.
    pub fixes: usize,
    pub pending_relock: usize,
    pub blocked: usize,
    pub skipped: usize,
    /// Fixes that exist but were not written: configuration holds them, or
    /// the file cannot express them.
    pub not_applied: usize,
    pub unfixable: usize,
    /// Distinct advisories of the packages every fix for which applied.
    pub advisories: usize,
    /// Releases the fixes' relocks locked inside the cooldown; absent when
    /// there are none.
    #[serde(skip_serializing_if = "is_zero")]
    pub young: usize,
    /// Fixed dependencies the update moved back to a release an advisory
    /// affects; absent when there are none.
    #[serde(skip_serializing_if = "is_zero")]
    pub reintroduced: usize,
    /// Warnings the security audits reported; absent when there are none.
    #[serde(skip_serializing_if = "is_zero")]
    pub audit_warnings: usize,
}

/// The security step's part of the review model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Security {
    pub fixes: Vec<SecurityFixRow>,
    pub unfixable: Vec<UnfixableRow>,
    /// Absent when no fix's relock locked a release inside the cooldown.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub young: Vec<YoungRow>,
    /// Absent unless the audit after the update found a fixed dependency
    /// vulnerable again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reintroduced: Vec<ReintroducedRow>,
    /// What the audit applying the fixes, and the audit after the update,
    /// could not check, such as a release a fix's relock locked whose age
    /// the registry would not give. Absent when both checked everything.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audit_warnings: Vec<String>,
    pub counts: SecurityCounts,
    /// Fixes a requirement blocked, lockfile regeneration being off
    /// skipped, or upd could not write; the presentation lists them with
    /// everything else that needs attention.
    #[serde(skip)]
    pub attention: Vec<BlockedRow>,
    /// Packages, by ecosystem, with at least one occurrence left
    /// vulnerable.
    #[serde(skip)]
    open: BTreeSet<(String, String)>,
    /// The vulnerable versions the security step found, by ecosystem and
    /// package: what a lockfile awaiting regeneration still records.
    #[serde(skip)]
    audited: BTreeMap<(String, String), BTreeSet<String>>,
    /// Packages every fix for which applied, in order.
    #[serde(skip)]
    resolved: Vec<String>,
    /// How many of `audit_warnings` the audit applying the fixes reported;
    /// the rest came from the audit after the update.
    #[serde(skip)]
    fix_audit_warnings: usize,
}

/// The warnings an audit report lists, cleaned for display.
fn report_warnings(report: &Value) -> Shaped<Vec<String>> {
    Ok(each_or_empty(field(report, "warnings")?)?
        .into_iter()
        .map(|warning| clean_or(warning, UNKNOWN, 400))
        .collect())
}

impl Security {
    /// Reads the report `upd audit --fix-audit --format json` printed, for a
    /// run that regenerated lockfiles when `lock` is set.
    pub fn from_report(report: &Value, lock: bool) -> Shaped<Self> {
        let advisories = advisories_by_package(report)?;
        let lookup = |ecosystem: &str, package: &str| {
            advisories
                .get(&(ecosystem.to_string(), package.to_string()))
                .map(|found| {
                    (
                        found.ids.iter().cloned().collect::<Vec<_>>(),
                        found.severity.label(),
                    )
                })
                .unwrap_or_else(|| (Vec::new(), UNKNOWN.to_string()))
        };

        let mut fixes = Vec::new();
        let mut attention = Vec::new();
        let mut unfixable = Vec::new();
        let (mut blocked, mut skipped, mut not_applied) = (0, 0, 0);
        // Packages, by ecosystem, with at least one occurrence left vulnerable.
        let mut open = BTreeSet::new();
        for fix in each_or_empty(field(report, "fixes")?)? {
            let package = clean_or(field(fix, "package")?, UNKNOWN_DEPENDENCY, 160);
            let ecosystem = clean_or(field(fix, "ecosystem")?, "", 32);
            let from = clean_or(field(fix, "from_version")?, UNKNOWN, 160);
            let status = clean_or(field(fix, "status")?, UNKNOWN, 32);
            let path = clean_or(field(fix, "path")?, UNKNOWN_FILE, 160);
            let error = field(fix, "error")?;
            match status.as_str() {
                fixed if FIXED.contains(&fixed) || (lock && fixed == "already_satisfied") => {
                    let (ids, severity) = lookup(&ecosystem, &package);
                    fixes.push(SecurityFixRow {
                        to: clean_or(field(fix, "to_version")?, UNNAMED_FLOOR, 160),
                        package,
                        ecosystem,
                        from,
                        path,
                        status,
                        advisories: ids,
                        severity,
                    });
                }
                "blocked" => {
                    blocked += 1;
                    open.insert((ecosystem.clone(), package.clone()));
                    attention.push(BlockedRow {
                        package,
                        current: from,
                        reason: format!(
                            "security fix blocked: {}",
                            clean_or(error, "a manifest requirement excludes it", 240)
                        ),
                        path,
                    });
                }
                "skipped" => {
                    skipped += 1;
                    open.insert((ecosystem.clone(), package.clone()));
                    attention.push(BlockedRow {
                        package,
                        current: from,
                        reason: "security fix needs lockfile regeneration (lock: true): it pins a lockfile entry".to_string(),
                        path,
                    });
                }
                "already_satisfied" => {
                    skipped += 1;
                    open.insert((ecosystem.clone(), package.clone()));
                    attention.push(BlockedRow {
                        package,
                        current: from,
                        reason: "security fix needs lockfile regeneration (lock: true): the manifest already requires the fix, but the lockfile still records the vulnerable version".to_string(),
                        path,
                    });
                }
                // An `unfixable` row naming the release that fixes it has a
                // fix upd did not write, which a person can still apply.
                "unfixable" if !field(fix, "to_version")?.is_null() => {
                    not_applied += 1;
                    open.insert((ecosystem.clone(), package.clone()));
                    attention.push(BlockedRow {
                        reason: format!(
                            "security fix to {} not applied: {}",
                            clean_or(field(fix, "to_version")?, UNKNOWN, 160),
                            clean_or(error, "upd could not write it", 240)
                        ),
                        package,
                        current: from,
                        path,
                    });
                }
                // `unfixable`, and anything a later updater reports that
                // this reader does not know, is shown as not fixed rather
                // than claimed as a fix.
                _ => {
                    open.insert((ecosystem.clone(), package.clone()));
                    let (ids, severity) = lookup(&ecosystem, &package);
                    unfixable.push(UnfixableRow {
                        package,
                        version: from,
                        advisories: ids,
                        severity,
                        reason: clean_or(error, "no release resolves its advisories", 240),
                    });
                }
            }
        }
        // A fix is written however young; what its relock locked besides,
        // inside the cooldown, is left for review rather than held back.
        let mut young = Vec::new();
        for entry in each_or_empty(field(report, "lockfile_cooldown")?)? {
            young.push(YoungRow {
                package: clean_or(field(entry, "package")?, UNKNOWN_DEPENDENCY, 160),
                version: clean_or(field(entry, "version")?, UNKNOWN, 160),
                published_at: clean_or(field(entry, "published_at")?, UNKNOWN, 64),
                cooldown: clean_or(field(entry, "cooldown")?, UNKNOWN, 32),
                lockfile: clean_or(field(entry, "lockfile")?, UNKNOWN_FILE, 160),
            });
        }
        unique_by(&mut young, |row| {
            (
                row.package.clone(),
                row.version.clone(),
                row.lockfile.clone(),
            )
        });
        young.sort_by(|a, b| (&a.package, &a.lockfile).cmp(&(&b.package, &b.lockfile)));
        unique_by(&mut fixes, |row| {
            (
                row.package.clone(),
                row.ecosystem.clone(),
                row.from.clone(),
                row.to.clone(),
                row.path.clone(),
            )
        });
        fixes.sort_by(|a, b| (&a.package, &a.path).cmp(&(&b.package, &b.path)));
        unique_by(&mut unfixable, |row| {
            (row.package.clone(), row.version.clone(), row.reason.clone())
        });
        unique_by(&mut attention, |row| {
            (
                row.package.clone(),
                row.current.clone(),
                row.reason.clone(),
                row.path.clone(),
            )
        });

        let mut audit_warnings = report_warnings(report)?;
        unique_by(&mut audit_warnings, Clone::clone);
        let counts = SecurityCounts {
            fixes: fixes.len(),
            pending_relock: fixes
                .iter()
                .filter(|row| row.status == "pending_relock")
                .count(),
            blocked,
            skipped,
            not_applied,
            unfixable: unfixable.len(),
            advisories: 0,
            young: young.len(),
            reintroduced: 0,
            audit_warnings: audit_warnings.len(),
        };
        let mut security = Self {
            fixes,
            unfixable,
            young,
            reintroduced: Vec::new(),
            fix_audit_warnings: audit_warnings.len(),
            audit_warnings,
            counts,
            attention,
            open,
            audited: vulnerable_versions(report)?,
            resolved: Vec::new(),
        };
        security.tally_resolved();
        Ok(security)
    }

    /// Recomputes the resolved packages and their advisory count from the
    /// fixes and the packages left open.
    fn tally_resolved(&mut self) {
        let resolved_rows = || {
            self.fixes.iter().filter(|row| {
                !self
                    .open
                    .contains(&(row.ecosystem.clone(), row.package.clone()))
            })
        };
        let advisories: BTreeSet<&String> =
            resolved_rows().flat_map(|row| &row.advisories).collect();
        let advisories = advisories.len();
        let mut resolved: Vec<String> = resolved_rows().map(|row| row.package.clone()).collect();
        resolved.dedup();
        self.counts.advisories = advisories;
        self.resolved = resolved;
    }

    /// Resolved fixes, the ones an audit of the final tree can confirm.
    fn confirmable(&self) -> impl Iterator<Item = &SecurityFixRow> {
        self.fixes.iter().filter(|row| {
            !self
                .open
                .contains(&(row.ecosystem.clone(), row.package.clone()))
        })
    }

    /// Whether any resolved fix is worth re-auditing once the update has
    /// changed the tree.
    pub fn is_recheckable(&self) -> bool {
        self.confirmable().next().is_some()
    }

    /// Reads the report `upd audit --format json` printed for the final
    /// tree. A resolved fix it still finds vulnerable was moved back by the
    /// update: it is listed for attention, and its advisories no longer
    /// count as resolved.
    pub fn apply_recheck(&mut self, report: &Value) -> Shaped<()> {
        // Vulnerable versions and their advisories, by ecosystem and package.
        let mut found: BTreeMap<(String, String), BTreeMap<String, Advisories>> = BTreeMap::new();
        for vulnerability in each_or_empty(field(report, "vulnerabilities")?)? {
            let package = clean_or(field(vulnerability, "package")?, UNKNOWN_DEPENDENCY, 160);
            let ecosystem = clean_or(field(vulnerability, "ecosystem")?, "", 32);
            let version = clean_or(field(vulnerability, "version")?, UNKNOWN, 160);
            let entry = found
                .entry((ecosystem, package))
                .or_default()
                .entry(version)
                .or_default();
            entry
                .ids
                .insert(clean_or(field(vulnerability, "id")?, UNKNOWN, 80));
            let severity = Severity::of(field(vulnerability, "severity")?);
            if severity > entry.severity {
                entry.severity = severity;
            }
        }
        let mut reintroduced = Vec::new();
        let mut reopened = BTreeSet::new();
        for row in self.confirmable() {
            let key = (row.ecosystem.clone(), row.package.clone());
            let Some(versions) = found.get(&key) else {
                continue;
            };
            // A fix awaiting lockfile regeneration leaves its lockfile at the
            // vulnerable version the security step found, whatever the
            // update did; only another vulnerable version is the update's.
            let stale = self.fixes.iter().any(|fix| {
                (fix.ecosystem.clone(), fix.package.clone()) == key
                    && fix.status == "pending_relock"
            });
            for (version, advisories) in versions {
                if stale
                    && self
                        .audited
                        .get(&key)
                        .is_some_and(|audited| audited.contains(version))
                {
                    continue;
                }
                let ids: Vec<String> = advisories.ids.iter().cloned().collect();
                // The update moved it only when every fix names its release
                // and none named this one.
                let moved = self
                    .fixes
                    .iter()
                    .filter(|fix| (fix.ecosystem.clone(), fix.package.clone()) == key)
                    .all(|fix| fix.to != UNNAMED_FLOOR && &fix.to != version);
                let reason = if !moved {
                    format!(
                        "after the dependency update, the audit finds {} {version} still affected by {}",
                        row.package,
                        ids.join(", ")
                    )
                } else {
                    format!(
                        "the dependency update moved {} to {version}, which {} still {}",
                        row.package,
                        ids.join(", "),
                        plural(ids.len(), "affects", "affect")
                    )
                };
                let fixed_in: BTreeSet<String> = self
                    .fixes
                    .iter()
                    .filter(|fix| (fix.ecosystem.clone(), fix.package.clone()) == key)
                    .map(|fix| fix.path.clone())
                    .collect();
                reopened.insert(key.clone());
                reintroduced.push(ReintroducedRow {
                    package: row.package.clone(),
                    ecosystem: row.ecosystem.clone(),
                    version: version.clone(),
                    advisories: ids,
                    severity: advisories.severity.label(),
                    reason,
                    fixed_in: fixed_in.into_iter().collect(),
                });
            }
        }
        unique_by(&mut reintroduced, |row| {
            (
                row.package.clone(),
                row.ecosystem.clone(),
                row.version.clone(),
            )
        });
        self.open.extend(reopened);
        self.reintroduced = reintroduced;
        self.counts.reintroduced = self.reintroduced.len();
        for warning in report_warnings(report)? {
            if !self.audit_warnings.contains(&warning) {
                self.audit_warnings.push(warning);
            }
        }
        self.counts.audit_warnings = self.audit_warnings.len();
        self.tally_resolved();
        Ok(())
    }

    /// Whether the step found anything to report.
    pub fn is_empty(&self) -> bool {
        self.fixes.is_empty()
            && self.unfixable.is_empty()
            && self.attention.is_empty()
            && self.young.is_empty()
            && self.reintroduced.is_empty()
    }

    /// Distinct packages the fixes moved, in order.
    pub fn fixed_packages(&self) -> Vec<&str> {
        let mut packages: Vec<&str> = self.fixes.iter().map(|row| row.package.as_str()).collect();
        packages.dedup();
        packages
    }

    /// Distinct packages every fix for which applied, in order: the ones
    /// whose advisories [`SecurityCounts::advisories`] counts as resolved.
    pub fn resolved_packages(&self) -> Vec<&str> {
        self.resolved.iter().map(String::as_str).collect()
    }

    /// The security lines of the evidence list, shared by the full and the
    /// fallback description so neither drops a caveat the other states.
    pub fn evidence_lines(&self) -> String {
        let counts = &self.counts;
        let young = if counts.young > 0 {
            format!(
                "- Released inside the freshness window, locked by a fix's relock: {}\n",
                counts.young
            )
        } else {
            String::new()
        };
        let reintroduced = if counts.reintroduced > 0 {
            format!(
                "- Vulnerable again after the dependency update: {}\n",
                counts.reintroduced
            )
        } else {
            String::new()
        };
        let audit_warnings = if counts.audit_warnings > 0 {
            format!("- Security audit warnings: {}\n", counts.audit_warnings)
        } else {
            String::new()
        };
        format!(
            "- Security fixes: {} ({} awaiting lockfile regeneration)\n- Advisories resolved: {}\n- Advisories without a fix: {}\n{reintroduced}{young}{audit_warnings}",
            counts.fixes, counts.pending_relock, counts.advisories, counts.unfixable,
        )
    }

    /// One-line account of the step for the job log, with the error count
    /// of the report it was read from.
    pub fn summary_line(&self, report: &Value) -> Shaped<String> {
        let errors = field(field(report, "summary")?, "errors")?;
        let counts = &self.counts;
        Ok(format!(
            "upd audit: {} fixed, {} pending relock, {} skipped, {} blocked, {} not applied, {} without a fix, {} error(s)",
            counts.fixes,
            counts.pending_relock,
            counts.skipped,
            counts.blocked,
            counts.not_applied,
            counts.unfixable,
            super::interpolate(errors),
        ))
    }

    /// A job-log warning for each dependency left vulnerable, and for each
    /// release a fix's relock locked inside the cooldown.
    pub fn warnings(&self) -> Vec<String> {
        let attention = self.attention.iter().map(|row| {
            format!(
                "warning: {} {} in {}: {}",
                row.package, row.current, row.path, row.reason
            )
        });
        let unfixable = self.unfixable.iter().map(|row| {
            format!(
                "warning: no security fix for {} {}: {}",
                row.package, row.version, row.reason
            )
        });
        let young = self.young.iter().map(|row| {
            format!(
                "warning: {} locks {} {}, released {}, inside the {} cooldown; a security fix's relock locked it",
                row.lockfile, row.package, row.version, row.published_at, row.cooldown
            )
        });
        let audit = self.audit_warnings[..self.fix_audit_warnings]
            .iter()
            .map(|warning| format!("warning: {warning}"));
        attention
            .chain(unfixable)
            .chain(young)
            .chain(audit)
            .collect()
    }

    /// A job-log warning for each fixed dependency the update moved back to
    /// a vulnerable release, and for each new warning the audit after the
    /// update reported.
    pub fn recheck_warnings(&self) -> Vec<String> {
        let reintroduced = self.reintroduced.iter().map(|row| {
            format!(
                "warning: {} (fixed in {}): {}",
                row.package,
                row.fixed_in.join(", "),
                row.reason
            )
        });
        let audit = self.audit_warnings[self.fix_audit_warnings..]
            .iter()
            .map(|warning| format!("warning: {warning}"));
        reintroduced.chain(audit).collect()
    }

    /// The part of Needs attention for what the security audits could not
    /// check, starting with its separating blank line; empty when they
    /// checked everything.
    pub fn audit_warnings_section(&self) -> String {
        if self.audit_warnings.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\n> The security audits could not check everything; what these warnings name was not verified. Review it before merging.\n\n",
        );
        out.push_str(&table(&self.audit_warnings, 12, |warning| {
            format!("- {}", md(warning))
        }));
        if self.audit_warnings.len() > 12 {
            out.push_str(&format!(
                "\n\n_{} more security audit warnings are preserved in the pipeline artifact._",
                self.audit_warnings.len() - 12
            ));
        }
        out
    }

    /// The part of Needs attention for fixed dependencies the update moved
    /// back to a vulnerable release, starting with its separating blank
    /// line; empty when there are none.
    pub fn reintroduced_section(&self) -> String {
        if self.reintroduced.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\n> The security step fixed these dependencies, but after the dependency update an audit finds them vulnerable again, so their advisories are not counted as resolved. Review them before merging; Fixed in names the files the security step changed, since the audit does not say which lockfile holds the release.\n\n| Dependency | Version | Advisories | Severity | Reason | Fixed in |\n|---|---:|---|---|---|---|\n",
        );
        out.push_str(&table(&self.reintroduced, 12, |row| {
            format!(
                "| {} | {} | {} | {} | {} | {} |",
                code(&row.package),
                code(&row.version),
                advisory_cell(&row.advisories),
                md(&row.severity),
                md(&row.reason),
                row.fixed_in
                    .iter()
                    .map(|path| code(path))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }));
        if self.reintroduced.len() > 12 {
            out.push_str(&format!(
                "\n\n_{} more dependencies vulnerable again are preserved in the pipeline artifact._",
                self.reintroduced.len() - 12
            ));
        }
        out
    }

    /// The part of Needs attention for releases the fixes' relocks locked
    /// inside the cooldown, starting with its separating blank line; empty
    /// when there are none.
    pub fn young_section(&self) -> String {
        if self.young.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\n> A security fix's relock also locked these releases, published inside the freshness window. upd leaves them locked, since holding them back could undo the fix; review them before merging.\n\n| Dependency | Version | Released | Cooldown | File |\n|---|---:|---|---|---|\n",
        );
        out.push_str(&table(&self.young, 12, |row| {
            format!(
                "| {} | {} | {} | {} | {} |",
                code(&row.package),
                code(&row.version),
                code(&row.published_at),
                md(&row.cooldown),
                code(&row.lockfile)
            )
        }));
        if self.young.len() > 12 {
            out.push_str(&format!(
                "\n\n_{} more releases inside the freshness window are preserved in the pipeline artifact._",
                self.young.len() - 12
            ));
        }
        out
    }

    /// The Security fixes section; empty without fixes.
    pub fn fixes_section(&self) -> String {
        if self.fixes.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "### Security fixes\n\nupd moved these dependencies to the lowest release that resolves their advisories, outside the freshness and bump policy.\n\n| Dependency | Change | Advisories | Severity | File |\n|---|---|---|---|---|\n",
        );
        out.push_str(&table(&self.fixes, 20, |row| {
            format!(
                "| {} | {} \u{2192} {} | {} | {} | {} |",
                code(&row.package),
                code(&row.from),
                code(&row.to),
                advisory_cell(&row.advisories),
                md(&row.severity),
                code(&row.path)
            )
        }));
        if self.fixes.len() > 20 {
            out.push_str(&format!(
                "\n\n_{} more security fixes are preserved in the pipeline artifact._",
                self.fixes.len() - 20
            ));
        }
        let pending = self.counts.pending_relock;
        if pending > 0 {
            out.push_str(&format!(
                "\n\n> Lockfile regeneration is off, so {} {} the manifest only: until a lockfile is regenerated it still records the vulnerable {}.",
                pending,
                plural(pending, "fix changes", "fixes change"),
                plural(pending, "version", "versions"),
            ));
        }
        out
    }

    /// The Advisories without a fix section, starting with its separating
    /// blank line; empty when every advisory has a fix.
    pub fn unfixable_section(&self) -> String {
        if self.unfixable.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\n### Advisories without a fix\n\n> upd found no release that resolves these advisories, so the dependencies are unchanged.\n\n| Dependency | Version | Advisories | Severity | Reason |\n|---|---:|---|---|---|\n",
        );
        out.push_str(&table(&self.unfixable, 12, |row| {
            format!(
                "| {} | {} | {} | {} | {} |",
                code(&row.package),
                code(&row.version),
                advisory_cell(&row.advisories),
                md(&row.severity),
                md(&row.reason)
            )
        }));
        if self.unfixable.len() > 12 {
            out.push_str(&format!(
                "\n\n_{} more unfixed advisories are preserved in the pipeline artifact._",
                self.unfixable.len() - 12
            ));
        }
        out
    }
}

/// Up to [`SHOWN_ADVISORIES`] ids, then how many more.
fn advisory_cell(ids: &[String]) -> String {
    if ids.is_empty() {
        return md(UNKNOWN);
    }
    let mut cell = ids
        .iter()
        .take(SHOWN_ADVISORIES)
        .map(|id| code(id))
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > SHOWN_ADVISORIES {
        cell.push_str(&format!(" +{} more", ids.len() - SHOWN_ADVISORIES));
    }
    cell
}

/// Severity labels in rising order; anything unrecognised ranks lowest.
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Severity {
    rank: u8,
    label: Option<String>,
}

impl Severity {
    fn of(value: &Value) -> Self {
        let label = clean(value, 32);
        let rank = match label.to_ascii_uppercase().as_str() {
            "LOW" => 1,
            "MEDIUM" | "MODERATE" => 2,
            "HIGH" => 3,
            "CRITICAL" => 4,
            _ => 0,
        };
        Self {
            rank,
            label: (!label.is_empty()).then_some(label),
        }
    }

    fn label(&self) -> String {
        self.label.clone().unwrap_or_else(|| UNKNOWN.to_string())
    }
}

#[derive(Default)]
struct Advisories {
    ids: BTreeSet<String>,
    severity: Severity,
}

/// The vulnerable versions a report names, keyed by ecosystem and package.
fn vulnerable_versions(report: &Value) -> Shaped<BTreeMap<(String, String), BTreeSet<String>>> {
    let mut by_package: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for vulnerability in each_or_empty(field(report, "vulnerabilities")?)? {
        by_package
            .entry((
                clean_or(field(vulnerability, "ecosystem")?, "", 32),
                clean_or(field(vulnerability, "package")?, UNKNOWN_DEPENDENCY, 160),
            ))
            .or_default()
            .insert(clean_or(field(vulnerability, "version")?, UNKNOWN, 160));
    }
    Ok(by_package)
}

/// Advisories keyed by ecosystem and package, the identity a fix entry
/// carries too.
fn advisories_by_package(report: &Value) -> Shaped<BTreeMap<(String, String), Advisories>> {
    let mut by_package: BTreeMap<(String, String), Advisories> = BTreeMap::new();
    for vulnerability in each_or_empty(field(report, "vulnerabilities")?)? {
        let package = clean_or(field(vulnerability, "package")?, UNKNOWN_DEPENDENCY, 160);
        let ecosystem = clean_or(field(vulnerability, "ecosystem")?, "", 32);
        let id = clean_or(field(vulnerability, "id")?, UNKNOWN, 80);
        let severity = Severity::of(field(vulnerability, "severity")?);
        let entry = by_package.entry((ecosystem, package)).or_default();
        entry.ids.insert(id);
        if severity > entry.severity {
            entry.severity = severity;
        }
    }
    Ok(by_package)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vulnerability(package: &str, id: &str, severity: &str) -> Value {
        json!({"package": package, "version": "1.0.0", "ecosystem": "npm", "id": id, "severity": severity})
    }

    fn fix(package: &str, to: &str, path: &str, status: &str) -> Value {
        json!({"package": package, "ecosystem": "npm", "from_version": "1.0.0", "to_version": to, "path": path, "status": status})
    }

    fn read(vulnerabilities: Value, fixes: Value) -> Security {
        read_with_lock(vulnerabilities, fixes, true)
    }

    fn read_with_lock(vulnerabilities: Value, fixes: Value, lock: bool) -> Security {
        Security::from_report(
            &json!({
                "vulnerabilities": vulnerabilities,
                "fixes": fixes,
                "summary": {"errors": 0},
            }),
            lock,
        )
        .unwrap()
    }

    #[test]
    fn a_report_without_fixes_is_empty() {
        let security = Security::from_report(&json!({"summary": {"errors": 0}}), true).unwrap();
        assert!(security.is_empty());
        assert_eq!(security.counts.fixes, 0);
        assert_eq!(security.fixes_section(), "");
        assert_eq!(security.unfixable_section(), "");
    }

    #[test]
    fn fixes_that_leave_a_safe_release_count_as_fixes() {
        let security = read(
            json!([
                vulnerability("a", "GHSA-a", "Low"),
                vulnerability("b", "GHSA-b", "High"),
                vulnerability("c", "GHSA-c", "Medium"),
            ]),
            json!([
                fix("a", "1.0.1", "package.json", "applied"),
                fix("b", "1.0.2", "package.json", "pending_relock"),
                fix("c", "1.0.3", "package.json", "already_satisfied"),
            ]),
        );
        assert_eq!(security.counts.fixes, 3);
        assert_eq!(security.counts.pending_relock, 1);
        assert_eq!(security.counts.advisories, 3);
        assert!(security.unfixable.is_empty() && security.attention.is_empty());
    }

    /// Two ecosystems can each publish a package of the same name. Each fix
    /// carries its own ecosystem's advisories, and a package left vulnerable
    /// in one ecosystem does not hold back the other's resolved advisories.
    #[test]
    fn a_same_named_package_in_another_ecosystem_is_a_different_package() {
        let security = read(
            json!([
                vulnerability("shared", "GHSA-npm", "Low"),
                {"package": "shared", "version": "1.0.0", "ecosystem": "PyPI", "id": "PYSEC-py", "severity": "Critical"},
            ]),
            json!([
                fix("shared", "1.0.1", "package.json", "applied"),
                {"package": "shared", "ecosystem": "PyPI", "from_version": "1.0.0", "path": "requirements.txt",
                 "status": "blocked", "error": "a manifest requirement excludes it"},
            ]),
        );
        assert_eq!(security.fixes.len(), 1);
        assert_eq!(security.fixes[0].advisories, vec!["GHSA-npm".to_string()]);
        assert_eq!(security.fixes[0].severity, "Low");
        assert_eq!(
            security.counts.advisories, 1,
            "the npm advisory is resolved"
        );
        assert_eq!(security.resolved_packages(), vec!["shared"]);
    }

    #[test]
    fn a_package_left_vulnerable_anywhere_resolves_none_of_its_advisories() {
        let security = read(
            json!([
                vulnerability("lru", "RUSTSEC-1", "High"),
                vulnerability("time", "RUSTSEC-2", "Low"),
            ]),
            json!([
                fix("lru", "0.16.3", "a/Cargo.toml", "applied"),
                {"ecosystem": "npm", "package": "lru", "from_version": "0.16.0", "path": "Cargo.lock", "status": "blocked",
                 "error": "ratatui-core requires lru ^0.16.0, <0.16.2"},
                fix("time", "0.3.36", "Cargo.lock", "applied"),
            ]),
        );
        assert_eq!(
            security.counts.fixes, 2,
            "the lru manifest fix still applied"
        );
        assert_eq!(security.counts.blocked, 1);
        assert_eq!(
            security.counts.advisories, 1,
            "only time's advisory is resolved"
        );
        assert_eq!(
            security.attention[0].reason,
            "security fix blocked: ratatui-core requires lru ^0.16.0, <0.16.2"
        );
    }

    #[test]
    fn an_already_satisfied_manifest_resolves_nothing_until_the_lockfile_is_regenerated() {
        let fixes = json!([fix(
            "lockonly",
            "0.49.1",
            "pyproject.toml",
            "already_satisfied"
        )]);
        let advisory = json!([vulnerability("lockonly", "GHSA-1", "High")]);

        let relocked = read_with_lock(advisory.clone(), fixes.clone(), true);
        assert_eq!(
            relocked.counts.fixes, 1,
            "the relock moved the locked version"
        );
        assert_eq!(relocked.counts.advisories, 1);
        assert!(relocked.attention.is_empty());

        let unlocked = read_with_lock(advisory, fixes, false);
        assert_eq!(
            unlocked.counts.fixes, 0,
            "nothing was written and nothing relocked"
        );
        assert_eq!(unlocked.counts.advisories, 0);
        assert_eq!(unlocked.counts.skipped, 1);
        assert!(unlocked.resolved_packages().is_empty());
        assert_eq!(
            unlocked.attention[0].reason,
            "security fix needs lockfile regeneration (lock: true): the manifest already requires the fix, but the lockfile still records the vulnerable version"
        );
    }

    #[test]
    fn a_fix_upd_did_not_write_needs_attention_rather_than_claiming_no_fix_exists() {
        let security = read(
            json!([
                vulnerability("requests", "GHSA-1", "High"),
                vulnerability("gone", "GHSA-2", "Low"),
            ]),
            json!([
                {"ecosystem": "npm", "package": "requests", "from_version": "1.0.0", "to_version": "2.28.0",
                 "path": "requirements.txt", "status": "unfixable",
                 "error": "pinned to 1.0.0 by configuration; the fix needs 2.28.0 or later"},
                {"ecosystem": "npm", "package": "gone", "from_version": "2.0.0", "status": "unfixable",
                 "error": "GHSA-2 has no fixed version"},
            ]),
        );
        assert_eq!(security.counts.not_applied, 1);
        assert_eq!(
            security.counts.unfixable, 1,
            "only the advisory with no fix"
        );
        assert_eq!(security.unfixable[0].package, "gone");
        assert_eq!(security.counts.advisories, 0);
        assert_eq!(security.attention.len(), 1);
        assert_eq!(security.attention[0].package, "requests");
        assert_eq!(security.attention[0].path, "requirements.txt");
        assert_eq!(
            security.attention[0].reason,
            "security fix to 2.28.0 not applied: pinned to 1.0.0 by configuration; the fix needs 2.28.0 or later"
        );
    }

    #[test]
    fn a_young_release_a_fix_relock_pulled_in_is_listed_without_reopening_the_fix() {
        let security = Security::from_report(
            &json!({
                "vulnerabilities": [vulnerability("lodash", "GHSA-1", "High")],
                "fixes": [fix("lodash", "4.17.21", "package.json", "applied")],
                "lockfile_cooldown": [
                    {"lockfile": "package-lock.json", "package": "newdep", "version": "1.0.0",
                     "published_at": "2026-09-29T08:00:00Z", "cooldown": "7d"},
                ],
                "summary": {"errors": 0},
            }),
            true,
        )
        .unwrap();
        assert_eq!(security.counts.fixes, 1);
        assert_eq!(security.counts.advisories, 1, "the fix still resolves");
        assert_eq!(security.resolved_packages(), ["lodash"]);
        assert!(
            security.attention.is_empty(),
            "a young release was changed, not blocked"
        );
        assert_eq!(security.counts.young, 1);
        assert_eq!(
            security.young,
            [YoungRow {
                package: "newdep".to_string(),
                version: "1.0.0".to_string(),
                published_at: "2026-09-29T08:00:00Z".to_string(),
                cooldown: "7d".to_string(),
                lockfile: "package-lock.json".to_string(),
            }]
        );
        assert!(!security.is_empty());
        assert_eq!(
            security.warnings(),
            [
                "warning: package-lock.json locks newdep 1.0.0, released 2026-09-29T08:00:00Z, inside the 7d cooldown; a security fix's relock locked it"
            ]
        );
    }

    #[test]
    fn a_report_without_young_releases_lists_none() {
        let security = Security::from_report(
            &json!({
                "vulnerabilities": [vulnerability("lodash", "GHSA-1", "High")],
                "fixes": [fix("lodash", "4.17.21", "package.json", "applied")],
                "summary": {"errors": 0},
            }),
            true,
        )
        .unwrap();
        assert!(security.attention.is_empty());
        assert!(security.young.is_empty());
        assert_eq!(security.young_section(), "");
    }

    #[test]
    fn only_packages_with_no_vulnerable_occurrence_left_are_resolved() {
        let security = read(
            json!([
                vulnerability("lru", "RUSTSEC-1", "High"),
                vulnerability("time", "RUSTSEC-2", "Low"),
            ]),
            json!([
                fix("lru", "0.16.3", "a/Cargo.toml", "applied"),
                {"ecosystem": "npm", "package": "lru", "from_version": "0.16.0", "path": "Cargo.lock", "status": "blocked"},
                fix("time", "0.3.36", "Cargo.lock", "applied"),
                fix("time", "0.3.36", "b/Cargo.toml", "applied"),
            ]),
        );
        assert_eq!(security.fixed_packages(), ["lru", "time"]);
        assert_eq!(security.resolved_packages(), ["time"]);
    }

    #[test]
    fn a_skipped_or_unknown_status_is_never_claimed_as_a_fix() {
        let security = read(
            json!([vulnerability("lru", "RUSTSEC-1", "Critical")]),
            json!([
                fix("lru", "0.12.1", "Cargo.lock", "skipped"),
                fix("left-pad", "", "package.json", "some_future_status"),
                {"ecosystem": "npm", "package": "gone", "from_version": "2.0.0", "status": "unfixable"},
            ]),
        );
        assert_eq!(security.counts.fixes, 0);
        assert_eq!(security.counts.skipped, 1);
        assert_eq!(security.counts.unfixable, 2);
        assert_eq!(security.counts.advisories, 0);
        assert!(security.attention[0].reason.contains("lock: true"));
        let gone = security
            .unfixable
            .iter()
            .find(|row| row.package == "gone")
            .unwrap();
        assert_eq!(gone.reason, "no release resolves its advisories");
        assert_eq!(gone.severity, UNKNOWN);
    }

    #[test]
    fn duplicate_fixes_collapse_and_rows_sort_by_package_then_file() {
        let security = read(
            json!([]),
            json!([
                fix("zod", "3.22.3", "web/package.json", "applied"),
                fix("axios", "1.6.0", "web/package.json", "applied"),
                fix("axios", "1.6.0", "api/package.json", "applied"),
                fix("axios", "1.6.0", "api/package.json", "applied"),
            ]),
        );
        let order: Vec<(&str, &str)> = security
            .fixes
            .iter()
            .map(|row| (row.package.as_str(), row.path.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("axios", "api/package.json"),
                ("axios", "web/package.json"),
                ("zod", "web/package.json"),
            ]
        );
        assert_eq!(security.fixed_packages(), ["axios", "zod"]);
    }

    #[test]
    fn a_package_takes_its_highest_severity_and_every_advisory() {
        let security = read(
            json!([
                vulnerability("lodash", "GHSA-2", "Medium"),
                vulnerability("lodash", "GHSA-1", "Critical"),
                vulnerability("lodash", "GHSA-3", "weird"),
                vulnerability("lodash", "GHSA-4", "Low"),
                vulnerability("lodash", "GHSA-1", "Critical"),
            ]),
            json!([fix("lodash", "4.17.21", "package.json", "applied")]),
        );
        let row = &security.fixes[0];
        assert_eq!(row.severity, "Critical");
        assert_eq!(row.advisories, ["GHSA-1", "GHSA-2", "GHSA-3", "GHSA-4"]);
        assert_eq!(security.counts.advisories, 4);
        assert!(
            security
                .fixes_section()
                .contains("<code>GHSA-1</code>, <code>GHSA-2</code>, <code>GHSA-3</code> +1 more")
        );
    }

    #[test]
    fn a_fix_without_a_target_names_the_lockfile_floor() {
        let security = read(
            json!([]),
            json!([{"ecosystem": "npm", "package": "time", "from_version": "0.3.20", "path": "Cargo.lock", "status": "applied"}]),
        );
        assert_eq!(security.fixes[0].to, "resolved lockfile floor");
    }

    #[test]
    fn the_summary_line_counts_every_outcome_and_the_report_errors() {
        let report = json!({
            "fixes": [
                fix("a", "1.0.1", "package.json", "pending_relock"),
                fix("b", "1.0.1", "Cargo.lock", "skipped"),
            ],
            "summary": {"errors": 0},
        });
        let security = Security::from_report(&report, true).unwrap();
        assert_eq!(
            security.summary_line(&report).unwrap(),
            "upd audit: 1 fixed, 1 pending relock, 1 skipped, 0 blocked, 0 not applied, 0 without a fix, 0 error(s)"
        );
        assert_eq!(
            security.warnings(),
            [
                "warning: b 1.0.0 in Cargo.lock: security fix needs lockfile regeneration (lock: true): it pins a lockfile entry"
            ]
        );
    }

    #[test]
    fn a_fixes_list_that_is_not_an_array_is_a_shape_error() {
        assert!(Security::from_report(&json!({"fixes": "none"}), true).is_err());
        assert!(Security::from_report(&json!({"fixes": [], "vulnerabilities": 3}), true).is_err());
    }

    fn recheck(vulnerabilities: Value) -> Value {
        json!({"vulnerabilities": vulnerabilities, "summary": {"errors": 0}})
    }

    fn found(package: &str, version: &str, ecosystem: &str, id: &str) -> Value {
        json!({"package": package, "version": version, "ecosystem": ecosystem, "id": id, "severity": "High"})
    }

    #[test]
    fn a_fixed_dependency_the_update_moved_to_an_affected_release_is_reintroduced() {
        let mut security = read(
            json!([
                vulnerability("lodash", "GHSA-1", "High"),
                vulnerability("semver", "GHSA-2", "Low"),
            ]),
            json!([
                fix("lodash", "4.17.21", "package.json", "applied"),
                fix("semver", "7.5.2", "package.json", "applied"),
            ]),
        );
        assert!(security.is_recheckable());
        assert_eq!(security.counts.advisories, 2);

        security
            .apply_recheck(&recheck(json!([found(
                "lodash", "4.17.22", "npm", "GHSA-9"
            )])))
            .unwrap();

        assert_eq!(security.counts.reintroduced, 1);
        assert_eq!(security.counts.advisories, 1);
        assert_eq!(security.resolved_packages(), ["semver"]);
        assert_eq!(
            security.reintroduced,
            [ReintroducedRow {
                package: "lodash".to_string(),
                version: "4.17.22".to_string(),
                advisories: vec!["GHSA-9".to_string()],
                severity: "High".to_string(),
                reason: "the dependency update moved lodash to 4.17.22, which GHSA-9 still affects"
                    .to_string(),
                fixed_in: vec!["package.json".to_string()],
                ecosystem: "npm".to_string(),
            }]
        );
        assert!(
            security
                .reintroduced_section()
                .contains("| <code>lodash</code> | <code>4.17.22</code> |")
        );
        assert!(
            security
                .evidence_lines()
                .contains("- Vulnerable again after the dependency update: 1\n")
        );
    }

    #[test]
    fn a_fixed_release_the_recheck_still_finds_affected_is_not_said_to_have_moved() {
        let mut security = read(
            json!([vulnerability("lodash", "GHSA-1", "High")]),
            json!([fix("lodash", "4.17.21", "package.json", "applied")]),
        );

        security
            .apply_recheck(&recheck(json!([found(
                "lodash", "4.17.21", "npm", "GHSA-9"
            )])))
            .unwrap();

        assert_eq!(
            security.reintroduced[0].reason,
            "after the dependency update, the audit finds lodash 4.17.21 still affected by GHSA-9"
        );
        assert_eq!(security.counts.advisories, 0);
    }

    #[test]
    fn a_recheck_matches_fixes_by_ecosystem_as_well_as_name() {
        let mut security = read(
            json!([vulnerability("lodash", "GHSA-1", "High")]),
            json!([fix("lodash", "4.17.21", "package.json", "applied")]),
        );

        security
            .apply_recheck(&recheck(json!([
                found("lodash", "1.0.0", "PyPI", "PYSEC-1"),
                found("other", "1.0.0", "npm", "GHSA-9"),
            ])))
            .unwrap();

        assert!(security.reintroduced.is_empty());
        assert_eq!(security.counts.reintroduced, 0);
        assert_eq!(security.counts.advisories, 1);
        assert_eq!(security.reintroduced_section(), "");
    }

    #[test]
    fn only_resolved_fixes_are_rechecked() {
        // A package another occurrence left vulnerable is not resolved, so
        // there is nothing to confirm.
        let open = read(
            json!([vulnerability("lodash", "GHSA-1", "High")]),
            json!([
                fix("lodash", "4.17.21", "a/package.json", "applied"),
                fix("lodash", "4.17.21", "b/package.json", "blocked"),
            ]),
        );
        assert!(!open.is_recheckable());

        for status in ["applied", "pending_relock"] {
            let resolved = read_with_lock(
                json!([vulnerability("lodash", "GHSA-1", "High")]),
                json!([fix("lodash", "4.17.21", "package.json", status)]),
                false,
            );
            assert!(resolved.is_recheckable(), "{status}");
        }
    }

    /// Without a lockfile the audit reads the manifest, so a manifest-only
    /// fix is still rechecked; a lockfile awaiting regeneration keeps the
    /// version the security step found, which is not the update's doing.
    #[test]
    fn a_fix_awaiting_a_relock_is_reintroduced_only_at_a_version_the_security_step_did_not_find() {
        let pending = || {
            read_with_lock(
                json!([vulnerability("lodash", "GHSA-1", "High")]),
                json!([fix("lodash", "4.17.21", "package.json", "pending_relock")]),
                false,
            )
        };

        let mut stale = pending();
        stale
            .apply_recheck(&recheck(json!([found("lodash", "1.0.0", "npm", "GHSA-1")])))
            .unwrap();
        assert!(stale.reintroduced.is_empty());
        assert_eq!(stale.counts.advisories, 1);

        let mut moved = pending();
        moved
            .apply_recheck(&recheck(json!([found(
                "lodash", "4.17.22", "npm", "GHSA-9"
            )])))
            .unwrap();
        assert_eq!(moved.counts.reintroduced, 1);
        assert_eq!(moved.reintroduced[0].version, "4.17.22");
        assert_eq!(moved.counts.advisories, 0);
    }

    #[test]
    fn same_named_dependencies_in_two_ecosystems_are_reintroduced_separately() {
        let fixed = |ecosystem: &str, path: &str| json!({"package": "shared", "ecosystem": ecosystem, "from_version": "0.9.0", "to_version": "1.0.0", "path": path, "status": "applied"});
        let mut security = Security::from_report(
            &json!({
                "vulnerabilities": [
                    {"package": "shared", "version": "0.9.0", "ecosystem": "npm", "id": "GHSA-1"},
                    {"package": "shared", "version": "0.9.0", "ecosystem": "PyPI", "id": "PYSEC-1"},
                ],
                "fixes": [fixed("npm", "package.json"), fixed("PyPI", "requirements.txt")],
                "summary": {"errors": 0},
            }),
            true,
        )
        .unwrap();

        security
            .apply_recheck(&recheck(json!([
                found("shared", "1.1.0", "npm", "GHSA-2"),
                found("shared", "1.1.0", "PyPI", "PYSEC-2"),
            ])))
            .unwrap();

        assert_eq!(security.counts.reintroduced, 2);
        let paths: BTreeSet<&[String]> = security
            .reintroduced
            .iter()
            .map(|row| row.fixed_in.as_slice())
            .collect();
        assert_eq!(
            paths,
            BTreeSet::from([
                &["package.json".to_string()][..],
                &["requirements.txt".to_string()][..]
            ])
        );
    }

    #[test]
    fn a_dependency_fixed_in_several_files_names_each_it_was_fixed_in() {
        let fixed = |path: &str| json!({"package": "lodash", "ecosystem": "npm", "from_version": "4.17.20", "to_version": "4.17.21", "path": path, "status": "applied"});
        let mut security = Security::from_report(
            &json!({
                "vulnerabilities": [
                    {"package": "lodash", "version": "4.17.20", "ecosystem": "npm", "id": "GHSA-1"},
                ],
                "fixes": [fixed("web/package.json"), fixed("api/package.json")],
                "summary": {"errors": 0},
            }),
            true,
        )
        .unwrap();

        security
            .apply_recheck(&recheck(json!([found(
                "lodash", "4.17.22", "npm", "GHSA-2"
            )])))
            .unwrap();

        assert_eq!(security.counts.reintroduced, 1);
        assert_eq!(
            security.reintroduced[0].fixed_in,
            ["api/package.json", "web/package.json"]
        );
        let section = security.reintroduced_section();
        assert!(
            section.contains("<code>api/package.json</code>, <code>web/package.json</code> |"),
            "{section}"
        );
        assert_eq!(
            security.recheck_warnings(),
            [
                "warning: lodash (fixed in api/package.json, web/package.json): the dependency update moved lodash to 4.17.22, which GHSA-2 still affects"
            ]
        );
    }

    const UNCHECKED: &str = "package-lock.json: companion 1.0.0 could not be checked against the 7d cooldown (the registry lookup failed)";

    #[test]
    fn a_warning_from_the_security_audit_needs_attention() {
        let security = Security::from_report(
            &json!({
                "vulnerabilities": [vulnerability("lodash", "GHSA-1", "High")],
                "fixes": [fix("lodash", "4.17.21", "package.json", "applied")],
                "warnings": [UNCHECKED],
                "summary": {"errors": 0},
            }),
            true,
        )
        .unwrap();

        assert_eq!(security.audit_warnings, [UNCHECKED]);
        assert_eq!(security.counts.audit_warnings, 1);
        assert!(
            security
                .warnings()
                .contains(&format!("warning: {UNCHECKED}")),
            "{:?}",
            security.warnings()
        );
        let section = security.audit_warnings_section();
        assert!(
            section.contains(&format!("- {}", md(UNCHECKED))),
            "{section}"
        );
        assert!(
            security
                .evidence_lines()
                .contains("- Security audit warnings: 1\n")
        );
    }

    #[test]
    fn a_security_audit_without_warnings_adds_nothing() {
        let security = read(
            json!([vulnerability("lodash", "GHSA-1", "High")]),
            json!([fix("lodash", "4.17.21", "package.json", "applied")]),
        );
        assert!(security.audit_warnings.is_empty());
        assert_eq!(security.audit_warnings_section(), "");
        assert!(!security.evidence_lines().contains("audit warnings"));
        let serialized = serde_json::to_value(&security).unwrap();
        assert!(serialized.get("audit_warnings").is_none(), "{serialized}");
        assert!(
            serialized["counts"].get("audit_warnings").is_none(),
            "{serialized}"
        );
    }

    #[test]
    fn a_warning_from_the_audit_after_the_update_is_added_once() {
        let mut security = Security::from_report(
            &json!({
                "vulnerabilities": [vulnerability("lodash", "GHSA-1", "High")],
                "fixes": [fix("lodash", "4.17.21", "package.json", "applied")],
                "warnings": [UNCHECKED],
                "summary": {"errors": 0},
            }),
            true,
        )
        .unwrap();
        let later =
            "go.mod: go 1.16 predates module graph pruning; indirect dependencies were not audited";

        security
            .apply_recheck(&json!({
                "vulnerabilities": [],
                "warnings": [UNCHECKED, later],
                "summary": {"errors": 0},
            }))
            .unwrap();

        assert_eq!(security.audit_warnings, [UNCHECKED, later]);
        assert_eq!(security.counts.audit_warnings, 2);
        assert_eq!(security.recheck_warnings(), [format!("warning: {later}")]);
    }
}
