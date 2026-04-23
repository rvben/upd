use super::{Registry, VersionMeta, get_with_retry, http_error_message};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct RubyGemsRegistry {
    client: Client,
    api_url: String,
}

#[derive(Debug, Deserialize)]
struct GemInfo {
    version: String,
}

#[derive(Debug, Deserialize)]
struct GemVersion {
    number: String,
    prerelease: bool,
    /// RubyGems marks yanked releases with this flag. Older API responses
    /// omit the field; `serde(default)` treats that as "not yanked".
    #[serde(default)]
    yanked: bool,
    #[serde(default)]
    created_at: Option<String>,
}

impl RubyGemsRegistry {
    pub fn new() -> Self {
        Self::with_api_url("https://rubygems.org".to_string())
    }

    pub fn with_api_url(api_url: String) -> Self {
        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client. This usually indicates a TLS/SSL configuration issue on your system.");

        Self { client, api_url }
    }
}

impl Default for RubyGemsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for RubyGemsRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let url = format!("{}/api/v1/gems/{}.json", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Gem",
                package,
                None
            )));
        }

        let gem_info: GemInfo = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse RubyGems response for '{}': {}", package, e))?;

        Ok(gem_info.version)
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let url = format!("{}/api/v1/versions/{}.json", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Gem",
                package,
                None
            )));
        }

        let versions: Vec<GemVersion> = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse RubyGems versions for '{}': {}", package, e))?;

        // Versions are returned newest first by RubyGems API
        versions
            .iter()
            .find(|v| !v.yanked)
            .map(|v| v.number.clone())
            .ok_or_else(|| anyhow!("Gem '{}' has no versions", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let url = format!("{}/api/v1/versions/{}.json", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Gem",
                package,
                None
            )));
        }

        let versions: Vec<GemVersion> = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse RubyGems versions for '{}': {}", package, e))?;

        // Parse the constraint (e.g., "~> 7.1", ">= 4.9.0")
        // For now, return latest stable version that satisfies semver constraints
        for version in &versions {
            if version.prerelease || version.yanked {
                continue;
            }

            if matches_ruby_constraint(&version.number, constraints) {
                return Ok(version.number.clone());
            }
        }

        Err(anyhow!(
            "No version of gem '{}' matches constraints '{}'",
            package,
            constraints
        ))
    }

    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        let url = format!("{}/api/v1/versions/{}.json", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(anyhow!(http_error_message(status, "Gem", package, None)));
        }

        let items: Vec<GemVersion> = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse RubyGems versions for '{}': {}", package, e))?;

        Ok(items
            .into_iter()
            .map(|v| {
                let published_at = v
                    .created_at
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc));
                VersionMeta {
                    version: v.number,
                    published_at,
                    yanked: v.yanked,
                    prerelease: v.prerelease,
                }
            })
            .collect())
    }

    fn name(&self) -> &'static str {
        "rubygems"
    }
}

/// Check if a version matches a Ruby version constraint.
/// Supports ~> (pessimistic), >=, <=, >, <, = operators.
fn matches_ruby_constraint(version: &str, constraint: &str) -> bool {
    let parts: Vec<&str> = constraint.trim().splitn(2, ' ').collect();
    let (op, required) = match parts.len() {
        2 => (parts[0].trim(), parts[1].trim()),
        1 => ("=", parts[0].trim()),
        _ => return false,
    };

    let ver = parse_ruby_version(version);
    let req = parse_ruby_version(required);

    match op {
        ">=" => ver >= req,
        "<=" => ver <= req,
        ">" => ver > req,
        "<" => ver < req,
        "=" | "==" => ver == req,
        "!=" => ver != req,
        "~>" => {
            // Pessimistic constraint: ~> 2.1 means >= 2.1 and < 3.0
            // ~> 2.1.0 means >= 2.1.0 and < 2.2.0
            if ver < req {
                return false;
            }
            // Upper bound: bump the second-to-last component
            let req_parts: Vec<u64> = required.split('.').filter_map(|s| s.parse().ok()).collect();
            if req_parts.len() < 2 {
                return ver >= req;
            }
            let mut upper = req_parts.clone();
            let bump_idx = upper.len() - 2;
            upper[bump_idx] += 1;
            // Truncate to just the bumped component (upper bound is exclusive)
            upper.truncate(bump_idx + 1);
            let ver_parts: Vec<u64> = version.split('.').filter_map(|s| s.parse().ok()).collect();
            // Compare version < upper bound (compare only up to upper's length)
            for (v, u) in ver_parts.iter().zip(upper.iter()) {
                match v.cmp(u) {
                    std::cmp::Ordering::Less => return true,
                    std::cmp::Ordering::Greater => return false,
                    std::cmp::Ordering::Equal => continue,
                }
            }
            // All compared parts equal means version equals upper bound prefix,
            // which is not less than the upper bound
            false
        }
        _ => false,
    }
}

fn parse_ruby_version(v: &str) -> Vec<u64> {
    v.split('.').filter_map(|s| s.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_get_latest_version() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/gems/rails.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"name":"rails","version":"7.2.1"}"#),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry.get_latest_version("rails").await.unwrap();
        assert_eq!(version, "7.2.1");
    }

    #[tokio::test]
    async fn test_package_not_found() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/gems/nonexistent-gem-xyz.json"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let result = registry.get_latest_version("nonexistent-gem-xyz").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_registry_name() {
        let registry = RubyGemsRegistry::new();
        assert_eq!(registry.name(), "rubygems");
    }

    #[tokio::test]
    async fn test_get_latest_including_prereleases() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"number":"8.0.0.beta1","prerelease":true},
                    {"number":"7.2.1","prerelease":false},
                    {"number":"7.2.0","prerelease":false}
                ]"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version_including_prereleases("rails")
            .await
            .unwrap();
        assert_eq!(version, "8.0.0.beta1");
    }

    #[test]
    fn test_matches_ruby_constraint_pessimistic() {
        // ~> 7.1 means >= 7.1 and < 8.0
        assert!(matches_ruby_constraint("7.1.0", "~> 7.1"));
        assert!(matches_ruby_constraint("7.2.3", "~> 7.1"));
        assert!(matches_ruby_constraint("7.99.0", "~> 7.1"));
        assert!(!matches_ruby_constraint("8.0.0", "~> 7.1"));
        assert!(!matches_ruby_constraint("6.0.0", "~> 7.1"));

        // ~> 7.1.0 means >= 7.1.0 and < 7.2.0
        assert!(matches_ruby_constraint("7.1.0", "~> 7.1.0"));
        assert!(matches_ruby_constraint("7.1.5", "~> 7.1.0"));
        assert!(!matches_ruby_constraint("7.2.0", "~> 7.1.0"));
        assert!(!matches_ruby_constraint("7.0.0", "~> 7.1.0"));
    }

    #[tokio::test]
    async fn test_get_latest_version_matching_pessimistic() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"number": "8.0.4", "prerelease": false},
                    {"number": "7.2.3", "prerelease": false},
                    {"number": "7.1.5", "prerelease": false},
                    {"number": "6.1.7", "prerelease": false}
                ]"#,
            ))
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        // ~> 7.1 should match >= 7.1, < 8.0
        let version = registry
            .get_latest_version_matching("rails", "~> 7.1")
            .await
            .unwrap();
        assert_eq!(version, "7.2.3");
    }

    #[tokio::test]
    async fn test_get_latest_version_matching_no_match() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/oldgem.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"[{"number": "2.0.0", "prerelease": false}]"#),
            )
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let result = registry
            .get_latest_version_matching("oldgem", "~> 1.0")
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_matches_ruby_constraint_comparison() {
        assert!(matches_ruby_constraint("5.0.0", ">= 4.9.0"));
        assert!(matches_ruby_constraint("4.9.0", ">= 4.9.0"));
        assert!(!matches_ruby_constraint("4.8.0", ">= 4.9.0"));

        assert!(matches_ruby_constraint("1.5.3", "< 2.0.0"));
        assert!(!matches_ruby_constraint("2.0.0", "< 2.0.0"));

        assert!(matches_ruby_constraint("1.5.4", "= 1.5.4"));
        assert!(!matches_ruby_constraint("1.5.5", "= 1.5.4"));
    }

    #[tokio::test]
    async fn test_get_latest_including_prereleases_skips_yanked() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"number":"8.0.1","prerelease":false,"yanked":true},
                    {"number":"8.0.0.beta1","prerelease":true,"yanked":false},
                    {"number":"7.2.1","prerelease":false,"yanked":false}
                ]"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version_including_prereleases("rails")
            .await
            .unwrap();
        assert_eq!(
            version, "8.0.0.beta1",
            "yanked 8.0.1 must be skipped, next newest returned"
        );
    }

    #[tokio::test]
    async fn test_get_latest_version_matching_skips_yanked() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"number":"7.2.3","prerelease":false,"yanked":true},
                    {"number":"7.1.5","prerelease":false,"yanked":false},
                    {"number":"7.1.4","prerelease":false,"yanked":false}
                ]"#,
            ))
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version_matching("rails", "~> 7.1")
            .await
            .unwrap();
        assert_eq!(
            version, "7.1.5",
            "yanked 7.2.3 must be skipped even when it matches the constraint"
        );
    }

    #[tokio::test]
    async fn test_get_latest_version_matching_accepts_missing_yanked_field() {
        // Older RubyGems responses may omit the `yanked` field entirely; we
        // must treat that as "not yanked" and still return the version.
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"[{"number":"7.1.5","prerelease":false}]"#),
            )
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version_matching("rails", "~> 7.1")
            .await
            .unwrap();
        assert_eq!(version, "7.1.5");
    }

    #[tokio::test]
    async fn test_rubygems_list_versions_returns_publish_dates() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
              {"number": "7.1.0", "created_at": "2023-10-05T10:00:00Z", "yanked": false, "prerelease": false},
              {"number": "7.0.8", "created_at": "2023-11-08T10:00:00Z", "yanked": true, "prerelease": false},
              {"number": "6.0.0.rc1", "created_at": "2019-04-24T10:00:00Z", "yanked": false, "prerelease": true}
            ]"#,
            ))
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let versions = registry.list_versions("rails").await.unwrap();

        assert_eq!(versions.len(), 3);
        let rc = versions.iter().find(|v| v.version == "6.0.0.rc1").unwrap();
        assert!(
            rc.prerelease,
            "6.0.0.rc1 should be recognised as pre-release"
        );
        let stable = versions.iter().find(|v| v.version == "7.1.0").unwrap();
        assert!(!stable.prerelease);
        assert!(stable.published_at.is_some());
        let yanked_entry = versions.iter().find(|v| v.version == "7.0.8").unwrap();
        assert!(
            yanked_entry.yanked,
            "yanked flag should round-trip from API"
        );
    }
}
