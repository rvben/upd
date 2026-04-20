//! JSON output format for `--format json`.
//!
//! Defines the stable schema emitted by `upd update`, `upd align`, and
//! `upd audit` when `--format json` is passed. The schema is part of the
//! public CLI contract: additive changes are allowed, field renames are
//! breaking.

use crate::align::{PackageAlignment, PackageOccurrence};
use crate::audit::{AuditResult, Vulnerability};
use crate::updater::{FileType, UpdateResult};
use serde::Serialize;
use std::path::Path;

/// Wire-level representation of a dependency file and what `upd update`
/// would or did do to it.
#[derive(Debug, Serialize)]
pub struct UpdateFileReport {
    pub path: String,
    pub file_type: &'static str,
    pub lang: &'static str,
    pub updates: Vec<UpdateEntry>,
    pub pinned: Vec<PinnedEntry>,
    pub ignored: Vec<IgnoredEntry>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct UpdateEntry {
    pub package: String,
    pub current: String,
    pub latest: String,
    pub bump: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct PinnedEntry {
    pub package: String,
    pub current: String,
    pub pinned_to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct IgnoredEntry {
    pub package: String,
    pub current: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct UpdateSummary {
    pub files_scanned: usize,
    pub files_with_changes: usize,
    pub updates_total: usize,
    pub updates_major: usize,
    pub updates_minor: usize,
    pub updates_patch: usize,
    pub pinned: usize,
    pub ignored: usize,
    pub errors: usize,
    pub warnings: usize,
}

#[derive(Debug, Serialize)]
pub struct UpdateReport {
    pub command: &'static str,
    pub mode: &'static str,
    pub files: Vec<UpdateFileReport>,
    pub summary: UpdateSummary,
}

#[derive(Debug, Serialize)]
pub struct AlignOccurrence {
    pub path: String,
    pub file_type: &'static str,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub has_upper_bound: bool,
    pub is_misaligned: bool,
}

#[derive(Debug, Serialize)]
pub struct AlignPackage {
    pub package: String,
    pub lang: &'static str,
    pub highest_version: String,
    pub is_misaligned: bool,
    pub occurrences: Vec<AlignOccurrence>,
}

#[derive(Debug, Serialize)]
pub struct AlignSummary {
    pub files_scanned: usize,
    pub packages: usize,
    pub misaligned_packages: usize,
    pub misaligned_occurrences: usize,
}

#[derive(Debug, Serialize)]
pub struct AlignReport {
    pub command: &'static str,
    pub packages: Vec<AlignPackage>,
    pub summary: AlignSummary,
}

#[derive(Debug, Serialize)]
pub struct AuditVulnerability {
    pub package: String,
    pub version: String,
    pub ecosystem: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuditSummary {
    pub packages_checked: usize,
    pub vulnerable_packages: usize,
    pub vulnerabilities: usize,
    pub errors: usize,
}

#[derive(Debug, Serialize)]
pub struct AuditReport {
    pub command: &'static str,
    /// `"complete"` when every scanned package was checked successfully;
    /// `"incomplete"` when the audit could not run (e.g. network failure).
    pub status: &'static str,
    pub vulnerabilities: Vec<AuditVulnerability>,
    pub errors: Vec<String>,
    pub summary: AuditSummary,
}

/// Build an [`UpdateFileReport`] from internal per-file data.
pub fn build_update_file_report(
    path: &Path,
    file_type: FileType,
    result: &UpdateResult,
    classify: impl Fn(&str, &str) -> &'static str,
) -> UpdateFileReport {
    let updates = result
        .updated
        .iter()
        .map(|(name, old, new, line)| UpdateEntry {
            package: name.clone(),
            current: old.clone(),
            latest: new.clone(),
            bump: classify(old, new),
            line: *line,
        })
        .collect();

    let pinned = result
        .pinned
        .iter()
        .map(|(name, old, new, line)| PinnedEntry {
            package: name.clone(),
            current: old.clone(),
            pinned_to: new.clone(),
            line: *line,
        })
        .collect();

    let ignored = result
        .ignored
        .iter()
        .map(|(name, current, line)| IgnoredEntry {
            package: name.clone(),
            current: current.clone(),
            line: *line,
        })
        .collect();

    UpdateFileReport {
        path: path.display().to_string(),
        file_type: file_type.as_str(),
        lang: file_type.lang().as_str(),
        updates,
        pinned,
        ignored,
        errors: result.errors.clone(),
        warnings: result.warnings.clone(),
    }
}

/// Build an [`AlignPackage`] from an internal [`PackageAlignment`].
pub fn build_align_package(alignment: &PackageAlignment) -> AlignPackage {
    let occurrences = alignment
        .occurrences
        .iter()
        .map(|o| occurrence_to_json(o, &alignment.highest_version))
        .collect();

    AlignPackage {
        package: alignment.package_name.clone(),
        lang: alignment.lang.as_str(),
        highest_version: alignment.highest_version.clone(),
        is_misaligned: alignment.has_misalignment(),
        occurrences,
    }
}

fn occurrence_to_json(o: &PackageOccurrence, highest: &str) -> AlignOccurrence {
    let misaligned = !o.has_upper_bound && o.version != highest;
    AlignOccurrence {
        path: o.file_path.display().to_string(),
        file_type: o.file_type.as_str(),
        version: o.version.clone(),
        line: o.line_number,
        has_upper_bound: o.has_upper_bound,
        is_misaligned: misaligned,
    }
}

/// Build an [`AuditReport`] from an internal [`AuditResult`].
pub fn build_audit_report(
    audit: &AuditResult,
    ecosystems_audited: usize,
    status: &'static str,
) -> AuditReport {
    let mut vulnerabilities = Vec::new();
    for pkg in &audit.vulnerable {
        for v in &pkg.vulnerabilities {
            vulnerabilities.push(build_vulnerability(
                &pkg.package.name,
                &pkg.package.version,
                pkg.package.ecosystem.as_str(),
                v,
            ));
        }
    }

    let packages_checked = audit.vulnerable.len() + audit.safe_count;
    let vulnerability_count = vulnerabilities.len();

    AuditReport {
        command: "audit",
        status,
        vulnerabilities,
        errors: audit.errors.clone(),
        summary: AuditSummary {
            packages_checked,
            vulnerable_packages: audit.vulnerable.len(),
            vulnerabilities: vulnerability_count,
            errors: audit.errors.len(),
        },
    }
    .with_ecosystem_noop(ecosystems_audited)
}

impl AuditReport {
    fn with_ecosystem_noop(self, _ecosystems: usize) -> Self {
        // Reserved for future schema expansion (e.g. skipped ecosystems).
        self
    }
}

fn build_vulnerability(
    package: &str,
    version: &str,
    ecosystem: &str,
    v: &Vulnerability,
) -> AuditVulnerability {
    AuditVulnerability {
        package: package.to_string(),
        version: version.to_string(),
        ecosystem: ecosystem.to_string(),
        id: v.id.clone(),
        summary: v.summary.clone(),
        severity: v.severity.clone(),
        fixed_version: v.fixed_version.clone(),
        url: v.url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::{PackageAlignment, PackageOccurrence};
    use crate::audit::{Ecosystem, Package, PackageAuditResult};
    use crate::updater::{FileType, Lang, UpdateResult};
    use std::path::PathBuf;

    fn stub_classify(old: &str, new: &str) -> &'static str {
        if old == new {
            "patch"
        } else if new.starts_with("2") && old.starts_with("1") {
            "major"
        } else if new.ends_with(".1.0") {
            "minor"
        } else {
            "patch"
        }
    }

    #[test]
    fn update_file_report_serializes_all_sections() {
        let result = UpdateResult {
            updated: vec![("react".into(), "18.2.0".into(), "19.0.0".into(), Some(7))],
            pinned: vec![("lodash".into(), "4.17.0".into(), "4.17.21".into(), Some(12))],
            ignored: vec![("chalk".into(), "5.0.0".into(), Some(20))],
            errors: vec!["lookup failed: foo".into()],
            warnings: vec![
                "skipping bar: current version \"%version%\" is not a valid PEP 440 version".into(),
            ],
            unchanged: 4,
        };
        let report = build_update_file_report(
            Path::new("package.json"),
            FileType::PackageJson,
            &result,
            |old, new| {
                if old == "18.2.0" && new == "19.0.0" {
                    "major"
                } else {
                    "patch"
                }
            },
        );

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["path"], "package.json");
        assert_eq!(json["file_type"], "package_json");
        assert_eq!(json["lang"], "node");
        assert_eq!(json["updates"][0]["package"], "react");
        assert_eq!(json["updates"][0]["current"], "18.2.0");
        assert_eq!(json["updates"][0]["latest"], "19.0.0");
        assert_eq!(json["updates"][0]["bump"], "major");
        assert_eq!(json["updates"][0]["line"], 7);
        assert_eq!(json["pinned"][0]["pinned_to"], "4.17.21");
        assert_eq!(json["ignored"][0]["package"], "chalk");
        assert_eq!(json["errors"][0], "lookup failed: foo");
        assert_eq!(
            json["warnings"][0],
            "skipping bar: current version \"%version%\" is not a valid PEP 440 version"
        );
    }

    #[test]
    fn update_file_report_omits_absent_line_numbers() {
        let result = UpdateResult {
            updated: vec![("flask".into(), "2.0".into(), "2.1.0".into(), None)],
            ..Default::default()
        };
        let report = build_update_file_report(
            Path::new("requirements.txt"),
            FileType::Requirements,
            &result,
            stub_classify,
        );
        let json = serde_json::to_value(&report).unwrap();
        assert!(
            json["updates"][0].get("line").is_none(),
            "line should be omitted when None"
        );
    }

    #[test]
    fn align_package_classifies_misaligned_occurrences() {
        let alignment = PackageAlignment {
            package_name: "react".into(),
            highest_version: "19.0.0".into(),
            lang: Lang::Node,
            occurrences: vec![
                PackageOccurrence {
                    file_path: PathBuf::from("app/package.json"),
                    file_type: FileType::PackageJson,
                    version: "18.2.0".into(),
                    line_number: Some(4),
                    has_upper_bound: false,
                    original_name: "react".into(),
                    is_bumpable: true,
                },
                PackageOccurrence {
                    file_path: PathBuf::from("api/package.json"),
                    file_type: FileType::PackageJson,
                    version: "19.0.0".into(),
                    line_number: Some(5),
                    has_upper_bound: false,
                    original_name: "react".into(),
                    is_bumpable: true,
                },
                PackageOccurrence {
                    file_path: PathBuf::from("legacy/package.json"),
                    file_type: FileType::PackageJson,
                    version: "17.0.0".into(),
                    line_number: Some(6),
                    has_upper_bound: true,
                    original_name: "react".into(),
                    is_bumpable: true,
                },
            ],
        };
        let pkg = build_align_package(&alignment);
        let json = serde_json::to_value(&pkg).unwrap();
        assert_eq!(json["package"], "react");
        assert_eq!(json["lang"], "node");
        assert_eq!(json["highest_version"], "19.0.0");
        assert_eq!(json["is_misaligned"], true);
        assert_eq!(json["occurrences"][0]["is_misaligned"], true);
        assert_eq!(json["occurrences"][1]["is_misaligned"], false);
        assert_eq!(
            json["occurrences"][2]["is_misaligned"], false,
            "upper-bound constrained occurrence is not misaligned"
        );
    }

    #[test]
    fn audit_report_flattens_vulnerabilities() {
        let audit = AuditResult {
            vulnerable: vec![PackageAuditResult {
                package: Package {
                    name: "lodash".into(),
                    version: "4.17.20".into(),
                    ecosystem: Ecosystem::Npm,
                },
                vulnerabilities: vec![
                    Vulnerability {
                        id: "GHSA-abc".into(),
                        summary: Some("Prototype pollution".into()),
                        severity: Some("HIGH".into()),
                        url: Some("https://example/abc".into()),
                        fixed_version: Some("4.17.21".into()),
                    },
                    Vulnerability {
                        id: "CVE-2020-1234".into(),
                        summary: None,
                        severity: None,
                        url: None,
                        fixed_version: None,
                    },
                ],
            }],
            safe_count: 5,
            errors: Vec::new(),
        };
        let report = build_audit_report(&audit, 2, "complete");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["command"], "audit");
        assert_eq!(json["status"], "complete");
        assert_eq!(json["vulnerabilities"].as_array().unwrap().len(), 2);
        assert_eq!(json["vulnerabilities"][0]["ecosystem"], "npm");
        assert_eq!(json["vulnerabilities"][0]["id"], "GHSA-abc");
        assert_eq!(json["vulnerabilities"][0]["severity"], "HIGH");
        assert!(
            json["vulnerabilities"][1].get("severity").is_none(),
            "absent severity must be omitted"
        );
        assert_eq!(json["summary"]["packages_checked"], 6);
        assert_eq!(json["summary"]["vulnerable_packages"], 1);
        assert_eq!(json["summary"]["vulnerabilities"], 2);
    }

    #[test]
    fn audit_report_incomplete_when_errors_present() {
        let audit = AuditResult {
            vulnerable: Vec::new(),
            safe_count: 0,
            errors: vec!["network error".into()],
        };
        let report = build_audit_report(&audit, 0, "incomplete");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["status"], "incomplete");
        assert_eq!(json["errors"][0], "network error");
        assert_eq!(json["summary"]["errors"], 1);
    }
}
