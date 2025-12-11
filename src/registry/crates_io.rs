use super::Registry;
use super::utils::home_dir;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Client, Response};
use serde::Deserialize;
use std::io::BufRead;
use std::path::PathBuf;
use std::time::Duration;

/// Maximum number of retry attempts for failed HTTP requests
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (100ms, 200ms, 400ms)
const BASE_DELAY_MS: u64 = 100;

/// Credentials for authenticating with a Cargo registry
#[derive(Clone)]
pub struct CargoCredentials {
    /// Bearer token for authentication
    pub token: String,
}

impl std::fmt::Debug for CargoCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CargoCredentials")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

pub struct CratesIoRegistry {
    client: Client,
    registry_url: String,
}

#[derive(Debug, Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    krate: CrateInfo,
    versions: Vec<VersionInfo>,
}

#[derive(Debug, Deserialize)]
struct CrateInfo {
    max_stable_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VersionInfo {
    num: String,
    yanked: bool,
}

/// Read token from ~/.cargo/credentials.toml
fn read_cargo_credentials(registry_name: &str) -> Option<CargoCredentials> {
    let credentials_path = home_dir()?.join(".cargo").join("credentials.toml");

    if !credentials_path.exists() {
        // Try legacy credentials file without .toml extension
        let legacy_path = home_dir()?.join(".cargo").join("credentials");
        if legacy_path.exists() {
            return read_token_from_credentials(&legacy_path, registry_name);
        }
        return None;
    }

    read_token_from_credentials(&credentials_path, registry_name)
}

/// Parse a credentials.toml file looking for the token
fn read_token_from_credentials(path: &PathBuf, registry_name: &str) -> Option<CargoCredentials> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);

    let mut in_target_section = false;
    let section_header = format!("[registries.{}]", registry_name);

    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Check for section headers
        if line.starts_with('[') {
            // Check for [registry] section (for crates-io default)
            if line == "[registry]" && registry_name == "crates-io" {
                in_target_section = true;
                continue;
            }
            // Check for [registries.name] section
            if line == section_header {
                in_target_section = true;
                continue;
            }
            // Other section, reset
            in_target_section = false;
            continue;
        }

        // Look for token in current section
        if in_target_section && let Some(token) = line.strip_prefix("token") {
            let token = token.trim().trim_start_matches('=').trim();
            // Remove quotes
            let token = token.trim_matches('"').trim_matches('\'');
            if !token.is_empty() {
                return Some(CargoCredentials {
                    token: token.to_string(),
                });
            }
        }
    }

    None
}

impl CratesIoRegistry {
    pub fn new() -> Self {
        Self::with_registry_url("https://crates.io/api/v1/crates".to_string())
    }

    pub fn with_registry_url(registry_url: String) -> Self {
        Self::with_registry_url_and_credentials(registry_url, None)
    }

    pub fn with_registry_url_and_credentials(
        registry_url: String,
        credentials: Option<CargoCredentials>,
    ) -> Self {
        let mut headers = HeaderMap::new();

        // Add Bearer token if credentials are provided
        if let Some(ref creds) = credentials
            && let Ok(header_value) = HeaderValue::from_str(&creds.token)
        {
            headers.insert(AUTHORIZATION, header_value);
        }

        let client = Client::builder()
            .gzip(true)
            // crates.io requires a descriptive User-Agent
            .user_agent(concat!(
                "upd/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/rvben/upd)"
            ))
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
        std::env::var("CARGO_REGISTRIES_CRATES_IO_INDEX")
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Detect credentials from environment variables or credentials.toml
    pub fn detect_credentials(registry_name: &str) -> Option<CargoCredentials> {
        // Try CARGO_REGISTRY_TOKEN first (for default registry)
        if registry_name == "crates-io"
            && let Ok(token) = std::env::var("CARGO_REGISTRY_TOKEN")
            && !token.is_empty()
        {
            return Some(CargoCredentials { token });
        }

        // Try CARGO_REGISTRIES_<NAME>_TOKEN
        let env_var = format!(
            "CARGO_REGISTRIES_{}_TOKEN",
            registry_name.to_uppercase().replace('-', "_")
        );
        if let Ok(token) = std::env::var(&env_var)
            && !token.is_empty()
        {
            return Some(CargoCredentials { token });
        }

        // Try reading from credentials.toml
        read_cargo_credentials(registry_name)
    }

    /// Execute a GET request with retry
    async fn get_with_retry(&self, url: &str) -> Result<Response, reqwest::Error> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            match self.client.get(url).send().await {
                Ok(response) => {
                    if response.status().is_client_error() || response.status().is_success() {
                        return Ok(response);
                    }
                    if response.status().is_server_error() && attempt < MAX_RETRIES - 1 {
                        let delay = Duration::from_millis(BASE_DELAY_MS * (1 << attempt));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Ok(response);
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < MAX_RETRIES - 1 {
                        let delay = Duration::from_millis(BASE_DELAY_MS * (1 << attempt));
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_error.unwrap())
    }

    async fn fetch_crate(&self, name: &str) -> Result<CratesResponse> {
        let url = format!("{}/{}", self.registry_url, name);
        let response = self.get_with_retry(&url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch crate '{}': HTTP {}",
                name,
                response.status()
            ));
        }

        Ok(response.json().await?)
    }

    fn get_sorted_versions(
        data: &CratesResponse,
        include_prereleases: bool,
    ) -> Vec<(semver::Version, String)> {
        let mut versions: Vec<_> = data
            .versions
            .iter()
            .filter(|v| !v.yanked)
            .filter_map(|v| {
                semver::Version::parse(&v.num).ok().and_then(|parsed| {
                    if include_prereleases || parsed.pre.is_empty() {
                        Some((parsed, v.num.clone()))
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

impl Default for CratesIoRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for CratesIoRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let data = self.fetch_crate(package).await?;

        // Fast path: use max_stable_version if available
        if let Some(ref max_stable) = data.krate.max_stable_version {
            return Ok(max_stable.clone());
        }

        // Fallback: find latest stable from versions list
        let versions = Self::get_sorted_versions(&data, false);
        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No stable versions found for crate '{}'", package))
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let data = self.fetch_crate(package).await?;
        let versions = Self::get_sorted_versions(&data, true);

        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No versions found for crate '{}'", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let data = self.fetch_crate(package).await?;
        let versions = Self::get_sorted_versions(&data, false);

        let req = semver::VersionReq::parse(constraints).map_err(|e| {
            anyhow!(
                "Failed to parse version constraints '{}': {}",
                constraints,
                e
            )
        })?;

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
        "crates.io"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_version_sorting() {
        // Test that we properly filter and sort versions
        let data = CratesResponse {
            krate: CrateInfo {
                max_stable_version: None,
            },
            versions: vec![
                VersionInfo {
                    num: "1.0.0".to_string(),
                    yanked: false,
                },
                VersionInfo {
                    num: "2.0.0".to_string(),
                    yanked: false,
                },
                VersionInfo {
                    num: "1.5.0".to_string(),
                    yanked: true,
                },
                VersionInfo {
                    num: "3.0.0-alpha.1".to_string(),
                    yanked: false,
                },
            ],
        };

        let stable = CratesIoRegistry::get_sorted_versions(&data, false);
        assert_eq!(stable.len(), 2);
        assert_eq!(stable[0].1, "2.0.0");
        assert_eq!(stable[1].1, "1.0.0");

        let all = CratesIoRegistry::get_sorted_versions(&data, true);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].1, "3.0.0-alpha.1");
    }

    #[test]
    fn test_detect_credentials_from_env() {
        // SAFETY: Test runs in isolation
        unsafe {
            std::env::set_var("CARGO_REGISTRY_TOKEN", "test-cargo-token");
        }

        let creds = CratesIoRegistry::detect_credentials("crates-io");
        assert!(creds.is_some());
        assert_eq!(creds.unwrap().token, "test-cargo-token");

        // SAFETY: Test runs in isolation
        unsafe {
            std::env::remove_var("CARGO_REGISTRY_TOKEN");
        }
    }

    #[test]
    fn test_read_token_from_credentials_registry_section() {
        // Create a temp credentials file
        let mut creds_file = NamedTempFile::new().unwrap();
        writeln!(creds_file, "[registry]").unwrap();
        writeln!(creds_file, "token = \"test-token-123\"").unwrap();

        // Test reading directly from file
        let path = creds_file.path().to_path_buf();
        let creds = read_token_from_credentials(&path, "crates-io");
        assert!(creds.is_some());
        assert_eq!(creds.unwrap().token, "test-token-123");
    }

    #[test]
    fn test_read_token_from_credentials_named_registry() {
        // Create a temp credentials file with named registry
        let mut creds_file = NamedTempFile::new().unwrap();
        writeln!(creds_file, "[registries.my-private-registry]").unwrap();
        writeln!(creds_file, "token = \"private-token-456\"").unwrap();

        let path = creds_file.path().to_path_buf();
        let creds = read_token_from_credentials(&path, "my-private-registry");
        assert!(creds.is_some());
        assert_eq!(creds.unwrap().token, "private-token-456");
    }

    #[test]
    fn test_registry_with_credentials() {
        let creds = CargoCredentials {
            token: "test-token".to_string(),
        };
        // Just verify that the registry can be created with credentials
        let _registry = CratesIoRegistry::with_registry_url_and_credentials(
            "https://crates.io/api/v1/crates".to_string(),
            Some(creds),
        );
    }
}
