//! Security vulnerability auditing via OSV (Open Source Vulnerabilities) API

pub mod cvss;

use anyhow::Result;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
#[derive(Debug, Clone)]
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
        let mut result = AuditResult::default();

        // Process in batches
        for chunk in packages.chunks(BATCH_SIZE) {
            match self.query_batch(chunk).await {
                Ok(batch_results) => {
                    for (package, mut vulns) in batch_results {
                        if vulns.is_empty() {
                            result.safe_count += 1;
                        } else {
                            // Sort vulnerabilities by severity descending (Critical first).
                            // Stable sort preserves the original order for equal severities.
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
}
