use super::Registry;
use super::utils::home_dir;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::Value;
use std::io::BufRead;
use std::path::PathBuf;
use std::time::Duration;

/// Credentials for authenticating with an npm registry
#[derive(Clone)]
pub struct NpmCredentials {
    /// Bearer token for authentication
    pub token: String,
}

impl std::fmt::Debug for NpmCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NpmCredentials")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

pub struct NpmRegistry {
    client: Client,
    registry_url: String,
}

/// Abbreviated npm response (smaller, faster)
/// Uses Accept: application/vnd.npm.install-v1+json
#[derive(Debug, Deserialize)]
struct NpmAbbreviatedResponse {
    #[serde(rename = "dist-tags")]
    dist_tags: DistTags,
    /// Version keys only (we parse them dynamically to avoid large struct)
    versions: Value,
}

#[derive(Debug, Deserialize)]
struct DistTags {
    latest: Option<String>,
}

/// Read token from .npmrc files
/// .npmrc format supports registry-scoped tokens:
/// //registry.npmjs.org/:_authToken=token-value
/// //custom.registry.com/:_authToken=token-value
fn read_npmrc_token(registry_url: &str) -> Option<NpmCredentials> {
    // Extract host from registry URL
    let url = url::Url::parse(registry_url).ok()?;
    let host = url.host_str()?;
    let path = url.path();
    let registry_pattern = format!("//{}{}", host, path.trim_end_matches('/'));

    // Search paths for .npmrc
    let mut search_paths = Vec::new();

    // Check current directory first
    if let Ok(cwd) = std::env::current_dir() {
        search_paths.push(cwd.join(".npmrc"));
    }

    // Check user's home directory
    if let Some(home) = home_dir() {
        search_paths.push(home.join(".npmrc"));
    }

    // Check NPM_CONFIG_USERCONFIG environment variable
    if let Ok(config_path) = std::env::var("NPM_CONFIG_USERCONFIG") {
        search_paths.push(PathBuf::from(config_path));
    }

    for path in search_paths {
        if let Some(token) = read_token_from_npmrc(&path, &registry_pattern) {
            return Some(NpmCredentials { token });
        }
    }

    None
}

/// Parse a single .npmrc file looking for the token
fn read_token_from_npmrc(path: &PathBuf, registry_pattern: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);

    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        // Look for _authToken entries
        // Format: //registry.npmjs.org/:_authToken=token-value
        if let Some(rest) = line.strip_prefix(registry_pattern)
            && let Some(token_part) = rest.strip_prefix("/:_authToken=")
        {
            let token = token_part.trim();
            if !token.is_empty() {
                // Handle environment variable references like ${NPM_TOKEN}
                if token.starts_with("${") && token.ends_with('}') {
                    let var_name = &token[2..token.len() - 1];
                    if let Ok(resolved) = std::env::var(var_name) {
                        return Some(resolved);
                    }
                } else {
                    return Some(token.to_string());
                }
            }
        }

        // Also check for a global _authToken (no registry prefix)
        if let Some(token) = line.strip_prefix("_authToken=") {
            let token = token.trim();
            if !token.is_empty() {
                // Handle environment variable references
                if token.starts_with("${") && token.ends_with('}') {
                    let var_name = &token[2..token.len() - 1];
                    if let Ok(resolved) = std::env::var(var_name) {
                        return Some(resolved);
                    }
                } else {
                    return Some(token.to_string());
                }
            }
        }
    }

    None
}

impl NpmRegistry {
    pub fn new() -> Self {
        Self::with_registry_url("https://registry.npmjs.org".to_string())
    }

    pub fn with_registry_url(registry_url: String) -> Self {
        Self::with_registry_url_and_credentials(registry_url, None)
    }

    pub fn with_registry_url_and_credentials(
        registry_url: String,
        credentials: Option<NpmCredentials>,
    ) -> Self {
        let mut headers = HeaderMap::new();

        // Add Bearer token if credentials are provided
        if let Some(ref creds) = credentials
            && let Ok(header_value) = HeaderValue::from_str(&format!("Bearer {}", creds.token))
        {
            headers.insert(AUTHORIZATION, header_value);
        }

        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            registry_url,
        }
    }

    /// Detect custom registry URL from environment
    pub fn detect_registry_url() -> Option<String> {
        std::env::var("NPM_REGISTRY").ok().filter(|s| !s.is_empty())
    }

    /// Detect credentials from environment variables or .npmrc
    pub fn detect_credentials(registry_url: &str) -> Option<NpmCredentials> {
        // Try NPM_TOKEN environment variable first
        if let Ok(token) = std::env::var("NPM_TOKEN")
            && !token.is_empty()
        {
            return Some(NpmCredentials { token });
        }

        // Try NODE_AUTH_TOKEN environment variable (used by GitHub Actions)
        if let Ok(token) = std::env::var("NODE_AUTH_TOKEN")
            && !token.is_empty()
        {
            return Some(NpmCredentials { token });
        }

        // Try reading from .npmrc
        read_npmrc_token(registry_url)
    }

    /// Fetch abbreviated package metadata from npm
    /// Uses the install-v1 format which is smaller and faster
    async fn fetch_package(&self, package: &str) -> Result<NpmAbbreviatedResponse> {
        let url = format!("{}/{}", self.registry_url, package);

        // Use abbreviated metadata format (much smaller for large packages like react)
        let response = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.npm.install-v1+json")
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch package '{}': HTTP {}",
                package,
                response.status()
            ));
        }

        response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse npm response for '{}': {}", package, e))
    }

    /// Get all stable (non-prerelease) versions sorted descending
    /// Note: abbreviated metadata doesn't include deprecated info, but dist-tags.latest
    /// is authoritative for the recommended version
    fn get_stable_versions(data: &NpmAbbreviatedResponse) -> Vec<(semver::Version, String)> {
        let versions_obj = match data.versions.as_object() {
            Some(obj) => obj,
            None => return Vec::new(),
        };

        let mut versions: Vec<_> = versions_obj
            .keys()
            .filter_map(|ver_str| {
                semver::Version::parse(ver_str).ok().and_then(|v| {
                    // Skip pre-release versions
                    if v.pre.is_empty() {
                        Some((v, ver_str.clone()))
                    } else {
                        None
                    }
                })
            })
            .collect();

        versions.sort_by(|a, b| b.0.cmp(&a.0));
        versions
    }
}

impl Default for NpmRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for NpmRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let data = self.fetch_package(package).await?;

        // Use the 'latest' dist-tag (this is the authoritative answer from npm)
        if let Some(latest) = &data.dist_tags.latest
            && let Ok(v) = semver::Version::parse(latest)
            && v.pre.is_empty()
        {
            return Ok(latest.clone());
        }

        // Fall back to finding the latest stable version from the versions list
        let versions = Self::get_stable_versions(&data);
        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No stable versions found for package '{}'", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let data = self.fetch_package(package).await?;
        let versions = Self::get_stable_versions(&data);

        // Parse npm-style version requirements (^1.0.0, ~2.0.0, >=1.0.0 <2.0.0, etc.)
        let req = semver::VersionReq::parse(constraints).map_err(|e| {
            anyhow!(
                "Failed to parse version constraints '{}': {}",
                constraints,
                e
            )
        })?;

        // Find the highest version that matches
        for (version, version_str) in versions {
            if req.matches(&version) {
                return Ok(version_str);
            }
        }

        Err(anyhow!(
            "No version of '{}' matches constraints '{}'",
            package,
            constraints
        ))
    }

    fn name(&self) -> &'static str {
        "npm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_get_stable_versions() {
        // Create a mock response with versions
        let json = serde_json::json!({
            "dist-tags": {
                "latest": "2.0.0"
            },
            "versions": {
                "1.0.0": {},
                "1.1.0": {},
                "2.0.0": {},
                "2.1.0-beta.1": {},
                "2.1.0-alpha.1": {}
            }
        });

        let response: NpmAbbreviatedResponse = serde_json::from_value(json).unwrap();
        let versions = NpmRegistry::get_stable_versions(&response);

        // Should only include stable versions, sorted descending
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].1, "2.0.0");
        assert_eq!(versions[1].1, "1.1.0");
        assert_eq!(versions[2].1, "1.0.0");
    }

    #[test]
    fn test_get_stable_versions_filters_prereleases() {
        let json = serde_json::json!({
            "dist-tags": {
                "latest": "1.0.0"
            },
            "versions": {
                "1.0.0": {},
                "2.0.0-rc.1": {},
                "2.0.0-beta.5": {},
                "2.0.0-alpha.1": {}
            }
        });

        let response: NpmAbbreviatedResponse = serde_json::from_value(json).unwrap();
        let versions = NpmRegistry::get_stable_versions(&response);

        // Only stable version should be included
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].1, "1.0.0");
    }

    #[test]
    fn test_registry_name() {
        let registry = NpmRegistry::new();
        assert_eq!(registry.name(), "npm");
    }

    #[test]
    fn test_with_registry_url() {
        let registry = NpmRegistry::with_registry_url("https://custom.registry.com".to_string());
        assert_eq!(registry.registry_url, "https://custom.registry.com");
    }

    #[test]
    fn test_detect_credentials_from_env() {
        // SAFETY: Test runs in isolation
        unsafe {
            std::env::set_var("NPM_TOKEN", "test-token-123");
        }

        let creds = NpmRegistry::detect_credentials("https://registry.npmjs.org");
        assert!(creds.is_some());
        assert_eq!(creds.unwrap().token, "test-token-123");

        // SAFETY: Test runs in isolation
        unsafe {
            std::env::remove_var("NPM_TOKEN");
        }
    }

    #[test]
    fn test_read_token_from_npmrc_global() {
        // Create a temp .npmrc file
        let mut npmrc_file = NamedTempFile::new().unwrap();
        writeln!(npmrc_file, "_authToken=npmrc-token-value").unwrap();

        // Test reading directly from file (doesn't use env vars)
        let path = npmrc_file.path().to_path_buf();
        let token = read_token_from_npmrc(&path, "//registry.npmjs.org");
        assert!(token.is_some());
        assert_eq!(token.unwrap(), "npmrc-token-value");
    }

    #[test]
    fn test_read_token_from_npmrc_scoped() {
        // Create a temp .npmrc file with scoped registry token
        let mut npmrc_file = NamedTempFile::new().unwrap();
        writeln!(
            npmrc_file,
            "//registry.npmjs.org/:_authToken=scoped-token-value"
        )
        .unwrap();

        // Test reading directly from file (doesn't use env vars)
        let path = npmrc_file.path().to_path_buf();
        let token = read_token_from_npmrc(&path, "//registry.npmjs.org");
        assert!(token.is_some());
        assert_eq!(token.unwrap(), "scoped-token-value");
    }

    #[test]
    fn test_registry_with_credentials() {
        let creds = NpmCredentials {
            token: "test-token".to_string(),
        };
        // Just verify that the registry can be created with credentials
        let _registry = NpmRegistry::with_registry_url_and_credentials(
            "https://registry.npmjs.org".to_string(),
            Some(creds),
        );
    }
}
