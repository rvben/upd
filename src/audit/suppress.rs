//! Accepting known advisories: `[audit] ignore-advisories`.
//!
//! A suppression names one advisory by id or alias. While it applies, the
//! matching findings move out of [`AuditResult::vulnerable`] into
//! [`AuditResult::suppressed`], so everything that reads `vulnerable` (the
//! exit code, `--fix-audit`, remediation counts) treats them as resolved,
//! while the reports keep listing them with the reason they were accepted.

use super::{AuditResult, Package, Vulnerability};
use crate::config::AdvisorySuppression;
use chrono::NaiveDate;

/// A finding the configuration accepts.
#[derive(Debug, Clone)]
pub struct SuppressedFinding {
    pub package: Package,
    pub vulnerability: Vulnerability,
    /// The reason written next to the suppression.
    pub reason: String,
    /// Last day the suppression applies, if it has one.
    pub expires: Option<NaiveDate>,
}

impl SuppressedFinding {
    /// The `id (package version)` form the text report and warnings use.
    pub fn label(&self) -> String {
        format!(
            "{} ({} {})",
            self.vulnerability.id, self.package.name, self.package.version
        )
    }
}

fn names(suppression: &AdvisorySuppression, vulnerability: &Vulnerability) -> bool {
    let id = suppression.id.trim();
    vulnerability.id.eq_ignore_ascii_case(id)
        || vulnerability
            .aliases
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(id))
}

/// Whether `suppression` still applies on `today`: through its last day.
fn in_force(suppression: &AdvisorySuppression, today: NaiveDate) -> bool {
    suppression.expires.is_none_or(|last_day| today <= last_day)
}

/// Move the findings `suppressions` accept out of `result.vulnerable`.
///
/// An expired suppression accepts nothing, and says so in
/// `suppression_warnings`, so its advisory is reported (and fails the audit)
/// again with the explanation next to it. With `report_unmatched`, a
/// suppression in force that matched no finding is reported too, so a stale
/// entry gets removed; callers pass `false` when the audit did not cover the
/// whole tree (narrowed by `--lang` or a package filter, or incomplete) and
/// the absence proves nothing.
pub fn apply_suppressions(
    result: &mut AuditResult,
    suppressions: &[AdvisorySuppression],
    today: NaiveDate,
    report_unmatched: bool,
) {
    // Findings whose details could not be fetched carry no aliases, so an
    // entry written as an alias cannot be matched against them.
    let aliases_unreliable = result.vulnerable.iter().any(|package| {
        package.vulnerabilities.iter().any(|vulnerability| {
            vulnerability.summary.is_none()
                && vulnerability.aliases.is_empty()
                && vulnerability.affected.is_empty()
        })
    });

    let mut matched = vec![false; suppressions.len()];
    let mut remaining = Vec::with_capacity(result.vulnerable.len());
    for mut package_result in std::mem::take(&mut result.vulnerable) {
        let mut kept = Vec::with_capacity(package_result.vulnerabilities.len());
        for vulnerability in std::mem::take(&mut package_result.vulnerabilities) {
            let naming: Vec<usize> = (0..suppressions.len())
                .filter(|&index| names(&suppressions[index], &vulnerability))
                .collect();
            let accepting = naming
                .iter()
                .copied()
                .find(|&index| in_force(&suppressions[index], today));
            for &index in &naming {
                matched[index] = true;
            }
            match accepting {
                Some(index) => result.suppressed.push(SuppressedFinding {
                    package: package_result.package.clone(),
                    vulnerability,
                    reason: suppressions[index].reason.trim().to_string(),
                    expires: suppressions[index].expires,
                }),
                None => kept.push(vulnerability),
            }
        }
        if kept.is_empty() {
            result.safe_count += 1;
        } else {
            package_result.vulnerabilities = kept;
            remaining.push(package_result);
        }
    }
    result.vulnerable = remaining;

    for (index, suppression) in suppressions.iter().enumerate() {
        if in_force(suppression, today) {
            if report_unmatched && !matched[index] && !aliases_unreliable {
                result.suppression_warnings.push(format!(
                    "the suppression of {} matches no advisory in the audited dependencies; \
                     remove it if the advisory no longer applies",
                    suppression.id
                ));
            }
            continue;
        }
        // Expired: it accepts nothing. The advisory it names is reported
        // again only if no other entry accepted it.
        let still_found = result.vulnerable.iter().any(|package| {
            package
                .vulnerabilities
                .iter()
                .any(|vulnerability| names(suppression, vulnerability))
        });
        let last_day = suppression.expires.expect("only a dated entry expires");
        result.suppression_warnings.push(format!(
            "the suppression of {} expired on {last_day} and no longer applies{}; \
             renew it with a new `expires` and a current reason, or remove it",
            suppression.id,
            if still_found {
                ", so the advisory is reported again"
            } else {
                ""
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{Ecosystem, PackageAuditResult};

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn entry(id: &str, expires: Option<NaiveDate>) -> AdvisorySuppression {
        AdvisorySuppression {
            id: id.to_string(),
            reason: " accepted ".to_string(),
            expires,
        }
    }

    fn result_with(id: &str, aliases: &[&str]) -> AuditResult {
        AuditResult {
            vulnerable: vec![PackageAuditResult {
                package: Package {
                    name: "chrono".into(),
                    version: "0.4.44".into(),
                    ecosystem: Ecosystem::CratesIo,
                },
                vulnerabilities: vec![Vulnerability {
                    id: id.into(),
                    summary: Some("summary".into()),
                    severity: None,
                    url: None,
                    fixed_version: None,
                    aliases: aliases.iter().map(|a| a.to_string()).collect(),
                    source: String::new(),
                    affected: Vec::new(),
                }],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn accepted_finding_leaves_vulnerable_and_keeps_its_reason() {
        let mut result = result_with("RUSTSEC-2026-0543", &[]);
        apply_suppressions(
            &mut result,
            &[entry("rustsec-2026-0543", None)],
            day(2026, 11, 26),
            true,
        );
        assert!(result.vulnerable.is_empty());
        assert_eq!(result.safe_count, 1);
        assert_eq!(result.suppressed.len(), 1);
        assert_eq!(result.suppressed[0].reason, "accepted");
        assert!(result.suppression_warnings.is_empty());
    }

    #[test]
    fn an_alias_names_the_advisory() {
        let mut result = result_with("GHSA-xxxx", &["CVE-2026-9"]);
        apply_suppressions(
            &mut result,
            &[entry("CVE-2026-9", None)],
            day(2026, 1, 1),
            true,
        );
        assert!(result.vulnerable.is_empty());
    }

    #[test]
    fn the_last_day_still_applies_and_the_next_day_does_not() {
        let last = Some(day(2027, 5, 31));
        let mut on_last_day = result_with("RUSTSEC-1", &[]);
        apply_suppressions(
            &mut on_last_day,
            &[entry("RUSTSEC-1", last)],
            day(2027, 5, 31),
            true,
        );
        assert!(on_last_day.vulnerable.is_empty());
        assert!(on_last_day.suppression_warnings.is_empty());

        let mut after = result_with("RUSTSEC-1", &[]);
        apply_suppressions(
            &mut after,
            &[entry("RUSTSEC-1", last)],
            day(2027, 6, 1),
            true,
        );
        assert_eq!(after.vulnerable.len(), 1);
        assert!(after.suppressed.is_empty());
        assert!(after.suppression_warnings[0].contains("expired on 2027-05-31"));
        assert!(after.suppression_warnings[0].contains("reported again"));
        assert_eq!(
            after.suppression_warnings.len(),
            1,
            "no extra unmatched warning"
        );
    }

    #[test]
    fn an_unmatched_entry_warns_only_when_the_whole_tree_was_audited() {
        let mut full = result_with("OTHER-1", &[]);
        apply_suppressions(
            &mut full,
            &[entry("RUSTSEC-1", None)],
            day(2026, 1, 1),
            true,
        );
        assert_eq!(full.vulnerable.len(), 1);
        assert!(full.suppression_warnings[0].contains("matches no advisory"));

        let mut narrowed = result_with("OTHER-1", &[]);
        apply_suppressions(
            &mut narrowed,
            &[entry("RUSTSEC-1", None)],
            day(2026, 1, 1),
            false,
        );
        assert!(narrowed.suppression_warnings.is_empty());
    }

    #[test]
    fn overlapping_entries_do_not_produce_false_warnings() {
        // An expired entry for the id and a live one for its alias: the
        // advisory is accepted, so nothing says it is reported again.
        let mut result = result_with("GHSA-xxxx", &["CVE-2026-9"]);
        apply_suppressions(
            &mut result,
            &[
                entry("GHSA-xxxx", Some(day(2020, 1, 1))),
                entry("CVE-2026-9", None),
            ],
            day(2026, 1, 1),
            true,
        );
        assert!(result.vulnerable.is_empty());
        assert_eq!(result.suppression_warnings.len(), 1);
        assert!(!result.suppression_warnings[0].contains("reported again"));

        // Two live entries naming one advisory: neither is unmatched.
        let mut result = result_with("GHSA-xxxx", &["CVE-2026-9"]);
        apply_suppressions(
            &mut result,
            &[entry("GHSA-xxxx", None), entry("CVE-2026-9", None)],
            day(2026, 1, 1),
            true,
        );
        assert!(result.suppression_warnings.is_empty());
    }

    #[test]
    fn a_finding_without_fetched_details_does_not_make_an_alias_entry_look_stale() {
        let mut result = result_with("GHSA-xxxx", &[]);
        result.vulnerable[0].vulnerabilities[0].summary = None;
        apply_suppressions(
            &mut result,
            &[entry("CVE-2026-9", None)],
            day(2026, 1, 1),
            true,
        );
        assert_eq!(result.vulnerable.len(), 1, "stays actionable");
        assert!(result.suppression_warnings.is_empty());
    }

    #[test]
    fn a_package_with_one_accepted_of_two_stays_vulnerable() {
        let mut result = result_with("RUSTSEC-1", &[]);
        let mut second = result.vulnerable[0].vulnerabilities[0].clone();
        second.id = "RUSTSEC-2".into();
        result.vulnerable[0].vulnerabilities.push(second);
        apply_suppressions(
            &mut result,
            &[entry("RUSTSEC-1", None)],
            day(2026, 1, 1),
            true,
        );
        assert_eq!(result.vulnerable.len(), 1);
        assert_eq!(result.vulnerable[0].vulnerabilities[0].id, "RUSTSEC-2");
        assert_eq!(result.safe_count, 0);
    }
}
