//! Security vulnerability auditing via OSV (Open Source Vulnerabilities) API

use anyhow::Result;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// OSV API base URL
const OSV_API_URL: &str = "https://api.osv.dev/v1";

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
}

impl Ecosystem {
    /// Convert to OSV API ecosystem string
    pub fn as_str(&self) -> &'static str {
        match self {
            Ecosystem::PyPI => "PyPI",
            Ecosystem::Npm => "npm",
            Ecosystem::CratesIo => "crates.io",
            Ecosystem::Go => "Go",
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

/// OSV API client for vulnerability checking
pub struct OsvClient {
    client: Client,
}

impl OsvClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("Failed to create HTTP client. This usually indicates a TLS/SSL configuration issue on your system."),
        }
    }

    /// Check a batch of packages for vulnerabilities
    pub async fn check_packages(&self, packages: &[Package]) -> Result<AuditResult> {
        let mut result = AuditResult::default();

        // Process in batches
        for chunk in packages.chunks(BATCH_SIZE) {
            match self.query_batch(chunk).await {
                Ok(batch_results) => {
                    for (package, vulns) in batch_results {
                        if vulns.is_empty() {
                            result.safe_count += 1;
                        } else {
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
            .post(format!("{}/querybatch", OSV_API_URL))
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
            .get(format!("{}/vulns/{}", OSV_API_URL, id))
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

        // Extract severity from database_specific or severity array
        let severity = vuln
            .severity
            .as_ref()
            .and_then(|s| s.first())
            .map(|s| s.score.clone())
            .or_else(|| {
                vuln.database_specific
                    .as_ref()
                    .and_then(|db| db.get("severity"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });

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
}
