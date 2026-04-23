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

/// A structured error entry in the JSON output.
///
/// Replaces the former `errors: Vec<String>` with typed objects so that
/// consumers can programmatically distinguish network failures from parse
/// errors without string-matching.
#[derive(Debug, Serialize, Clone)]
pub struct ErrorEntry {
    /// Path of the file being processed when the error occurred, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Package name involved in the error, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Machine-readable error category.
    pub kind: &'static str,
    /// Human-readable error description.
    pub message: String,
}

impl ErrorEntry {
    /// Construct an error with a known file path.
    ///
    /// The `kind` argument is one of the documented error categories:
    /// `"network"`, `"parse"`, `"registry"`, `"io"`, or `"other"`.
    pub fn with_file(
        file: impl Into<String>,
        kind: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            file: Some(file.into()),
            package: None,
            kind,
            message: message.into(),
        }
    }
}

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held_back: Vec<HeldBackEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_by_cooldown: Vec<SkippedByCooldownEntry>,
    pub errors: Vec<ErrorEntry>,
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

/// A package update held back by cooldown — the chosen version is older than
/// the absolute latest, which was too new.
#[derive(Debug, Serialize)]
pub struct HeldBackEntry {
    pub package: String,
    pub current: String,
    /// The version that was actually written (old enough to pass the cooldown).
    pub chosen: String,
    /// The absolute latest that was skipped because it is too new.
    pub skipped_latest: String,
    /// RFC 3339 timestamp of when `skipped_latest` was published.
    pub skipped_published_at: String,
    /// Cooldown duration that caused the hold-back, in seconds.
    pub cooldown_seconds: i64,
}

/// A package skipped by cooldown entirely — every newer version is too new,
/// so the current version is kept.
#[derive(Debug, Serialize)]
pub struct SkippedByCooldownEntry {
    pub package: String,
    pub current: String,
    /// The latest version that was skipped because it is too new.
    pub skipped_latest: String,
    /// RFC 3339 timestamp of when `skipped_latest` was published.
    pub skipped_published_at: String,
    /// Cooldown duration that caused the skip, in seconds.
    pub cooldown_seconds: i64,
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
    #[serde(default, skip_serializing_if = "is_zero")]
    pub held_back: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped_by_cooldown: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Serialize)]
pub struct UpdateReport {
    pub command: &'static str,
    pub mode: &'static str,
    pub files: Vec<UpdateFileReport>,
    pub summary: UpdateSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cooldown_notes: Vec<String>,
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
///
/// `cooldown_seconds` is the effective cooldown for this file's ecosystem (0
/// when cooldown is disabled or unknown).
pub fn build_update_file_report(
    path: &Path,
    file_type: FileType,
    result: &UpdateResult,
    cooldown_seconds: i64,
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

    let held_back = result
        .held_back
        .iter()
        .map(|(name, old, chosen, skipped, pub_at)| HeldBackEntry {
            package: name.clone(),
            current: old.clone(),
            chosen: chosen.clone(),
            skipped_latest: skipped.clone(),
            skipped_published_at: pub_at.to_rfc3339(),
            cooldown_seconds,
        })
        .collect();

    let skipped_by_cooldown = result
        .skipped_by_cooldown
        .iter()
        .map(|(name, current, skipped, pub_at)| SkippedByCooldownEntry {
            package: name.clone(),
            current: current.clone(),
            skipped_latest: skipped.clone(),
            skipped_published_at: pub_at.to_rfc3339(),
            cooldown_seconds,
        })
        .collect();

    let path_str = path.display().to_string();
    let errors = result
        .errors
        .iter()
        .map(|msg| ErrorEntry::with_file(path_str.clone(), "other", msg.clone()))
        .collect();

    UpdateFileReport {
        path: path_str,
        file_type: file_type.as_str(),
        lang: file_type.lang().as_str(),
        updates,
        pinned,
        ignored,
        held_back,
        skipped_by_cooldown,
        errors,
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

// ──────────────────────────────────────────────────────────────────────────────
// SARIF 2.1.0 types
// ──────────────────────────────────────────────────────────────────────────────

/// Top-level SARIF 2.1.0 log document.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifLog {
    #[serde(rename = "$schema")]
    pub schema: &'static str,
    pub version: &'static str,
    pub runs: Vec<SarifRun>,
}

/// A single tool run within a SARIF log.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifRun {
    pub tool: SarifTool,
    pub results: Vec<SarifResult>,
}

/// Tool metadata for a SARIF run.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifTool {
    pub driver: SarifDriver,
}

/// SARIF tool driver (the analysis tool itself).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifDriver {
    pub name: &'static str,
    pub version: String,
    pub information_uri: &'static str,
    pub rules: Vec<SarifRule>,
}

/// A SARIF rule entry (one per unique vulnerability ID).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifRule {
    pub id: String,
    pub name: String,
    pub short_description: SarifMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help_uri: Option<String>,
    pub default_configuration: SarifRuleConfiguration,
}

/// SARIF rule default configuration.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifRuleConfiguration {
    pub level: &'static str,
}

/// A SARIF result (one finding).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifResult {
    pub rule_id: String,
    pub level: &'static str,
    pub message: SarifMessage,
    pub locations: Vec<SarifLocation>,
    pub properties: SarifResultProperties,
}

/// A plain text message in SARIF.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifMessage {
    pub text: String,
}

/// A location within a SARIF result.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifLocation {
    pub physical_location: SarifPhysicalLocation,
}

/// Physical file location within SARIF.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifPhysicalLocation {
    pub artifact_location: SarifArtifactLocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<SarifRegion>,
}

/// Artifact (file) location in SARIF.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifArtifactLocation {
    pub uri: String,
}

/// Line/column region within a SARIF artifact.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifRegion {
    pub start_line: usize,
}

/// Additional metadata attached to a SARIF result.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarifResultProperties {
    pub package: String,
    pub version: String,
    pub ecosystem: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Map an OSV severity label to a SARIF level string.
///
/// SARIF levels are: `"error"`, `"warning"`, `"note"`, `"none"`.
fn osv_severity_to_sarif_level(severity: Option<&str>) -> &'static str {
    match severity {
        Some(s) if s.eq_ignore_ascii_case("critical") || s.eq_ignore_ascii_case("high") => "error",
        Some(s) if s.eq_ignore_ascii_case("medium") => "warning",
        Some(s) if s.eq_ignore_ascii_case("low") => "note",
        _ => "warning",
    }
}

/// Maps `(package_name, version, ecosystem)` to the list of `(file_path, line)`
/// where that exact pin appears. Used as input to [`build_sarif_audit_report`].
pub type SarifOccurrenceMap =
    std::collections::HashMap<(String, String, String), Vec<(String, Option<usize>)>>;

/// Build a [`SarifLog`] from an [`AuditResult`] and package location data.
///
/// The `occurrences` parameter maps each `(package_name, version, ecosystem)`
/// triple to the list of `(file_path, line_number)` pairs where that exact pin
/// appears. The caller is responsible for building this mapping from the
/// already-scanned package data.
pub fn build_sarif_audit_report(audit: &AuditResult, occurrences: &SarifOccurrenceMap) -> SarifLog {
    // Collect deduplicated rules (one per unique vulnerability ID).
    let mut rules: Vec<SarifRule> = Vec::new();
    let mut seen_rule_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for pkg_result in &audit.vulnerable {
        for vuln in &pkg_result.vulnerabilities {
            if seen_rule_ids.insert(vuln.id.clone()) {
                let level = osv_severity_to_sarif_level(vuln.severity.as_deref());
                let description = vuln.summary.clone().unwrap_or_else(|| {
                    format!(
                        "Vulnerability in {} {}",
                        pkg_result.package.name, pkg_result.package.version
                    )
                });
                rules.push(SarifRule {
                    id: vuln.id.clone(),
                    name: vuln.id.clone(),
                    short_description: SarifMessage { text: description },
                    help_uri: vuln.url.clone(),
                    default_configuration: SarifRuleConfiguration { level },
                });
            }
        }
    }

    // Build one result per (vulnerability, file occurrence).
    let mut results: Vec<SarifResult> = Vec::new();

    for pkg_result in &audit.vulnerable {
        let eco = pkg_result.package.ecosystem.as_str().to_string();
        let lookup_key = (
            pkg_result.package.name.clone(),
            pkg_result.package.version.clone(),
            eco.clone(),
        );
        let file_locations: &[(String, Option<usize>)] = occurrences
            .get(&lookup_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        for vuln in &pkg_result.vulnerabilities {
            let level = osv_severity_to_sarif_level(vuln.severity.as_deref());
            let message_text = vuln.summary.clone().unwrap_or_else(|| {
                format!(
                    "Vulnerability in {} {}",
                    pkg_result.package.name, pkg_result.package.version
                )
            });

            // Emit one location per file where the package is pinned, or a
            // placeholder location when no file occurrence data is available.
            let sarif_locations: Vec<SarifLocation> = if file_locations.is_empty() {
                vec![SarifLocation {
                    physical_location: SarifPhysicalLocation {
                        artifact_location: SarifArtifactLocation { uri: String::new() },
                        region: None,
                    },
                }]
            } else {
                file_locations
                    .iter()
                    .map(|(path, line)| SarifLocation {
                        physical_location: SarifPhysicalLocation {
                            artifact_location: SarifArtifactLocation { uri: path.clone() },
                            region: line.map(|l| SarifRegion { start_line: l }),
                        },
                    })
                    .collect()
            };

            results.push(SarifResult {
                rule_id: vuln.id.clone(),
                level,
                message: SarifMessage { text: message_text },
                locations: sarif_locations,
                properties: SarifResultProperties {
                    package: pkg_result.package.name.clone(),
                    version: pkg_result.package.version.clone(),
                    ecosystem: eco.clone(),
                    fixed_version: vuln.fixed_version.clone(),
                    url: vuln.url.clone(),
                },
            });
        }
    }

    SarifLog {
        schema: "https://json.schemastore.org/sarif-2.1.0.json",
        version: "2.1.0",
        runs: vec![SarifRun {
            tool: SarifTool {
                driver: SarifDriver {
                    name: "upd",
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    information_uri: "https://github.com/rvben/upd",
                    rules,
                },
            },
            results,
        }],
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

    // Sort the flattened list by severity descending so JSON output is
    // deterministic and consistent with the text output order.
    vulnerabilities.sort_by_key(|v| crate::audit::severity_sort_key(v.severity.as_deref()));

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
            ..Default::default()
        };
        let report = build_update_file_report(
            Path::new("package.json"),
            FileType::PackageJson,
            &result,
            0,
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
        assert_eq!(json["errors"][0]["message"], "lookup failed: foo");
        assert_eq!(json["errors"][0]["kind"], "other");
        assert_eq!(json["errors"][0]["file"], "package.json");
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
            0,
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
                        severity: Some("High".into()),
                        url: Some("https://example/abc".into()),
                        fixed_version: Some("4.17.21".into()),
                    },
                    Vulnerability {
                        id: "CVE-2020-1234".into(),
                        summary: None,
                        severity: Some("Unknown".into()),
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
        assert_eq!(json["vulnerabilities"][0]["severity"], "High");
        assert_eq!(
            json["vulnerabilities"][1]["severity"], "Unknown",
            "vulnerabilities with no severity data must serialize as Unknown"
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

    // ──────────────────────────────────────────────────────────────────────────
    // SARIF output unit tests
    // ──────────────────────────────────────────────────────────────────────────

    fn make_audit_with_vuln() -> AuditResult {
        AuditResult {
            vulnerable: vec![PackageAuditResult {
                package: Package {
                    name: "requests".into(),
                    version: "2.27.0".into(),
                    ecosystem: Ecosystem::PyPI,
                },
                vulnerabilities: vec![
                    Vulnerability {
                        id: "GHSA-abcd-1234-efgh".into(),
                        summary: Some("Remote code execution in requests".into()),
                        severity: Some("High".into()),
                        url: Some("https://osv.dev/vulnerability/GHSA-abcd-1234-efgh".into()),
                        fixed_version: Some("2.28.0".into()),
                    },
                    Vulnerability {
                        id: "CVE-2023-99999".into(),
                        summary: None,
                        severity: Some("Medium".into()),
                        url: None,
                        fixed_version: None,
                    },
                ],
            }],
            safe_count: 2,
            errors: Vec::new(),
        }
    }

    #[test]
    fn sarif_report_includes_schema_and_version() {
        let audit = make_audit_with_vuln();
        let occurrences = std::collections::HashMap::new();
        let log = build_sarif_audit_report(&audit, &occurrences);

        assert_eq!(log.schema, "https://json.schemastore.org/sarif-2.1.0.json");
        assert_eq!(log.version, "2.1.0");
        assert_eq!(log.runs.len(), 1);

        let json = serde_json::to_value(&log).unwrap();
        assert_eq!(
            json["$schema"],
            "https://json.schemastore.org/sarif-2.1.0.json"
        );
        assert_eq!(json["version"], "2.1.0");
        assert_eq!(json["runs"][0]["tool"]["driver"]["name"], "upd");
        assert_eq!(
            json["runs"][0]["tool"]["driver"]["informationUri"],
            "https://github.com/rvben/upd"
        );
    }

    #[test]
    fn sarif_rules_deduplicated_by_id() {
        // Two vulnerabilities with the same ID across different packages should
        // produce only one rule entry.
        let audit = AuditResult {
            vulnerable: vec![
                PackageAuditResult {
                    package: Package {
                        name: "pkg-a".into(),
                        version: "1.0.0".into(),
                        ecosystem: Ecosystem::Npm,
                    },
                    vulnerabilities: vec![Vulnerability {
                        id: "GHSA-dup-test".into(),
                        summary: Some("Shared vuln".into()),
                        severity: Some("High".into()),
                        url: None,
                        fixed_version: None,
                    }],
                },
                PackageAuditResult {
                    package: Package {
                        name: "pkg-b".into(),
                        version: "2.0.0".into(),
                        ecosystem: Ecosystem::Npm,
                    },
                    vulnerabilities: vec![Vulnerability {
                        id: "GHSA-dup-test".into(),
                        summary: Some("Shared vuln".into()),
                        severity: Some("High".into()),
                        url: None,
                        fixed_version: None,
                    }],
                },
            ],
            safe_count: 0,
            errors: Vec::new(),
        };

        let occurrences = std::collections::HashMap::new();
        let log = build_sarif_audit_report(&audit, &occurrences);
        let rules = &log.runs[0].tool.driver.rules;

        assert_eq!(
            rules.len(),
            1,
            "duplicate rule ID must appear only once; got {} rules",
            rules.len()
        );
        assert_eq!(rules[0].id, "GHSA-dup-test");
    }

    #[test]
    fn sarif_severity_mapping_critical_high_medium_low() {
        for (sev, expected_level) in [
            ("Critical", "error"),
            ("High", "error"),
            ("Medium", "warning"),
            ("Low", "note"),
            ("Unknown", "warning"),
        ] {
            let level = osv_severity_to_sarif_level(Some(sev));
            assert_eq!(
                level, expected_level,
                "severity '{}' should map to '{}'",
                sev, expected_level
            );
        }
        // None maps to warning
        assert_eq!(osv_severity_to_sarif_level(None), "warning");
    }

    #[test]
    fn sarif_result_locations_from_file_occurrences() {
        let audit = make_audit_with_vuln();

        let mut occurrences = std::collections::HashMap::new();
        occurrences.insert(
            (
                "requests".to_string(),
                "2.27.0".to_string(),
                "PyPI".to_string(),
            ),
            vec![
                ("requirements.txt".to_string(), Some(5usize)),
                ("setup.cfg".to_string(), None),
            ],
        );

        let log = build_sarif_audit_report(&audit, &occurrences);
        let results = &log.runs[0].results;

        // Two vulns × two locations each = 4 results total.
        // Each result for the first vuln should have two locations.
        let first_result = &results[0];
        assert_eq!(first_result.locations.len(), 2);

        let loc0 = &first_result.locations[0].physical_location;
        assert_eq!(loc0.artifact_location.uri, "requirements.txt");
        assert_eq!(loc0.region.as_ref().unwrap().start_line, 5);

        let loc1 = &first_result.locations[1].physical_location;
        assert_eq!(loc1.artifact_location.uri, "setup.cfg");
        assert!(
            loc1.region.is_none(),
            "line-less occurrence should have no region"
        );
    }

    #[test]
    fn sarif_result_properties_include_fixed_version() {
        let audit = make_audit_with_vuln();
        let occurrences = std::collections::HashMap::new();
        let log = build_sarif_audit_report(&audit, &occurrences);
        let json = serde_json::to_value(&log).unwrap();

        let first_result = &json["runs"][0]["results"][0];
        assert_eq!(first_result["properties"]["package"], "requests");
        assert_eq!(first_result["properties"]["version"], "2.27.0");
        assert_eq!(first_result["properties"]["ecosystem"], "PyPI");
        assert_eq!(first_result["properties"]["fixedVersion"], "2.28.0");
        assert_eq!(
            first_result["properties"]["url"],
            "https://osv.dev/vulnerability/GHSA-abcd-1234-efgh"
        );

        // Second vuln has no fixed_version — field should be absent.
        let second_result = &json["runs"][0]["results"][1];
        assert!(
            second_result["properties"].get("fixedVersion").is_none()
                || second_result["properties"]["fixedVersion"].is_null(),
            "fixedVersion must be absent when not set"
        );
    }
}
