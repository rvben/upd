//! Security vulnerability auditing via OSV (Open Source Vulnerabilities) API

pub mod cache;
pub mod cvss;

use anyhow::Result;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default OSV API base URL (override via `OsvClient::with_base_url`).
const DEFAULT_OSV_API_URL: &str = "https://api.osv.dev/v1";

/// Maximum packages per batch request (OSV limit)
const BATCH_SIZE: usize = 1000;

/// Maximum concurrent requests for fetching vulnerability details
const MAX_CONCURRENT_REQUESTS: usize = 20;

/// Ecosystem names for OSV API
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecosystem {
    PyPI,
    Npm,
    CratesIo,
    Go,
    RubyGems,
    NuGet,
}

impl Ecosystem {
    /// Convert to OSV API ecosystem string
    pub fn as_str(&self) -> &'static str {
        match self {
            Ecosystem::PyPI => "PyPI",
            Ecosystem::Npm => "npm",
            Ecosystem::CratesIo => "crates.io",
            Ecosystem::Go => "Go",
            Ecosystem::RubyGems => "RubyGems",
            Ecosystem::NuGet => "NuGet",
        }
    }
}

/// A package to check for vulnerabilities
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub ecosystem: Ecosystem,
}

/// A vulnerability found in a package
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vulnerability {
    /// Unique vulnerability ID (e.g., GHSA-xxxx-xxxx-xxxx, CVE-xxxx-xxxx)
    pub id: String,
    /// Short summary of the vulnerability
    pub summary: Option<String>,
    /// Severity level if available
    pub severity: Option<String>,
    /// URL for more information
    pub url: Option<String>,
    /// Fixed version if available
    pub fixed_version: Option<String>,
}

/// Result of checking a package for vulnerabilities
#[derive(Debug, Clone)]
pub struct PackageAuditResult {
    pub package: Package,
    pub vulnerabilities: Vec<Vulnerability>,
}

/// Overall audit result
#[derive(Debug, Default)]
pub struct AuditResult {
    /// Packages with vulnerabilities
    pub vulnerable: Vec<PackageAuditResult>,
    /// Packages that are safe
    pub safe_count: usize,
    /// Errors encountered during audit
    pub errors: Vec<String>,
}

impl AuditResult {
    pub fn total_vulnerabilities(&self) -> usize {
        self.vulnerable
            .iter()
            .map(|p| p.vulnerabilities.len())
            .sum()
    }

    pub fn vulnerable_packages(&self) -> usize {
        self.vulnerable.len()
    }
}

/// Compute which packages can be auto-fixed and which cannot.
///
/// Returns two collections:
/// - `fixable`: a map of `package_name → min_safe_version`. The min safe version
///   is the maximum `fixed_version` across all vulnerabilities for that package;
///   upgrading to this version clears every known CVE.
/// - `unfixable`: a list of `(package_name, reason)` for packages that have at
///   least one vulnerability with no `fixed_version`.
///
/// A package appears in at most one of the two collections: if it has even one
/// vulnerability without a fix, it is considered unfixable and excluded from the
/// fixable set.
pub fn compute_fix_plan(audit: &AuditResult) -> (HashMap<String, String>, Vec<(String, String)>) {
    let mut fixable: HashMap<String, String> = HashMap::new();
    let mut unfixable: Vec<(String, String)> = Vec::new();

    for pkg_result in &audit.vulnerable {
        let name = &pkg_result.package.name;

        // Check whether any vulnerability has no fixed_version.
        let blocking: Option<&Vulnerability> = pkg_result
            .vulnerabilities
            .iter()
            .find(|v| v.fixed_version.is_none());

        if let Some(blocker) = blocking {
            unfixable.push((name.clone(), format!("{} has no fixed version", blocker.id)));
            continue;
        }

        // All vulnerabilities have a fixed_version — find the maximum.
        let max_fixed = pkg_result
            .vulnerabilities
            .iter()
            .filter_map(|v| v.fixed_version.as_deref())
            .max_by(|a, b| compare_fix_versions(a, b));

        if let Some(version) = max_fixed {
            fixable.insert(name.clone(), version.to_string());
        }
    }

    (fixable, unfixable)
}

/// Compare two version strings for ordering, preferring semver but falling back
/// to lexicographic comparison for non-semver ecosystems.
fn compare_fix_versions(a: &str, b: &str) -> std::cmp::Ordering {
    match (semver_parse(a), semver_parse(b)) {
        (Some(va), Some(vb)) => va.cmp(&vb),
        _ => a.cmp(b),
    }
}

/// Parse a version string as semver, accepting an optional leading `v`.
///
/// Returns `(major, minor, patch, is_stable)` where `is_stable` is 1 for stable
/// releases and 0 for pre-releases (e.g. `2.0.0-rc1`). This ensures stable
/// versions beat pre-releases when all numeric components are equal.
fn semver_parse(v: &str) -> Option<(u64, u64, u64, u8)> {
    let v = v.trim_start_matches('v');
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let major: u64 = parts[0].parse().ok()?;
    let minor: u64 = parts[1].parse().ok()?;
    // Patch may carry a pre-release or build suffix (e.g. "0-rc1", "3+build1").
    // Take only the leading digit run so "0-rc1" → patch=0, rest="-rc1".
    let patch_part = parts.get(2).copied().unwrap_or("0");
    let (patch_digits, rest) = patch_part
        .split_once(|c: char| !c.is_ascii_digit())
        .unwrap_or((patch_part, ""));
    let patch: u64 = if patch_digits.is_empty() {
        0
    } else {
        patch_digits.parse().ok()?
    };
    // Stability flag: 1 = stable, 0 = pre-release. Stable wins ties.
    let is_stable: u8 = if rest.is_empty() { 1 } else { 0 };
    Some((major, minor, patch, is_stable))
}

/// Return a sort key for a severity string such that Critical sorts first.
///
/// Lower numeric values sort earlier, so Critical = 0, Unknown = 5.
pub(crate) fn severity_sort_key(severity: Option<&str>) -> u8 {
    match severity {
        Some("Critical") => 0,
        Some("High") => 1,
        Some("Medium") => 2,
        Some("Low") => 3,
        Some("None") => 4,
        _ => 5, // Unknown or unexpected values sort last
    }
}

/// OSV API client for vulnerability checking
pub struct OsvClient {
    client: Client,
    base_url: String,
}

impl OsvClient {
    pub fn new() -> Self {
        let base_url =
            std::env::var("OSV_API_URL").unwrap_or_else(|_| DEFAULT_OSV_API_URL.to_string());
        Self::with_base_url(base_url)
    }

    /// Create a client pointing at a custom base URL (used by tests).
    pub fn with_base_url(base_url: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("Failed to create HTTP client. This usually indicates a TLS/SSL configuration issue on your system."),
            base_url,
        }
    }

    /// Check a batch of packages for vulnerabilities
    pub async fn check_packages(&self, packages: &[Package]) -> Result<AuditResult> {
        self.check_packages_cached(packages, None, false).await
    }

    /// Check packages for vulnerabilities with optional disk-backed caching.
    ///
    /// - `cache`: when `Some`, fresh cache entries are returned directly and new
    ///   OSV responses are written back. Pass `None` to disable caching entirely
    ///   (equivalent to the previous `check_packages` behavior).
    /// - `offline`: when `true`, the cache is the only source of truth. Any
    ///   package whose cache entry is missing or expired is reported as an error
    ///   and contributes to exit-code 2. OSV is never contacted.
    pub async fn check_packages_cached(
        &self,
        packages: &[Package],
        cache: Option<&Arc<Mutex<cache::AuditCache>>>,
        offline: bool,
    ) -> Result<AuditResult> {
        use cache::AuditKey;

        let mut result = AuditResult::default();

        // Partition packages into cache-hits and misses.
        let mut uncached: Vec<Package> = Vec::new();

        for package in packages {
            let key = AuditKey::new(package.ecosystem.as_str(), &package.name, &package.version);

            // Try cache first (if enabled).
            if let Some(c) = cache
                && let Ok(guard) = c.lock()
                && let Some(entry) = guard.get(&key)
            {
                // Cache hit — replay the stored result.
                let mut vulns = entry.vulnerabilities.clone();
                if vulns.is_empty() {
                    result.safe_count += 1;
                } else {
                    vulns.sort_by_key(|v| severity_sort_key(v.severity.as_deref()));
                    result.vulnerable.push(PackageAuditResult {
                        package: package.clone(),
                        vulnerabilities: vulns,
                    });
                }
                continue;
            }

            // Cache miss.
            if offline {
                result.errors.push(format!(
                    "cache miss, cannot audit {}/{} {} offline",
                    package.ecosystem.as_str(),
                    package.name,
                    package.version
                ));
            } else {
                uncached.push(package.clone());
            }
        }

        // Query OSV for packages not satisfied by the cache.
        for chunk in uncached.chunks(BATCH_SIZE) {
            match self.query_batch(chunk).await {
                Ok(batch_results) => {
                    for (package, mut vulns) in batch_results {
                        let key = AuditKey::new(
                            package.ecosystem.as_str(),
                            &package.name,
                            &package.version,
                        );

                        // Write back to cache before sorting/consuming.
                        if let Some(c) = cache
                            && let Ok(mut guard) = c.lock()
                        {
                            guard.set(&key, vulns.clone());
                        }

                        if vulns.is_empty() {
                            result.safe_count += 1;
                        } else {
                            vulns.sort_by_key(|v| severity_sort_key(v.severity.as_deref()));
                            result.vulnerable.push(PackageAuditResult {
                                package,
                                vulnerabilities: vulns,
                            });
                        }
                    }
                }
                Err(e) => {
                    result.errors.push(format!("Batch query failed: {}", e));
                }
            }
        }

        Ok(result)
    }

    /// Query OSV API for a batch of packages
    async fn query_batch(
        &self,
        packages: &[Package],
    ) -> Result<Vec<(Package, Vec<Vulnerability>)>> {
        let queries: Vec<OsvQuery> = packages
            .iter()
            .map(|p| OsvQuery {
                package: OsvPackage {
                    name: p.name.clone(),
                    ecosystem: p.ecosystem.as_str().to_string(),
                },
                version: p.version.clone(),
            })
            .collect();

        let request = OsvBatchRequest { queries };

        let response = self
            .client
            .post(format!("{}/querybatch", self.base_url))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("OSV API error: HTTP {}", response.status());
        }

        let batch_response: OsvBatchResponse = response.json().await?;

        // Map results back to packages
        let mut results = Vec::new();
        for (i, osv_result) in batch_response.results.into_iter().enumerate() {
            if i >= packages.len() {
                break;
            }

            let package = packages[i].clone();
            let vulns = self.fetch_vulnerability_details(&osv_result.vulns).await?;
            results.push((package, vulns));
        }

        Ok(results)
    }

    /// Fetch full vulnerability details for a list of vulnerability IDs (in parallel)
    async fn fetch_vulnerability_details(
        &self,
        vuln_refs: &[OsvVulnRef],
    ) -> Result<Vec<Vulnerability>> {
        let vulnerabilities: Vec<Vulnerability> = stream::iter(vuln_refs)
            .map(|vuln_ref| async move {
                match self.fetch_vuln_by_id(&vuln_ref.id).await {
                    Ok(vuln) => vuln,
                    Err(_) => {
                        // If we can't fetch details, create a minimal entry
                        Vulnerability {
                            id: vuln_ref.id.clone(),
                            summary: None,
                            severity: None,
                            url: Some(format!("https://osv.dev/vulnerability/{}", vuln_ref.id)),
                            fixed_version: None,
                        }
                    }
                }
            })
            .buffer_unordered(MAX_CONCURRENT_REQUESTS)
            .collect()
            .await;

        Ok(vulnerabilities)
    }

    /// Fetch vulnerability details by ID
    async fn fetch_vuln_by_id(&self, id: &str) -> Result<Vulnerability> {
        let response = self
            .client
            .get(format!("{}/vulns/{}", self.base_url, id))
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Failed to fetch vulnerability {}: HTTP {}",
                id,
                response.status()
            );
        }

        let vuln: OsvVulnerability = response.json().await?;

        // Resolve severity: prefer database_specific label, then CVSS vector.
        // database_specific.severity takes priority because it is curated by
        // the advisory database and is often more accurate than a computed score.
        let db_severity = vuln
            .database_specific
            .as_ref()
            .and_then(|db| db.get("severity"))
            .and_then(|v| v.as_str());
        let cvss_vector = vuln
            .severity
            .as_ref()
            .and_then(|s| s.first())
            .map(|s| s.score.as_str());
        let resolved = cvss::resolve_severity(db_severity, cvss_vector);
        // All resolved labels, including "Unknown", are serialised so that
        // JSON consumers see a consistent schema regardless of whether
        // severity data was available. "Unknown" sorts last in the sort key,
        // matching the same position as None would have occupied.
        let severity = Some(resolved.as_severity_string());

        // Extract fixed version from affected ranges
        let fixed_version = vuln
            .affected
            .as_ref()
            .and_then(|affected| affected.first())
            .and_then(|a| a.ranges.as_ref())
            .and_then(|ranges| ranges.first())
            .and_then(|r| r.events.as_ref())
            .and_then(|events| events.iter().find_map(|e| e.fixed.clone()));

        // Get reference URL
        let url = vuln
            .references
            .as_ref()
            .and_then(|refs| refs.first())
            .map(|r| r.url.clone())
            .unwrap_or_else(|| format!("https://osv.dev/vulnerability/{}", id));

        Ok(Vulnerability {
            id: vuln.id,
            summary: vuln.summary,
            severity,
            url: Some(url),
            fixed_version,
        })
    }
}

impl Default for OsvClient {
    fn default() -> Self {
        Self::new()
    }
}

// OSV API request/response types

#[derive(Debug, Serialize)]
struct OsvQuery {
    package: OsvPackage,
    version: String,
}

#[derive(Debug, Serialize)]
struct OsvPackage {
    name: String,
    ecosystem: String,
}

#[derive(Debug, Serialize)]
struct OsvBatchRequest {
    queries: Vec<OsvQuery>,
}

#[derive(Debug, Deserialize)]
struct OsvBatchResponse {
    results: Vec<OsvBatchResult>,
}

#[derive(Debug, Deserialize)]
struct OsvBatchResult {
    #[serde(default)]
    vulns: Vec<OsvVulnRef>,
}

#[derive(Debug, Deserialize)]
struct OsvVulnRef {
    id: String,
}

#[derive(Debug, Deserialize)]
struct OsvVulnerability {
    id: String,
    summary: Option<String>,
    severity: Option<Vec<OsvSeverity>>,
    references: Option<Vec<OsvReference>>,
    affected: Option<Vec<OsvAffected>>,
    database_specific: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct OsvSeverity {
    score: String,
}

#[derive(Debug, Deserialize)]
struct OsvReference {
    url: String,
}

#[derive(Debug, Deserialize)]
struct OsvAffected {
    ranges: Option<Vec<OsvRange>>,
}

#[derive(Debug, Deserialize)]
struct OsvRange {
    events: Option<Vec<OsvEvent>>,
}

#[derive(Debug, Deserialize)]
struct OsvEvent {
    fixed: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ecosystem_as_str() {
        assert_eq!(Ecosystem::PyPI.as_str(), "PyPI");
        assert_eq!(Ecosystem::Npm.as_str(), "npm");
        assert_eq!(Ecosystem::CratesIo.as_str(), "crates.io");
        assert_eq!(Ecosystem::Go.as_str(), "Go");
        assert_eq!(Ecosystem::RubyGems.as_str(), "RubyGems");
        // OSV schema uses "NuGet" (capital N, capital G) as the ecosystem
        // identifier: https://ossf.github.io/osv-schema/#affectedpackage-field
        assert_eq!(Ecosystem::NuGet.as_str(), "NuGet");
    }

    #[test]
    fn test_every_ecosystem_has_unique_osv_identifier() {
        // Protects against accidental collisions when new ecosystems are
        // added. All OSV ecosystem IDs must be distinct.
        let all = [
            Ecosystem::PyPI,
            Ecosystem::Npm,
            Ecosystem::CratesIo,
            Ecosystem::Go,
            Ecosystem::RubyGems,
            Ecosystem::NuGet,
        ];
        let mut seen = std::collections::HashSet::new();
        for eco in all {
            assert!(
                seen.insert(eco.as_str()),
                "duplicate ecosystem identifier: {}",
                eco.as_str()
            );
        }
    }

    #[test]
    fn test_audit_result_counts() {
        let mut result = AuditResult {
            safe_count: 5,
            ..Default::default()
        };
        result.vulnerable.push(PackageAuditResult {
            package: Package {
                name: "test".to_string(),
                version: "1.0.0".to_string(),
                ecosystem: Ecosystem::PyPI,
            },
            vulnerabilities: vec![
                Vulnerability {
                    id: "CVE-2024-001".to_string(),
                    summary: Some("Test vuln".to_string()),
                    severity: Some("HIGH".to_string()),
                    url: None,
                    fixed_version: Some("1.0.1".to_string()),
                },
                Vulnerability {
                    id: "CVE-2024-002".to_string(),
                    summary: None,
                    severity: None,
                    url: None,
                    fixed_version: None,
                },
            ],
        });

        assert_eq!(result.total_vulnerabilities(), 2);
        assert_eq!(result.vulnerable_packages(), 1);
    }

    #[tokio::test]
    async fn test_check_packages_sends_nuget_ecosystem_and_reports_vulnerabilities() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let expected_batch_body = serde_json::json!({
            "queries": [
                {
                    "package": { "name": "Newtonsoft.Json", "ecosystem": "NuGet" },
                    "version": "12.0.2"
                }
            ]
        });

        Mock::given(method("POST"))
            .and(path("/querybatch"))
            .and(body_json(&expected_batch_body))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    { "vulns": [{ "id": "GHSA-nuget-test" }] }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/vulns/GHSA-nuget-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "GHSA-nuget-test",
                "summary": "test",
                "affected": [{
                    "ranges": [{
                        "events": [{ "fixed": "13.0.1" }]
                    }]
                }],
                "references": [{ "url": "https://example/nuget" }]
            })))
            .mount(&server)
            .await;

        let client = OsvClient::with_base_url(server.uri());
        let audit = client
            .check_packages(&[Package {
                name: "Newtonsoft.Json".into(),
                version: "12.0.2".into(),
                ecosystem: Ecosystem::NuGet,
            }])
            .await
            .unwrap();

        assert_eq!(audit.vulnerable.len(), 1);
        assert_eq!(audit.vulnerable[0].package.name, "Newtonsoft.Json");
        assert_eq!(audit.vulnerable[0].vulnerabilities.len(), 1);
        assert_eq!(audit.vulnerable[0].vulnerabilities[0].id, "GHSA-nuget-test");
        assert_eq!(
            audit.vulnerable[0].vulnerabilities[0]
                .fixed_version
                .as_deref(),
            Some("13.0.1")
        );
    }

    #[tokio::test]
    async fn test_check_packages_reports_safe_when_no_vulnerabilities() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/querybatch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{ "vulns": [] }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = OsvClient::with_base_url(server.uri());
        let audit = client
            .check_packages(&[Package {
                name: "Serilog".into(),
                version: "4.0.0".into(),
                ecosystem: Ecosystem::NuGet,
            }])
            .await
            .unwrap();

        assert!(audit.vulnerable.is_empty());
        assert_eq!(audit.safe_count, 1);
        assert!(audit.errors.is_empty());
    }

    // ─── compute_fix_plan unit tests ──────────────────────────────────────────

    fn make_vuln(id: &str, fixed: Option<&str>) -> Vulnerability {
        Vulnerability {
            id: id.to_string(),
            summary: None,
            severity: None,
            url: None,
            fixed_version: fixed.map(str::to_string),
        }
    }

    fn make_pkg_result(name: &str, vulns: Vec<Vulnerability>) -> PackageAuditResult {
        PackageAuditResult {
            package: Package {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                ecosystem: Ecosystem::PyPI,
            },
            vulnerabilities: vulns,
        }
    }

    #[test]
    fn fix_plan_empty_audit_returns_empty_plan() {
        let audit = AuditResult::default();
        let (fixable, unfixable) = compute_fix_plan(&audit);
        assert!(fixable.is_empty());
        assert!(unfixable.is_empty());
    }

    #[test]
    fn fix_plan_all_vulns_have_fixed_version() {
        let audit = AuditResult {
            vulnerable: vec![make_pkg_result(
                "requests",
                vec![
                    make_vuln("CVE-2024-001", Some("2.28.0")),
                    make_vuln("CVE-2024-002", Some("2.30.0")),
                ],
            )],
            safe_count: 0,
            errors: vec![],
        };
        let (fixable, unfixable) = compute_fix_plan(&audit);
        assert!(unfixable.is_empty());
        assert_eq!(fixable.get("requests").map(|s| s.as_str()), Some("2.30.0"));
    }

    #[test]
    fn fix_plan_one_vuln_missing_fixed_version_makes_unfixable() {
        let audit = AuditResult {
            vulnerable: vec![make_pkg_result(
                "django",
                vec![
                    make_vuln("CVE-2024-003", Some("3.2.0")),
                    make_vuln("CVE-2024-004", None),
                ],
            )],
            safe_count: 0,
            errors: vec![],
        };
        let (fixable, unfixable) = compute_fix_plan(&audit);
        assert!(fixable.is_empty());
        assert_eq!(unfixable.len(), 1);
        assert_eq!(unfixable[0].0, "django");
        assert!(
            unfixable[0].1.contains("CVE-2024-004"),
            "reason should name the blocking vuln: {}",
            unfixable[0].1
        );
    }

    #[test]
    fn fix_plan_multiple_packages_mixed_fixability() {
        let audit = AuditResult {
            vulnerable: vec![
                make_pkg_result("fixable-pkg", vec![make_vuln("CVE-A", Some("1.5.0"))]),
                make_pkg_result("broken-pkg", vec![make_vuln("CVE-B", None)]),
            ],
            safe_count: 1,
            errors: vec![],
        };
        let (fixable, unfixable) = compute_fix_plan(&audit);
        assert_eq!(fixable.len(), 1);
        assert!(fixable.contains_key("fixable-pkg"));
        assert_eq!(unfixable.len(), 1);
        assert_eq!(unfixable[0].0, "broken-pkg");
    }

    #[test]
    fn fix_plan_max_fixed_version_wins_when_multiple_vulns() {
        let audit = AuditResult {
            vulnerable: vec![make_pkg_result(
                "flask",
                vec![
                    make_vuln("CVE-X", Some("2.0.0")),
                    make_vuln("CVE-Y", Some("2.3.1")),
                    make_vuln("CVE-Z", Some("2.1.0")),
                ],
            )],
            safe_count: 0,
            errors: vec![],
        };
        let (fixable, unfixable) = compute_fix_plan(&audit);
        assert!(unfixable.is_empty());
        assert_eq!(fixable.get("flask").map(|s| s.as_str()), Some("2.3.1"));
    }

    #[test]
    fn fix_plan_prefers_stable_over_prerelease() {
        // One vuln fixed at "2.0.0-rc1", another at "2.0.0".
        // The stable release "2.0.0" must win regardless of iteration order.
        for vulns in [
            vec![
                make_vuln("CVE-pre", Some("2.0.0-rc1")),
                make_vuln("CVE-stable", Some("2.0.0")),
            ],
            vec![
                make_vuln("CVE-stable", Some("2.0.0")),
                make_vuln("CVE-pre", Some("2.0.0-rc1")),
            ],
        ] {
            let audit = AuditResult {
                vulnerable: vec![make_pkg_result("mypkg", vulns)],
                safe_count: 0,
                errors: vec![],
            };
            let (fixable, unfixable) = compute_fix_plan(&audit);
            assert!(unfixable.is_empty());
            assert_eq!(
                fixable.get("mypkg").map(|s| s.as_str()),
                Some("2.0.0"),
                "stable 2.0.0 must beat pre-release 2.0.0-rc1"
            );
        }
    }

    #[test]
    fn fix_plan_semver_ordering_not_lexicographic() {
        // "2.10.0" > "2.9.0" semver-wise but lexicographically "2.9.0" > "2.10.0"
        let audit = AuditResult {
            vulnerable: vec![make_pkg_result(
                "pkg",
                vec![
                    make_vuln("CVE-1", Some("2.9.0")),
                    make_vuln("CVE-2", Some("2.10.0")),
                ],
            )],
            safe_count: 0,
            errors: vec![],
        };
        let (fixable, _) = compute_fix_plan(&audit);
        assert_eq!(fixable.get("pkg").map(|s| s.as_str()), Some("2.10.0"));
    }

    // ─── check_packages_cached unit tests ────────────────────────────────────

    fn sample_package(name: &str) -> Package {
        Package {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            ecosystem: Ecosystem::PyPI,
        }
    }

    #[tokio::test]
    async fn cached_offline_with_empty_cache_reports_error_per_package() {
        // With an empty cache and offline=true, every package should produce
        // one error and OSV should never be contacted (no mock server needed).
        let client = OsvClient::with_base_url("http://127.0.0.1:0".to_string());
        let cache = cache::AuditCache::new_shared();
        let pkgs = vec![sample_package("requests"), sample_package("flask")];

        let result = client
            .check_packages_cached(&pkgs, Some(&cache), true)
            .await
            .unwrap();

        assert_eq!(
            result.errors.len(),
            2,
            "one error per cache-miss package in offline mode"
        );
        assert!(result.vulnerable.is_empty());
        assert_eq!(result.safe_count, 0);
        for err in &result.errors {
            assert!(
                err.contains("cache miss"),
                "error message should mention 'cache miss': {err}"
            );
        }
    }

    #[tokio::test]
    async fn cached_offline_with_populated_cache_skips_osv() {
        use cache::{AuditCache, AuditKey};

        // Pre-populate cache with a known vulnerability entry.
        let cache = AuditCache::new_shared();
        {
            let mut guard = cache.lock().unwrap();
            let key = AuditKey::new("PyPI", "requests", "1.0.0");
            guard.set(
                &key,
                vec![Vulnerability {
                    id: "CVE-CACHED".to_string(),
                    summary: None,
                    severity: None,
                    url: None,
                    fixed_version: None,
                }],
            );
        }

        // Point client at an unused address — if OSV were contacted, the test
        // would fail with a connection error.
        let client = OsvClient::with_base_url("http://127.0.0.1:0".to_string());
        let pkgs = vec![sample_package("requests")];

        let result = client
            .check_packages_cached(&pkgs, Some(&cache), true)
            .await
            .unwrap();

        assert!(
            result.errors.is_empty(),
            "no errors when cache is populated"
        );
        assert_eq!(result.vulnerable.len(), 1);
        assert_eq!(result.vulnerable[0].vulnerabilities[0].id, "CVE-CACHED");
    }

    #[tokio::test]
    async fn cached_online_miss_queries_osv_and_populates_cache() {
        use cache::{AuditCache, AuditKey};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/querybatch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{ "vulns": [] }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let cache = AuditCache::new_shared();
        let client = OsvClient::with_base_url(server.uri());
        let pkgs = vec![sample_package("safe-pkg")];

        let result = client
            .check_packages_cached(&pkgs, Some(&cache), false)
            .await
            .unwrap();

        assert!(result.errors.is_empty());
        assert_eq!(result.safe_count, 1);

        // Verify that the cache was populated.
        let guard = cache.lock().unwrap();
        let key = AuditKey::new("PyPI", "safe-pkg", "1.0.0");
        let entry = guard
            .get(&key)
            .expect("cache should be populated after OSV query");
        assert!(entry.vulnerabilities.is_empty());
    }

    #[tokio::test]
    async fn cached_online_hit_skips_osv() {
        use cache::{AuditCache, AuditKey};
        use wiremock::MockServer;

        // Start a mock server but mount NO mocks — any request would be unexpected.
        let server = MockServer::start().await;

        let cache = AuditCache::new_shared();
        {
            let mut guard = cache.lock().unwrap();
            let key = AuditKey::new("PyPI", "cached-pkg", "1.0.0");
            guard.set(&key, vec![]);
        }

        let client = OsvClient::with_base_url(server.uri());
        let pkgs = vec![Package {
            name: "cached-pkg".to_string(),
            version: "1.0.0".to_string(),
            ecosystem: Ecosystem::PyPI,
        }];

        let result = client
            .check_packages_cached(&pkgs, Some(&cache), false)
            .await
            .unwrap();

        assert!(result.errors.is_empty());
        assert_eq!(result.safe_count, 1);
        // wiremock will assert that no requests were received when the server drops.
    }
}
