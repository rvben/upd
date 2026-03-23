use super::{Registry, get_with_retry, http_error_message};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct TerraformRegistry {
    client: Client,
    api_url: String,
}

#[derive(Debug, Deserialize)]
struct ProviderVersionsResponse {
    versions: Vec<ProviderVersion>,
}

#[derive(Debug, Deserialize)]
struct ProviderVersion {
    version: String,
}

#[derive(Debug, Deserialize)]
struct ModuleVersionsResponse {
    modules: Vec<ModuleVersionList>,
}

#[derive(Debug, Deserialize)]
struct ModuleVersionList {
    versions: Vec<ModuleVersion>,
}

#[derive(Debug, Deserialize)]
struct ModuleVersion {
    version: String,
}

impl TerraformRegistry {
    pub fn new() -> Self {
        Self::with_api_url("https://registry.terraform.io".to_string())
    }

    #[cfg(test)]
    pub fn with_api_url(api_url: String) -> Self {
        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, api_url }
    }

    #[cfg(not(test))]
    fn with_api_url(api_url: String) -> Self {
        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, api_url }
    }

    /// Determine if a package identifier refers to a module (3 segments) or provider (2 segments)
    fn is_module(package: &str) -> bool {
        package.split('/').count() == 3
    }

    /// Fetch all versions for a provider (namespace/type)
    async fn get_provider_versions(&self, package: &str) -> Result<Vec<String>> {
        let url = format!("{}/v1/providers/{}/versions", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Provider",
                package,
                None
            )));
        }

        let data: ProviderVersionsResponse = response.json().await.map_err(|e| {
            anyhow!(
                "Failed to parse Terraform Registry response for '{}': {}",
                package,
                e
            )
        })?;

        Ok(data.versions.into_iter().map(|v| v.version).collect())
    }

    /// Fetch all versions for a module (namespace/name/provider)
    async fn get_module_versions(&self, package: &str) -> Result<Vec<String>> {
        let url = format!("{}/v1/modules/{}/versions", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Module",
                package,
                None
            )));
        }

        let data: ModuleVersionsResponse = response.json().await.map_err(|e| {
            anyhow!(
                "Failed to parse Terraform Registry response for '{}': {}",
                package,
                e
            )
        })?;

        let versions = data
            .modules
            .into_iter()
            .flat_map(|m| m.versions.into_iter().map(|v| v.version))
            .collect();

        Ok(versions)
    }

    /// Get all versions (dispatches to provider or module endpoint)
    async fn get_all_versions(&self, package: &str) -> Result<Vec<String>> {
        if Self::is_module(package) {
            self.get_module_versions(package).await
        } else {
            self.get_provider_versions(package).await
        }
    }

    /// Find the latest stable version from a list of version strings
    fn find_latest_stable(versions: &[String]) -> Option<String> {
        versions
            .iter()
            .filter(|v| !v.contains('-')) // Skip prereleases (semver convention)
            .filter_map(|v| semver::Version::parse(v).ok().map(|sv| (v.clone(), sv)))
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(v, _)| v)
    }

    /// Find the latest version including prereleases
    fn find_latest_any(versions: &[String]) -> Option<String> {
        versions
            .iter()
            .filter_map(|v| semver::Version::parse(v).ok().map(|sv| (v.clone(), sv)))
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(v, _)| v)
    }
}

impl Default for TerraformRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for TerraformRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let versions = self.get_all_versions(package).await?;

        Self::find_latest_stable(&versions).ok_or_else(|| {
            anyhow!(
                "No stable versions found for '{}' in Terraform Registry",
                package
            )
        })
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let versions = self.get_all_versions(package).await?;

        Self::find_latest_any(&versions)
            .ok_or_else(|| anyhow!("No versions found for '{}' in Terraform Registry", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let versions = self.get_all_versions(package).await?;

        // Parse constraint and find matching versions
        let matching: Vec<_> = versions
            .iter()
            .filter(|v| !v.contains('-')) // Skip prereleases
            .filter(|v| matches_terraform_constraint(v, constraints))
            .filter_map(|v| semver::Version::parse(v).ok().map(|sv| (v.clone(), sv)))
            .collect();

        matching
            .into_iter()
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(v, _)| v)
            .ok_or_else(|| {
                anyhow!(
                    "No version of '{}' matches constraints '{}'",
                    package,
                    constraints
                )
            })
    }

    fn name(&self) -> &'static str {
        "terraform"
    }
}

/// Check if a version matches a Terraform version constraint string.
/// Supports ~> (pessimistic), >=, <=, >, <, = operators.
/// Multiple constraints can be comma-separated: ">= 5.0, < 6.0"
fn matches_terraform_constraint(version: &str, constraint: &str) -> bool {
    // Split on commas for multiple constraints
    for part in constraint.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !matches_single_constraint(version, part) {
            return false;
        }
    }
    true
}

fn matches_single_constraint(version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();

    // Extract operator and required version
    let (op, required) = if let Some(rest) = constraint.strip_prefix("~>") {
        ("~>", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix(">=") {
        (">=", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix("<=") {
        ("<=", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix("!=") {
        ("!=", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix('>') {
        (">", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix('<') {
        ("<", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix('=') {
        ("=", rest.trim())
    } else {
        ("=", constraint)
    };

    let ver = parse_version_parts(version);
    let req = parse_version_parts(required);

    match op {
        ">=" => ver >= req,
        "<=" => ver <= req,
        ">" => ver > req,
        "<" => ver < req,
        "=" | "==" => ver == req,
        "!=" => ver != req,
        "~>" => {
            // Pessimistic constraint: ~> 5.0 means >= 5.0 and < 6.0
            // ~> 5.1.0 means >= 5.1.0 and < 5.2.0
            if ver < req {
                return false;
            }
            let req_parts: Vec<u64> = required.split('.').filter_map(|s| s.parse().ok()).collect();
            if req_parts.len() < 2 {
                return ver >= req;
            }
            let mut upper = req_parts.clone();
            let bump_idx = upper.len() - 2;
            upper[bump_idx] += 1;
            upper.truncate(bump_idx + 1);
            let ver_parts: Vec<u64> = version.split('.').filter_map(|s| s.parse().ok()).collect();
            for (v, u) in ver_parts.iter().zip(upper.iter()) {
                match v.cmp(u) {
                    std::cmp::Ordering::Less => return true,
                    std::cmp::Ordering::Greater => return false,
                    std::cmp::Ordering::Equal => continue,
                }
            }
            false
        }
        _ => false,
    }
}

fn parse_version_parts(v: &str) -> Vec<u64> {
    v.split('.').filter_map(|s| s.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_get_latest_provider_version() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/providers/hashicorp/aws/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"versions": [{"version": "5.83.0"}, {"version": "5.82.0"}, {"version": "4.67.0"}]}"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = TerraformRegistry::with_api_url(mock_server.uri());
        let version = registry.get_latest_version("hashicorp/aws").await.unwrap();
        assert_eq!(version, "5.83.0");
    }

    #[tokio::test]
    async fn test_get_latest_module_version() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/modules/terraform-aws-modules/vpc/aws/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"modules": [{"versions": [{"version": "5.1.0"}, {"version": "5.0.0"}, {"version": "4.0.0"}]}]}"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = TerraformRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version("terraform-aws-modules/vpc/aws")
            .await
            .unwrap();
        assert_eq!(version, "5.1.0");
    }

    #[tokio::test]
    async fn test_provider_not_found() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/providers/nonexistent/provider/versions"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = TerraformRegistry::with_api_url(mock_server.uri());
        let result = registry.get_latest_version("nonexistent/provider").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_registry_name() {
        let registry = TerraformRegistry::new();
        assert_eq!(registry.name(), "terraform");
    }

    #[tokio::test]
    async fn test_skips_prereleases() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/providers/hashicorp/aws/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"versions": [{"version": "6.0.0-beta1"}, {"version": "5.83.0"}, {"version": "5.82.0"}]}"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = TerraformRegistry::with_api_url(mock_server.uri());
        let version = registry.get_latest_version("hashicorp/aws").await.unwrap();
        assert_eq!(version, "5.83.0");
    }

    #[test]
    fn test_matches_terraform_constraint_pessimistic() {
        // ~> 5.0 means >= 5.0 and < 6.0
        assert!(matches_terraform_constraint("5.0.0", "~> 5.0"));
        assert!(matches_terraform_constraint("5.83.0", "~> 5.0"));
        assert!(!matches_terraform_constraint("6.0.0", "~> 5.0"));
        assert!(!matches_terraform_constraint("4.0.0", "~> 5.0"));

        // ~> 5.1.0 means >= 5.1.0 and < 5.2.0
        assert!(matches_terraform_constraint("5.1.0", "~> 5.1.0"));
        assert!(matches_terraform_constraint("5.1.5", "~> 5.1.0"));
        assert!(!matches_terraform_constraint("5.2.0", "~> 5.1.0"));
        assert!(!matches_terraform_constraint("5.0.0", "~> 5.1.0"));
    }

    #[test]
    fn test_matches_terraform_constraint_comparison() {
        assert!(matches_terraform_constraint("5.0.0", ">= 4.0.0"));
        assert!(!matches_terraform_constraint("3.0.0", ">= 4.0.0"));
        assert!(matches_terraform_constraint("5.0.0", "< 6.0.0"));
        assert!(!matches_terraform_constraint("6.0.0", "< 6.0.0"));
    }

    #[test]
    fn test_matches_terraform_constraint_compound() {
        // >= 5.0, < 6.0
        assert!(matches_terraform_constraint("5.5.0", ">= 5.0, < 6.0"));
        assert!(!matches_terraform_constraint("6.0.0", ">= 5.0, < 6.0"));
        assert!(!matches_terraform_constraint("4.0.0", ">= 5.0, < 6.0"));
    }

    #[test]
    fn test_is_module() {
        assert!(!TerraformRegistry::is_module("hashicorp/aws"));
        assert!(TerraformRegistry::is_module(
            "terraform-aws-modules/vpc/aws"
        ));
    }
}
