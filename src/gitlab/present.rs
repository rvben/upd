//! Review presentation for a dependency merge request.
//!
//! The raw `upd update --format json` report stays the canonical record; this
//! module derives a bounded model from it (the `upd-presentation.json`
//! artifact), a concise title and the Markdown description a reviewer reads.
//!
//! Report text is untrusted: package names, versions and messages come from
//! manifests and registries. Every such value passes through [`clean`] before
//! it enters the model and through [`md`] before it enters Markdown, so it can
//! neither break a table nor inject markup.
//!
//! The report is read loosely where a missing or oddly shaped section simply
//! means "nothing here", and strictly where a shape cannot be interpreted: a
//! file entry that is not an object is an error rather than an empty row.

use std::cmp::Ordering;

use serde::Serialize;
use serde_json::Value;

/// Largest description, in bytes, sent to GitLab before falling back to the
/// compact summary.
pub const DESCRIPTION_BUDGET: usize = 32 * 1024;

const UNKNOWN_DEPENDENCY: &str = "unknown dependency";
const UNKNOWN_FILE: &str = "unknown file";
const UNKNOWN: &str = "unknown";

const BRAND: &str = "<p><a href=\"https://github.com/rvben/upd\"><img src=\"https://raw.githubusercontent.com/rvben/upd/84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg\" alt=\"upd\" width=\"96\"></a></p>";

/// A report shape the presentation cannot interpret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportShapeError(pub String);

impl std::fmt::Display for ReportShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unexpected update report shape: {}", self.0)
    }
}

impl std::error::Error for ReportShapeError {}

type Shaped<T> = Result<T, ReportShapeError>;

/// Job facts the presentation needs beyond the report itself.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    /// Freshness input as configured; empty defers to repository configuration.
    pub min_age: &'a str,
    /// Bump ceiling as configured; empty defers to repository configuration.
    pub max_bump: &'a str,
    pub lock: bool,
    pub auto_merge: bool,
    pub validation_configured: bool,
    /// Whether the staged tree differs from the default branch.
    pub changed: bool,
    /// Repository paths the staged tree changes.
    pub changed_paths: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateRow {
    pub package: String,
    pub current: String,
    pub latest: String,
    pub bump: String,
    pub path: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnnotationRow {
    pub package: String,
    pub version: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NormalizedRow {
    pub package: String,
    pub previous: String,
    pub new: String,
    pub version: String,
    pub path: String,
    /// Carried through from the report unchanged; it orders rows only.
    pub line: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PolicyRow {
    pub kind: String,
    pub package: String,
    pub selected: String,
    pub available: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockedRow {
    pub package: String,
    pub current: String,
    pub reason: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Counts {
    pub updates: usize,
    pub updates_major: usize,
    pub updates_minor: usize,
    pub updates_patch: usize,
    pub updates_revision: usize,
    pub updates_review_worthy: usize,
    pub updates_quiet: usize,
    pub annotations: usize,
    pub normalized: usize,
    pub files_changed: usize,
    pub policy_holds: usize,
    pub blocked: usize,
    /// Copied from the report summary as found.
    pub warnings: Value,
    /// Copied from the report summary as found.
    pub not_examined: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Policy {
    pub min_age: String,
    pub max_bump: String,
    pub lockfile_regeneration: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Validation {
    pub repository_command_configured: bool,
    pub proposal_integrity_passed: bool,
}

/// The bounded review model written to `upd-presentation.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Presentation {
    pub schema: u32,
    pub state: &'static str,
    pub changed: bool,
    pub updates: Vec<UpdateRow>,
    pub annotations: Vec<AnnotationRow>,
    pub normalized: Vec<NormalizedRow>,
    pub policy_holds: Vec<PolicyRow>,
    pub blocked: Vec<BlockedRow>,
    pub changed_paths: Vec<String>,
    pub counts: Counts,
    pub policy: Policy,
    pub validation: Validation,
    pub auto_merge_requested: bool,
    pub title: String,
}

impl Presentation {
    /// Builds the model from an update report.
    pub fn from_report(report: &Value, context: &Context<'_>) -> Shaped<Self> {
        let updates = update_rows(report)?;
        let annotations = annotation_rows(report)?;
        let normalized = normalized_rows(report)?;
        let policy_holds = policy_rows(report)?;
        let blocked = blocked_rows(report)?;

        let mut changed_paths: Vec<String> = context
            .changed_paths
            .iter()
            .map(|path| clean_str(path, 160))
            .collect();
        changed_paths.sort();
        changed_paths.dedup();

        let summary = field(report, "summary")?;
        let bump_count = |level: &str| {
            updates
                .iter()
                .filter(|row| row.bump.to_ascii_lowercase() == level)
                .count()
        };
        let updates_patch = bump_count("patch");
        let counts = Counts {
            updates: updates.len(),
            updates_major: bump_count("major"),
            updates_minor: bump_count("minor"),
            updates_patch,
            updates_revision: bump_count("revision"),
            updates_review_worthy: updates.len() - updates_patch,
            updates_quiet: updates_patch,
            annotations: annotations.len(),
            normalized: normalized.len(),
            files_changed: context.changed_paths.len(),
            policy_holds: policy_holds.len(),
            blocked: blocked.len(),
            warnings: or_else(field(summary, "warnings")?, Value::from(0)),
            not_examined: or_else(field(summary, "not_examined")?, Value::from(0)),
        };

        let state = if context.changed {
            "ready"
        } else if !blocked.is_empty() {
            "blocked_no_change"
        } else {
            "clean"
        };

        let mut presentation = Self {
            schema: 1,
            state,
            changed: context.changed,
            updates,
            annotations,
            normalized,
            policy_holds,
            blocked,
            changed_paths,
            counts,
            policy: Policy {
                min_age: configured_or_repository(context.min_age, 80),
                max_bump: configured_or_repository(context.max_bump, 32),
                lockfile_regeneration: context.lock,
            },
            validation: Validation {
                repository_command_configured: context.validation_configured,
                proposal_integrity_passed: !context.changed,
            },
            auto_merge_requested: context.auto_merge,
            title: String::new(),
        };
        presentation.title = presentation.derive_title();
        Ok(presentation)
    }

    /// Pretty JSON artifact, newline-terminated.
    pub fn to_artifact(&self) -> String {
        let mut json = serde_json::to_string_pretty(self)
            .expect("presentation is plain data and always serializes");
        json.push('\n');
        json
    }

    fn derive_title(&self) -> String {
        let workflow_only = !self.changed_paths.is_empty()
            && self
                .changed_paths
                .iter()
                .all(|path| path.starts_with(".github/workflows/"));
        let prefix = if workflow_only {
            "ci(deps)"
        } else {
            "chore(deps)"
        };
        let counts = &self.counts;
        let title = if counts.updates == 1 && counts.normalized == 0 {
            let update = &self.updates[0];
            if is_title_package(&update.package) && is_title_version(&update.latest) {
                format!("{prefix}: refresh {} to {}", update.package, update.latest)
            } else {
                format!("{prefix}: refresh dependency")
            }
        } else if counts.updates > 0 && counts.normalized > 0 {
            format!(
                "{prefix}: prepare {} dependency changes",
                counts.updates + counts.normalized
            )
        } else if counts.updates > 1 {
            format!("{prefix}: refresh {} dependencies", counts.updates)
        } else if counts.normalized == 1 {
            if is_title_package(&self.normalized[0].package) {
                format!(
                    "{prefix}: normalize {} specifier",
                    self.normalized[0].package
                )
            } else {
                format!("{prefix}: normalize dependency specifier")
            }
        } else if counts.normalized > 1 {
            format!(
                "{prefix}: normalize {} dependency specifiers",
                counts.normalized
            )
        } else if counts.annotations == 1 {
            format!("{prefix}: annotate dependency metadata")
        } else if counts.annotations > 1 {
            format!(
                "{prefix}: annotate {} dependency declarations",
                counts.annotations
            )
        } else {
            format!("{prefix}: refresh dependency metadata")
        };
        clean_str(&title, 72)
    }

    /// The merge-request description: the full review when it fits the
    /// budget, otherwise a compact summary pointing at the pipeline artifact.
    /// Newline-terminated, as written to `upd-mr-description.md`.
    pub fn description(&self) -> String {
        let full = format!("{}\n", self.full_description());
        if full.len() > DESCRIPTION_BUDGET {
            format!("{}\n", self.fallback_description())
        } else {
            full
        }
    }

    fn full_description(&self) -> String {
        format!(
            "{BRAND}\n\n{}\n\n{}\n\n{}\n\n{}{}{}\n\n{}\n\n> Rebuilt from the latest default branch. The project pipeline remains the final merge boundary.\n\n---\nPrepared by [upd](https://github.com/rvben/upd).",
            self.header(),
            self.facts(),
            self.changes(),
            self.confidence(),
            self.held(),
            self.blocked_section(),
            self.evidence(),
        )
    }

    fn result_summary(&self) -> String {
        let counts = &self.counts;
        let mut parts = Vec::new();
        if counts.updates > 0 {
            parts.push(format!(
                "{} dependency {}",
                counts.updates,
                plural(counts.updates, "update", "updates")
            ));
        }
        if counts.normalized > 0 {
            parts.push(format!(
                "{} normalized {}",
                counts.normalized,
                plural(counts.normalized, "specifier", "specifiers")
            ));
        }
        if parts.is_empty() {
            format!(
                "{} dependency metadata {}",
                counts.annotations,
                plural(counts.annotations, "annotation", "annotations")
            )
        } else {
            parts.join(" and ")
        }
    }

    fn validation_phrase(&self) -> &'static str {
        if self.validation.repository_command_configured {
            "passed project validation and proposal-integrity checks"
        } else {
            "passed proposal-integrity checks"
        }
    }

    fn version_boundary(&self) -> String {
        const NORMALIZED: &str = "Normalized specifiers are replaced as a whole; see the Normalized specifiers section for the versions written.";
        let counts = &self.counts;
        if counts.updates == 0 && counts.normalized > 0 {
            return NORMALIZED.to_string();
        }
        let majors = if counts.updates_major == 0 {
            "No major-version jumps".to_string()
        } else {
            format!(
                "Includes {} major-version {}",
                counts.updates_major,
                plural(counts.updates_major, "jump", "jumps")
            )
        };
        if counts.normalized > 0 {
            format!("{majors} among ordinary updates. {NORMALIZED}")
        } else {
            format!("{majors}.")
        }
    }

    fn files_phrase(&self) -> String {
        let files = self.counts.files_changed;
        format!("{files} {}", plural(files, "file", "files"))
    }

    fn header(&self) -> String {
        let counts = &self.counts;
        if counts.blocked > 0 {
            format!(
                "> **A careful upgrade, with follow-up.** upd prepared {} across {} and {}. It stopped short of {} {} it could not change safely.",
                self.result_summary(),
                self.files_phrase(),
                self.validation_phrase(),
                counts.blocked,
                plural(counts.blocked, "dependency", "dependencies"),
            )
        } else {
            format!(
                "> **A tidy upgrade, already prepared.** upd prepared {} across {} and {}. {}",
                self.result_summary(),
                self.files_phrase(),
                self.validation_phrase(),
                self.version_boundary(),
            )
        }
    }

    fn facts(&self) -> String {
        let counts = &self.counts;
        let mut facts = Vec::new();
        if counts.updates > 0 {
            facts.push(format!("{} moved forward", counts.updates));
        }
        if counts.updates_review_worthy > 0 {
            facts.push(format!("{} worth a look", counts.updates_review_worthy));
        }
        if counts.updates_quiet > 0 {
            facts.push(format!(
                "{} quiet {}",
                counts.updates_quiet,
                plural(counts.updates_quiet, "patch", "patches")
            ));
        }
        if counts.normalized > 0 {
            facts.push(format!("{} normalized", counts.normalized));
        }
        if counts.policy_holds > 0 {
            facts.push(format!("{} saved for later", counts.policy_holds));
        }
        if counts.blocked > 0 {
            facts.push(format!(
                "{} {} attention",
                counts.blocked,
                plural(counts.blocked, "needs", "need")
            ));
        }
        facts
            .iter()
            .map(|fact| format!("**{fact}**"))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    fn changes(&self) -> String {
        let review: Vec<&UpdateRow> = self
            .updates
            .iter()
            .filter(|row| !row.bump.eq_ignore_ascii_case("patch"))
            .collect();
        let quiet: Vec<&UpdateRow> = self
            .updates
            .iter()
            .filter(|row| row.bump.eq_ignore_ascii_case("patch"))
            .collect();
        let update_line = |row: &&UpdateRow| {
            format!(
                "| {} | {} | {} | {} | {} |",
                code(&row.package),
                code(&row.current),
                code(&row.latest),
                md(&row.bump),
                code(&row.path)
            )
        };

        let mut out = if !review.is_empty() {
            let mut section = String::from(
                "### Worth a look\n\nYour attention is best spent on these non-patch updates.\n\n| Dependency | Before | After | Change | File |\n|---|---:|---:|---|---|\n",
            );
            section.push_str(&table(&review, 12, update_line));
            if review.len() > 12 {
                section.push_str(&format!(
                    "\n\n_{} more review-worthy updates are preserved in the pipeline artifact._",
                    review.len() - 12
                ));
            }
            section
        } else if self.counts.updates > 0 {
            let rows: Vec<&UpdateRow> = self.updates.iter().collect();
            let mut section = String::from(
                "### What changed\n\n| Dependency | Before | After | Change | File |\n|---|---:|---:|---|---|\n",
            );
            section.push_str(&table(&rows, 12, update_line));
            if self.counts.updates > 12 {
                section.push_str(&format!(
                    "\n\n_{} more updates are preserved in the pipeline artifact._",
                    self.counts.updates - 12
                ));
            }
            section
        } else if self.counts.normalized > 0 {
            String::from(
                "### What changed\n\nupd rewrote dependency specifiers into the configured shape. See the Normalized specifiers section for the versions written.",
            )
        } else {
            String::from(
                "### What changed\n\nupd added or refreshed dependency metadata without changing selected versions.",
            )
        };

        if !review.is_empty() && !quiet.is_empty() {
            out.push_str(&format!(
                "\n\n<details>\n<summary><strong>Quiet patch updates ({})</strong></summary>\n\n| Dependency | Before | After | File |\n|---|---:|---:|---|\n",
                quiet.len()
            ));
            out.push_str(&table(&quiet, 20, |row| {
                format!(
                    "| {} | {} | {} | {} |",
                    code(&row.package),
                    code(&row.current),
                    code(&row.latest),
                    code(&row.path)
                )
            }));
            if quiet.len() > 20 {
                out.push_str(&format!(
                    "\n\n_{} more patch updates are preserved in the pipeline artifact._",
                    quiet.len() - 20
                ));
            }
            out.push_str("\n\n</details>");
        }

        if self.counts.normalized > 0 {
            out.push_str(
                "\n\n### Normalized specifiers\n\nEach specifier below was replaced as a whole, including any range or ceiling it carried.\n\n| Dependency | Before | After | Version | File |\n|---|---:|---:|---:|---|\n",
            );
            out.push_str(&table(&self.normalized, 20, |row| {
                format!(
                    "| {} | {} | {} | {} | {} |",
                    code(&row.package),
                    code(&row.previous),
                    code(&row.new),
                    code(&row.version),
                    code(&row.path)
                )
            }));
            if self.counts.normalized > 20 {
                out.push_str(&format!(
                    "\n\n_{} more normalized specifiers are preserved in the pipeline artifact._",
                    self.counts.normalized - 20
                ));
            }
        }

        if self.counts.annotations > 0 {
            out.push_str(&format!(
                "\n\n<details>\n<summary><strong>Dependency metadata ({})</strong></summary>\n\n| Dependency | Version | File |\n|---|---:|---|\n",
                self.counts.annotations
            ));
            out.push_str(&table(&self.annotations, 20, |row| {
                format!(
                    "| {} | {} | {} |",
                    code(&row.package),
                    code(&row.version),
                    code(&row.path)
                )
            }));
            out.push_str("\n\n</details>");
        }
        out
    }

    fn held(&self) -> String {
        let holds = self.counts.policy_holds;
        if holds == 0 {
            return String::new();
        }
        let mut out = format!(
            "\n\n<details>\n<summary><strong>Saved for a deliberate upgrade ({holds})</strong></summary>\n\nupd left these releases unchanged because they sit outside this project\u{2019}s current update policy.\n\n| Dependency | Selected | Available | Policy | File |\n|---|---:|---:|---|---|\n"
        );
        out.push_str(&table(&self.policy_holds, 20, |row| {
            format!(
                "| {} | {} | {} | {} | {} |",
                code(&row.package),
                code(&row.selected),
                code(&row.available),
                md(&row.kind),
                code(&row.path)
            )
        }));
        if holds > 20 {
            out.push_str(&format!(
                "\n\n_{} more policy decisions are preserved in the pipeline artifact._",
                holds - 20
            ));
        }
        out.push_str("\n\n</details>");
        out
    }

    fn blocked_section(&self) -> String {
        let blocked = self.counts.blocked;
        if blocked == 0 {
            return String::new();
        }
        let mut out = String::from(
            "\n\n### Needs attention\n\n> These dependencies were not changed because upd could not do so safely.\n\n| Dependency | Current | Reason | File |\n|---|---:|---|---|\n",
        );
        out.push_str(&table(&self.blocked, 12, |row| {
            format!(
                "| {} | {} | {} | {} |",
                code(&row.package),
                code(&row.current),
                md(&row.reason),
                code(&row.path)
            )
        }));
        if blocked > 12 {
            out.push_str(&format!(
                "\n\n_{} more blocked decisions are preserved in the pipeline artifact._",
                blocked - 12
            ));
        }
        out
    }

    fn evidence(&self) -> String {
        let counts = &self.counts;
        let auto_merge = if self.auto_merge_requested {
            "requested; project requirements still control readiness"
        } else {
            "off"
        };
        format!(
            "<details>\n<summary><strong>Proof and provenance</strong></summary>\n\n- Applied updates: {}\n- Change mix: {} major, {} minor, {} patch, {} revision\n- Normalized specifiers: {}\n- Dependency annotations: {}\n- Held back by policy: {}\n- Blocked: {}\n- Not examined: {}\n- Warnings: {}\n- Auto-merge: {auto_merge}\n- Full update report and presentation model retained in the pipeline artifact\n\n</details>",
            counts.updates,
            counts.updates_major,
            counts.updates_minor,
            counts.updates_patch,
            counts.updates_revision,
            counts.normalized,
            counts.annotations,
            counts.policy_holds,
            counts.blocked,
            interpolate(&counts.not_examined),
            interpolate(&counts.warnings),
        )
    }

    fn scope(&self) -> String {
        let files = self.counts.files_changed;
        let paths = |take: usize| {
            self.changed_paths
                .iter()
                .take(take)
                .map(|path| code(path))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if files == 0 {
            "No repository files changed".to_string()
        } else if files <= 3 {
            paths(usize::MAX)
        } else {
            format!("{} and {} more", paths(3), files - 3)
        }
    }

    fn confidence(&self) -> String {
        let counts = &self.counts;
        let configured = self.validation.repository_command_configured;
        let heading = if counts.blocked == 0 && counts.updates_major == 0 && configured {
            "Why this is a comfortable review"
        } else {
            "What upd verified"
        };
        let validation = if configured {
            "Project validation passed"
        } else {
            "No project-specific command was configured"
        };
        let lockfiles = if self.policy.lockfile_regeneration {
            "on"
        } else {
            "off"
        };
        format!(
            "### {heading}\n\n- **Validation:** {validation}; proposal integrity passed.\n- **Scope:** {}.\n- **Version boundary:** {}\n- **Policy:** Freshness {}; maximum bump {}; lockfile regeneration {lockfiles}.",
            self.scope(),
            self.version_boundary(),
            code(&self.policy.min_age),
            code(&self.policy.max_bump),
        )
    }

    fn fallback_description(&self) -> String {
        let counts = &self.counts;
        let changes = counts.updates + counts.normalized;
        let files = self.files_phrase();
        let header = if counts.blocked > 0 {
            format!(
                "> **A careful upgrade, with follow-up.** upd prepared {changes} dependency {}; {} {} attention.",
                plural(changes, "change", "changes"),
                counts.blocked,
                plural(counts.blocked, "dependency needs", "dependencies need"),
            )
        } else if counts.updates_major > 0 {
            format!(
                "> **A substantial upgrade, prepared for review.** upd moved {} dependencies forward, including {} major-version {}.",
                counts.updates,
                counts.updates_major,
                plural(counts.updates_major, "jump", "jumps"),
            )
        } else if counts.updates == 0 && counts.normalized > 0 {
            format!(
                "> **A tidy normalization, already prepared.** upd normalized {} dependency {} across {files}.",
                counts.normalized,
                plural(counts.normalized, "specifier", "specifiers"),
            )
        } else if self.validation.repository_command_configured {
            format!(
                "> **A tidy upgrade, already prepared.** upd moved {} dependency {} forward across {files}.",
                counts.updates,
                plural(counts.updates, "update", "updates"),
            )
        } else {
            format!(
                "> **A dependency upgrade, prepared for review.** upd moved {} dependency {} forward across {files}.",
                counts.updates,
                plural(counts.updates, "update", "updates"),
            )
        };
        let validation = if self.validation.repository_command_configured {
            "project validation and proposal integrity passed"
        } else {
            "proposal integrity passed; no project-specific command was configured"
        };
        format!(
            "{BRAND}\n\n{header}\n\n- Major-version jumps: {}\n- Normalized specifiers: {}\n- Saved for a deliberate upgrade: {}\n- Needs attention: {}\n- Validation: {validation}\n\nThe detailed presentation exceeded the configured body budget, so complete decisions and evidence are retained in the pipeline artifact.\n\n> Review the project pipeline before merging.",
            counts.updates_major, counts.normalized, counts.policy_holds, counts.blocked,
        )
    }
}

/// One-line account of an update report for the job log.
pub fn summary_line(report: &Value) -> Shaped<String> {
    let summary = field(report, "summary")?;
    let value = |key: &str| field(summary, key).map(interpolate);
    let or_zero =
        |key: &str| field(summary, key).map(|value| interpolate(&or_else(value, 0.into())));
    Ok(format!(
        "upd: {} update(s), {} normalized specifier(s), {} changed file(s), {} held back, {} capped, {} error(s)",
        value("updates_total")?,
        or_zero("normalized")?,
        value("files_with_changes")?,
        or_zero("held_back")?,
        or_zero("capped")?,
        value("errors")?,
    ))
}

/// Whether the report records no errors. An absent count means none.
pub fn report_is_error_free(report: &Value) -> Shaped<bool> {
    let errors = or_else(field(field(report, "summary")?, "errors")?, 0.into());
    Ok(errors.as_f64() == Some(0.0))
}

fn update_rows(report: &Value) -> Shaped<Vec<UpdateRow>> {
    let mut rows = Vec::new();
    for file in each_optional(field(report, "files")?) {
        let path = clean_or(field(file, "path")?, UNKNOWN_FILE, 160);
        for update in each_optional(field(file, "updates")?) {
            let status = field(update, "status")?;
            let listed = status.is_null()
                || status.as_str() == Some("applied")
                || status.as_str() == Some("pending_relock");
            if !listed {
                continue;
            }
            rows.push(UpdateRow {
                package: clean_or(field(update, "package")?, UNKNOWN_DEPENDENCY, 160),
                current: clean_or(field(update, "current")?, UNKNOWN, 160),
                latest: clean_or(field(update, "latest")?, UNKNOWN, 160),
                bump: clean_or(field(update, "bump")?, UNKNOWN, 32),
                path: path.clone(),
                status: clean_or(status, "applied", 32),
            });
        }
    }
    unique_by(&mut rows, |row| {
        (
            row.package.clone(),
            row.current.clone(),
            row.latest.clone(),
            row.path.clone(),
            row.status.clone(),
        )
    });
    rows.sort_by(|a, b| {
        (&a.package, &a.path, &a.current, &a.latest)
            .cmp(&(&b.package, &b.path, &b.current, &b.latest))
    });
    Ok(rows)
}

fn annotation_rows(report: &Value) -> Shaped<Vec<AnnotationRow>> {
    let mut rows = Vec::new();
    for file in each_optional(field(report, "files")?) {
        let path = clean_or(field(file, "path")?, UNKNOWN_FILE, 160);
        for annotation in each_optional(field(file, "annotations")?) {
            rows.push(AnnotationRow {
                package: clean_or(field(annotation, "package")?, UNKNOWN_DEPENDENCY, 160),
                version: clean_or(field(annotation, "version")?, UNKNOWN, 160),
                path: path.clone(),
            });
        }
    }
    unique_by(&mut rows, |row| {
        (row.package.clone(), row.version.clone(), row.path.clone())
    });
    rows.sort_by(|a, b| (&a.package, &a.path).cmp(&(&b.package, &b.path)));
    Ok(rows)
}

fn normalized_rows(report: &Value) -> Shaped<Vec<NormalizedRow>> {
    let mut rows = Vec::new();
    for file in each_optional(field(report, "files")?) {
        let path = clean_or(field(file, "path")?, UNKNOWN_FILE, 160);
        for entry in each_optional(field(file, "normalized")?) {
            let previous = field(entry, "previous_spec")?;
            rows.push(NormalizedRow {
                package: clean_or(field(entry, "package")?, UNKNOWN_DEPENDENCY, 160),
                previous: if previous.is_null() {
                    "(no specifier)".to_string()
                } else {
                    clean(previous, 160)
                },
                new: clean_or(field(entry, "new_spec")?, UNKNOWN, 160),
                version: clean_or(field(entry, "version")?, UNKNOWN, 160),
                path: path.clone(),
                line: field(entry, "line")?.clone(),
            });
        }
    }
    rows.sort_by(|a, b| {
        a.package
            .cmp(&b.package)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| jq_cmp(&a.line, &b.line))
    });
    Ok(rows)
}

fn policy_rows(report: &Value) -> Shaped<Vec<PolicyRow>> {
    let mut rows = Vec::new();
    let files = each_optional(field(report, "files")?);
    for file in &files {
        let path = clean_or(field(file, "path")?, UNKNOWN_FILE, 160);
        for held in each_or_empty(field(file, "held_back")?)? {
            let chosen = field(held, "chosen")?;
            let selected = if truthy(chosen) {
                chosen
            } else {
                field(held, "current")?
            };
            rows.push(PolicyRow {
                kind: "cooldown".to_string(),
                package: clean_or(field(held, "package")?, UNKNOWN_DEPENDENCY, 160),
                selected: clean_or(selected, UNKNOWN, 160),
                available: clean_or(field(held, "skipped_latest")?, UNKNOWN, 160),
                path: path.clone(),
            });
        }
        for skipped in each_or_empty(field(file, "skipped_by_cooldown")?)? {
            rows.push(PolicyRow {
                kind: "cooldown".to_string(),
                package: clean_or(field(skipped, "package")?, UNKNOWN_DEPENDENCY, 160),
                selected: clean_or(field(skipped, "current")?, UNKNOWN, 160),
                available: clean_or(field(skipped, "skipped_latest")?, UNKNOWN, 160),
                path: path.clone(),
            });
        }
        for capped in each_or_empty(field(file, "capped")?)? {
            rows.push(PolicyRow {
                kind: "bump ceiling".to_string(),
                package: clean_or(field(capped, "package")?, UNKNOWN_DEPENDENCY, 160),
                selected: clean_or(field(capped, "current")?, UNKNOWN, 160),
                available: clean_or(field(capped, "available")?, UNKNOWN, 160),
                path: path.clone(),
            });
        }
    }
    unique_by(&mut rows, |row| {
        (
            row.kind.clone(),
            row.package.clone(),
            row.selected.clone(),
            row.available.clone(),
            row.path.clone(),
        )
    });
    for file in &files {
        let path = clean_or(field(file, "path")?, UNKNOWN_FILE, 160);
        for entry in each_or_empty(field(file, "normalized")?)? {
            let skipped_latest = field(entry, "skipped_latest")?;
            if skipped_latest.is_null() {
                continue;
            }
            rows.push(PolicyRow {
                kind: "cooldown".to_string(),
                package: clean_or(field(entry, "package")?, UNKNOWN_DEPENDENCY, 160),
                selected: clean_or(field(entry, "version")?, UNKNOWN, 160),
                available: clean(skipped_latest, 160),
                path: path.clone(),
            });
        }
    }
    rows.sort_by(|a, b| (&a.kind, &a.package, &a.path).cmp(&(&b.kind, &b.package, &b.path)));
    Ok(rows)
}

fn blocked_rows(report: &Value) -> Shaped<Vec<BlockedRow>> {
    let mut rows = Vec::new();
    let files = each_optional(field(report, "files")?);
    for file in &files {
        let path = clean_or(field(file, "path")?, UNKNOWN_FILE, 160);
        for skipped in each_or_empty(field(file, "skipped")?)? {
            if field(skipped, "status")?.as_str() != Some("blocked") {
                continue;
            }
            let message = field(skipped, "message")?;
            let reason = if truthy(message) {
                message
            } else {
                field(skipped, "reason")?
            };
            rows.push(BlockedRow {
                package: clean_or(field(skipped, "package")?, UNKNOWN_DEPENDENCY, 160),
                current: clean_or(field(skipped, "current")?, UNKNOWN, 160),
                reason: clean_or(reason, "blocked by a safety condition", 240),
                path: path.clone(),
            });
        }
    }
    for file in &files {
        let path = clean_or(field(file, "path")?, UNKNOWN_FILE, 160);
        for update in each_optional(field(file, "updates")?) {
            let status = field(update, "status")?.as_str();
            if status != Some("unfixable") && status != Some("skipped") {
                continue;
            }
            rows.push(BlockedRow {
                package: clean_or(field(update, "package")?, UNKNOWN_DEPENDENCY, 160),
                current: clean_or(field(update, "current")?, UNKNOWN, 160),
                reason: clean_or(
                    field(update, "error")?,
                    "upd could not apply this update safely",
                    240,
                ),
                path: path.clone(),
            });
        }
    }
    unique_by(&mut rows, |row| {
        (
            row.package.clone(),
            row.current.clone(),
            row.reason.clone(),
            row.path.clone(),
        )
    });
    rows.sort_by(|a, b| (&a.package, &a.path, &a.reason).cmp(&(&b.package, &b.path, &b.reason)));
    Ok(rows)
}

fn configured_or_repository(value: &str, limit: usize) -> String {
    if value.is_empty() {
        "repository configuration".to_string()
    } else {
        clean_str(value, limit)
    }
}

/// Reads `key` from an object. An absent key, or a null container, reads as
/// null; any other container cannot have fields and is a shape error.
fn field<'a>(value: &'a Value, key: &str) -> Shaped<&'a Value> {
    static NULL: Value = Value::Null;
    match value {
        Value::Object(map) => Ok(map.get(key).unwrap_or(&NULL)),
        Value::Null => Ok(&NULL),
        other => Err(ReportShapeError(format!(
            "expected an object with \"{key}\", found {}",
            type_name(other)
        ))),
    }
}

/// Members of an optional collection: array elements or object values, and
/// nothing for anything else.
fn each_optional(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(map) => map.values().collect(),
        _ => Vec::new(),
    }
}

/// Members of a collection that may be absent (null or false) but must
/// otherwise be an array or object.
fn each_or_empty(value: &Value) -> Shaped<Vec<&Value>> {
    match value {
        Value::Null | Value::Bool(false) => Ok(Vec::new()),
        Value::Array(_) | Value::Object(_) => Ok(each_optional(value)),
        other => Err(ReportShapeError(format!(
            "expected a list, found {}",
            type_name(other)
        ))),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Anything but null and false.
fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

fn or_else(value: &Value, fallback: Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        fallback
    }
}

fn clean_or(value: &Value, fallback: &str, limit: usize) -> String {
    if truthy(value) {
        clean(value, limit)
    } else {
        clean_str(fallback, limit)
    }
}

/// Text form used when a value is spliced into prose: strings as they are,
/// anything else as compact JSON.
fn interpolate(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Single-line, bounded text for a value: null becomes empty, control and
/// bidirectional-override characters become spaces, whitespace runs collapse
/// to one space, the ends are trimmed, and the result keeps at most `limit`
/// characters.
pub fn clean(value: &Value, limit: usize) -> String {
    match value {
        Value::Null => String::new(),
        other => clean_str(&interpolate(other), limit),
    }
}

fn clean_str(text: &str, limit: usize) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.chars() {
        let ch = if is_neutralized(ch) { ' ' } else { ch };
        if is_space(ch) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(ch);
            in_space = false;
        }
    }
    let trimmed = collapsed.strip_prefix(' ').unwrap_or(&collapsed);
    let trimmed = trimmed.strip_suffix(' ').unwrap_or(trimmed);
    trimmed.chars().take(limit).collect()
}

/// Controls and the bidirectional embedding, override and isolate characters.
fn is_neutralized(ch: char) -> bool {
    matches!(ch, '\u{00}'..='\u{1f}' | '\u{7f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// The whitespace class runs are collapsed over: ASCII whitespace and the
/// Unicode space separators, including line and paragraph separators.
fn is_space(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n'
            | '\u{0b}'
            | '\u{0c}'
            | '\r'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// Escapes text for a Markdown table cell: HTML-significant characters and
/// every Markdown character that could start emphasis, code, a link or a cell
/// boundary become character references.
pub fn md(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            '`' => out.push_str("&#96;"),
            '*' => out.push_str("&#42;"),
            '_' => out.push_str("&#95;"),
            '[' => out.push_str("&#91;"),
            ']' => out.push_str("&#93;"),
            '|' => out.push_str("&#124;"),
            '\\' => out.push_str("&#92;"),
            other => out.push(other),
        }
    }
    out
}

fn code(text: &str) -> String {
    format!("<code>{}</code>", md(text))
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

fn table<T>(rows: &[T], limit: usize, line: impl Fn(&T) -> String) -> String {
    rows.iter()
        .take(limit)
        .map(line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `[A-Za-z0-9@][A-Za-z0-9._@/+-]{0,79}`: a package name safe to put in a
/// commit-style title verbatim.
fn is_title_package(text: &str) -> bool {
    title_token(
        text,
        |c| c.is_ascii_alphanumeric() || c == '@',
        |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '/' | '+' | '-'),
    )
}

/// `[A-Za-z0-9][A-Za-z0-9._:+-]{0,79}`: a version safe to put in a title.
fn is_title_version(text: &str) -> bool {
    title_token(
        text,
        |c| c.is_ascii_alphanumeric(),
        |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '+' | '-'),
    )
}

fn title_token(text: &str, first: impl Fn(char) -> bool, rest: impl Fn(char) -> bool) -> bool {
    let mut chars = text.chars();
    let Some(head) = chars.next() else {
        return false;
    };
    first(head) && text.chars().count() <= 80 && chars.all(rest)
}

/// Keeps the first row of each key, leaving rows sorted by that key.
fn unique_by<T, K: Ord>(rows: &mut Vec<T>, key: impl Fn(&T) -> K) {
    rows.sort_by_key(|row| key(row));
    rows.dedup_by(|later, earlier| key(later) == key(earlier));
}

/// Total order over JSON values: null, false, true, numbers, strings, arrays,
/// objects; arrays element by element, objects by their sorted keys and then
/// their values.
fn jq_cmp(a: &Value, b: &Value) -> Ordering {
    fn rank(value: &Value) -> u8 {
        match value {
            Value::Null => 0,
            Value::Bool(false) => 1,
            Value::Bool(true) => 2,
            Value::Number(_) => 3,
            Value::String(_) => 4,
            Value::Array(_) => 5,
            Value::Object(_) => 6,
        }
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap_or(0.0), y.as_f64().unwrap_or(0.0));
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Array(x), Value::Array(y)) => x
            .iter()
            .zip(y)
            .map(|(x, y)| jq_cmp(x, y))
            .find(|order| order.is_ne())
            .unwrap_or_else(|| x.len().cmp(&y.len())),
        (Value::Object(x), Value::Object(y)) => {
            let mut x_keys: Vec<&String> = x.keys().collect();
            let mut y_keys: Vec<&String> = y.keys().collect();
            x_keys.sort();
            y_keys.sort();
            x_keys.cmp(&y_keys).then_with(|| {
                x_keys
                    .iter()
                    .map(|key| jq_cmp(&x[*key], &y[*key]))
                    .find(|order| order.is_ne())
                    .unwrap_or(Ordering::Equal)
            })
        }
        _ => rank(a).cmp(&rank(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_neutralizes_controls_and_collapses_whitespace() {
        assert_eq!(clean(&json!("  a\tb\u{202e}c\n\n d  "), 160), "a b c d");
        assert_eq!(clean(&json!("a\u{00a0}\u{3000}b"), 160), "a b");
        assert_eq!(clean(&json!(null), 160), "");
        assert_eq!(clean(&json!(3), 160), "3");
        assert_eq!(clean(&json!(true), 160), "true");
    }

    #[test]
    fn clean_keeps_zero_width_characters_that_are_not_whitespace() {
        assert_eq!(
            clean(&json!("a\u{200b}b\u{feff}c"), 160),
            "a\u{200b}b\u{feff}c"
        );
    }

    #[test]
    fn clean_truncates_by_character_not_byte() {
        assert_eq!(clean(&json!("héllo😀wörld"), 6), "héllo😀");
    }

    #[test]
    fn md_escapes_markup_and_table_syntax() {
        assert_eq!(
            md("<a href='x'>&\"`*_[]|\\"),
            "&lt;a href=&apos;x&apos;&gt;&amp;&quot;&#96;&#42;&#95;&#91;&#93;&#124;&#92;"
        );
    }

    #[test]
    fn jq_order_ranks_types_then_values() {
        let mut values = vec![
            json!({"a": 1}),
            json!([1]),
            json!("b"),
            json!("B"),
            json!(10),
            json!(2),
            json!(true),
            json!(false),
            json!(null),
        ];
        values.sort_by(jq_cmp);
        assert_eq!(
            values,
            vec![
                json!(null),
                json!(false),
                json!(true),
                json!(2),
                json!(10),
                json!("B"),
                json!("b"),
                json!([1]),
                json!({"a": 1}),
            ]
        );
    }

    #[test]
    fn unique_by_keeps_the_first_row_of_each_key() {
        let mut rows = vec![("b", 1), ("a", 2), ("b", 3), ("a", 4)];
        unique_by(&mut rows, |row| row.0);
        assert_eq!(rows, vec![("a", 2), ("b", 1)]);
    }

    #[test]
    fn title_tokens_follow_the_commit_title_alphabet() {
        assert!(is_title_package("@scope/name"));
        assert!(!is_title_package("-leading"));
        assert!(!is_title_package(&"a".repeat(81)));
        assert!(is_title_package(&"a".repeat(80)));
        assert!(is_title_version("1.2.3+build"));
        assert!(!is_title_version("v1 2"));
    }

    #[test]
    fn a_file_entry_that_is_not_an_object_is_a_shape_error() {
        let context = Context {
            min_age: "",
            max_bump: "",
            lock: false,
            auto_merge: false,
            validation_configured: false,
            changed: true,
            changed_paths: &[],
        };
        let report = json!({"files": ["dependency.txt"], "summary": {}});
        assert!(Presentation::from_report(&report, &context).is_err());
        let report = json!({"files": [{"held_back": "x"}], "summary": {}});
        assert!(Presentation::from_report(&report, &context).is_err());
    }

    #[test]
    fn missing_sections_read_as_empty() {
        let context = Context {
            min_age: "7d",
            max_bump: "",
            lock: false,
            auto_merge: false,
            validation_configured: false,
            changed: false,
            changed_paths: &[],
        };
        let report = json!({"files": "none", "summary": {"warnings": 2}});
        let presentation = Presentation::from_report(&report, &context).unwrap();
        assert_eq!(presentation.state, "clean");
        assert_eq!(presentation.counts.warnings, json!(2));
        assert_eq!(presentation.counts.not_examined, json!(0));
        assert_eq!(presentation.policy.min_age, "7d");
        assert_eq!(presentation.policy.max_bump, "repository configuration");
        assert_eq!(
            presentation.title,
            "chore(deps): refresh dependency metadata"
        );
    }

    #[test]
    fn summary_line_defaults_optional_counts_to_zero() {
        let report = json!({"summary": {"updates_total": 2, "files_with_changes": 1, "errors": 0}});
        assert_eq!(
            summary_line(&report).unwrap(),
            "upd: 2 update(s), 0 normalized specifier(s), 1 changed file(s), 0 held back, 0 capped, 0 error(s)"
        );
    }

    #[test]
    fn error_gate_treats_only_a_zero_count_as_clean() {
        assert!(report_is_error_free(&json!({"summary": {"errors": 0}})).unwrap());
        assert!(report_is_error_free(&json!({"summary": {}})).unwrap());
        assert!(!report_is_error_free(&json!({"summary": {"errors": 1}})).unwrap());
        assert!(!report_is_error_free(&json!({"summary": {"errors": "0"}})).unwrap());
    }
}
