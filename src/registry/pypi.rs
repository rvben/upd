use super::Registry;
#[cfg(test)]
use super::utils::read_netrc_credentials_from_path;
use super::utils::{base64_encode, read_netrc_credentials};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use pep440_rs::{Version, VersionSpecifiers};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Client, Response};
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

/// Maximum number of retry attempts for failed HTTP requests
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (100ms, 200ms, 400ms)
const BASE_DELAY_MS: u64 = 100;

/// Credentials for authenticating with a PyPI registry
#[derive(Clone)]
pub struct PyPiCredentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for PyPiCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyPiCredentials")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

pub struct PyPiRegistry {
    client: Client,
    index_url: String,
}

#[derive(Debug, Deserialize)]
struct PyPiResponse {
    releases: HashMap<String, Vec<ReleaseFile>>,
}

#[derive(Debug, Deserialize)]
struct ReleaseFile {
    yanked: Option<bool>,
}

impl PyPiRegistry {
    pub fn new() -> Self {
        Self::with_index_url("https://pypi.org/pypi".to_string())
    }

    pub fn with_index_url(index_url: String) -> Self {
        Self::with_index_url_and_credentials(index_url, None)
    }

    pub fn with_index_url_and_credentials(
        index_url: String,
        credentials: Option<PyPiCredentials>,
    ) -> Self {
        let mut headers = HeaderMap::new();

        // Add Basic Auth header if credentials are provided
        if let Some(ref creds) = credentials {
            let auth = format!("{}:{}", creds.username, creds.password);
            let encoded = base64_encode(&auth);
            if let Ok(header_value) = HeaderValue::from_str(&format!("Basic {}", encoded)) {
                headers.insert(AUTHORIZATION, header_value);
            }
        }

        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()
            .expect("Failed to create HTTP client");

        Self { client, index_url }
    }

    /// Detect custom index URL from environment or config
    pub fn detect_index_url() -> Option<String> {
        // Check environment variables in order of precedence
        for var in ["UV_INDEX_URL", "PIP_INDEX_URL", "PYTHON_INDEX_URL"] {
            if let Ok(url) = std::env::var(var)
                && !url.is_empty()
            {
                return Some(url);
            }
        }
        None
    }

    /// Create a registry from a URL that may contain embedded credentials
    /// Supports URLs like: https://user:pass@private.pypi.com/simple
    pub fn from_url(url: &str) -> Self {
        if let Ok(parsed) = url::Url::parse(url) {
            let username = parsed.username();
            let password = parsed.password().unwrap_or("");

            if !username.is_empty() {
                // URL has embedded credentials - extract them and create clean URL
                let mut clean_url = parsed.clone();
                clean_url.set_username("").ok();
                clean_url.set_password(None).ok();

                let credentials = PyPiCredentials {
                    username: username.to_string(),
                    password: password.to_string(),
                };

                return Self::with_index_url_and_credentials(
                    clean_url.to_string().trim_end_matches('/').to_string(),
                    Some(credentials),
                );
            }

            // No embedded credentials - try to detect from netrc
            let host = parsed.host_str().unwrap_or("");
            let credentials = read_netrc_credentials(host).map(|c| PyPiCredentials {
                username: c.login,
                password: c.password,
            });

            Self::with_index_url_and_credentials(url.trim_end_matches('/').to_string(), credentials)
        } else {
            // Invalid URL - create without credentials
            Self::with_index_url(url.trim_end_matches('/').to_string())
        }
    }

    /// Get the index URL this registry is configured for
    pub fn index_url(&self) -> &str {
        &self.index_url
    }

    /// Detect credentials from environment variables or netrc
    pub fn detect_credentials(index_url: &str) -> Option<PyPiCredentials> {
        // Try environment variables first (uv-style)
        if let (Ok(username), Ok(password)) = (
            std::env::var("UV_INDEX_USERNAME"),
            std::env::var("UV_INDEX_PASSWORD"),
        ) && !username.is_empty()
            && !password.is_empty()
        {
            return Some(PyPiCredentials { username, password });
        }

        // Try PIP-style environment variables
        if let (Ok(username), Ok(password)) = (
            std::env::var("PIP_INDEX_USERNAME"),
            std::env::var("PIP_INDEX_PASSWORD"),
        ) && !username.is_empty()
            && !password.is_empty()
        {
            return Some(PyPiCredentials { username, password });
        }

        // Extract host from index URL and try netrc
        if let Ok(url) = url::Url::parse(index_url)
            && let Some(host) = url.host_str()
            && let Some(creds) = read_netrc_credentials(host)
        {
            return Some(PyPiCredentials {
                username: creds.login,
                password: creds.password,
            });
        }

        None
    }

    /// Execute a GET request with retry and authentication
    async fn get_with_retry(&self, url: &str) -> Result<Response, reqwest::Error> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            match self.client.get(url).send().await {
                Ok(response) => {
                    // Don't retry client errors (4xx) - they won't succeed on retry
                    if response.status().is_client_error() || response.status().is_success() {
                        return Ok(response);
                    }

                    // Retry server errors (5xx)
                    if response.status().is_server_error() && attempt < MAX_RETRIES - 1 {
                        let delay = Duration::from_millis(BASE_DELAY_MS * (1 << attempt));
                        tokio::time::sleep(delay).await;
                        continue;
                    }

                    return Ok(response);
                }
                Err(e) => {
                    last_error = Some(e);

                    // Don't retry on the last attempt
                    if attempt < MAX_RETRIES - 1 {
                        let delay = Duration::from_millis(BASE_DELAY_MS * (1 << attempt));
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_error.unwrap())
    }

    fn is_stable_version(version_str: &str) -> bool {
        if let Ok(version) = version_str.parse::<Version>() {
            // Exclude pre-releases (alpha, beta, rc, dev)
            !version.is_pre() && !version.is_dev()
        } else {
            false
        }
    }

    /// Fetch all available stable versions for a package
    async fn fetch_versions(&self, package: &str) -> Result<Vec<(Version, String)>> {
        self.fetch_versions_internal(package, false).await
    }

    /// Fetch all versions including pre-releases
    async fn fetch_all_versions(&self, package: &str) -> Result<Vec<(Version, String)>> {
        self.fetch_versions_internal(package, true).await
    }

    /// Internal method to fetch versions with optional pre-release inclusion
    async fn fetch_versions_internal(
        &self,
        package: &str,
        include_prereleases: bool,
    ) -> Result<Vec<(Version, String)>> {
        let normalized = package.to_lowercase().replace('_', "-");
        let url = format!("{}/{}/json", self.index_url, normalized);

        let response = self.get_with_retry(&url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch package '{}': HTTP {}",
                package,
                response.status()
            ));
        }

        let data: PyPiResponse = response.json().await?;

        let mut versions: Vec<(Version, String)> = data
            .releases
            .iter()
            .filter(|(ver_str, files)| {
                let is_yanked = files.iter().all(|f| f.yanked.unwrap_or(false));
                if is_yanked {
                    return false;
                }
                include_prereleases || Self::is_stable_version(ver_str)
            })
            .filter_map(|(ver_str, _)| {
                ver_str
                    .parse::<Version>()
                    .ok()
                    .map(|v| (v, ver_str.clone()))
            })
            .collect();

        versions.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(versions)
    }
}

impl Default for PyPiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for PyPiRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let versions = self.fetch_versions(package).await?;

        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No stable versions found for package '{}'", package))
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let versions = self.fetch_all_versions(package).await?;

        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No versions found for package '{}'", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let versions = self.fetch_versions(package).await?;

        // Parse constraints (e.g., ">=2.8.0,<9" or ">=1.0.0")
        let specifiers = VersionSpecifiers::from_str(constraints).map_err(|e| {
            anyhow!(
                "Failed to parse version constraints '{}': {}",
                constraints,
                e
            )
        })?;

        // Find the highest version that matches all constraints
        for (version, version_str) in versions {
            if specifiers.contains(&version) {
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
        "pypi"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_stable_version_detection() {
        assert!(PyPiRegistry::is_stable_version("1.0.0"));
        assert!(PyPiRegistry::is_stable_version("2.31.0"));
        assert!(!PyPiRegistry::is_stable_version("1.0.0a1"));
        assert!(!PyPiRegistry::is_stable_version("1.0.0b2"));
        assert!(!PyPiRegistry::is_stable_version("1.0.0rc1"));
        assert!(!PyPiRegistry::is_stable_version("1.0.0.dev1"));
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode("hello"), "aGVsbG8=");
        assert_eq!(base64_encode("user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_encode("a"), "YQ==");
        assert_eq!(base64_encode("ab"), "YWI=");
        assert_eq!(base64_encode("abc"), "YWJj");
        assert_eq!(base64_encode(""), "");
    }

    #[test]
    fn test_read_netrc_credentials() {
        // Create a temp netrc file
        let mut netrc_file = NamedTempFile::new().unwrap();
        writeln!(
            netrc_file,
            "machine pypi.example.com login testuser password testpass"
        )
        .unwrap();
        writeln!(
            netrc_file,
            "machine other.example.com login other password otherpass"
        )
        .unwrap();

        let netrc_path = netrc_file.path().to_path_buf();

        let creds = read_netrc_credentials_from_path(&netrc_path, "pypi.example.com");
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.login, "testuser");
        assert_eq!(creds.password, "testpass");

        // Test non-existent host
        let creds = read_netrc_credentials_from_path(&netrc_path, "nonexistent.example.com");
        assert!(creds.is_none());
    }

    #[test]
    fn test_read_netrc_multiline() {
        // Create a temp netrc file with multiline format
        let mut netrc_file = NamedTempFile::new().unwrap();
        writeln!(netrc_file, "machine pypi.example.com").unwrap();
        writeln!(netrc_file, "  login testuser").unwrap();
        writeln!(netrc_file, "  password testpass").unwrap();

        let netrc_path = netrc_file.path().to_path_buf();

        let creds = read_netrc_credentials_from_path(&netrc_path, "pypi.example.com");
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.login, "testuser");
        assert_eq!(creds.password, "testpass");
    }

    #[test]
    fn test_detect_credentials_from_env() {
        // Set UV_INDEX_* credentials
        // SAFETY: Test runs in isolation
        unsafe {
            std::env::set_var("UV_INDEX_USERNAME", "uvuser");
            std::env::set_var("UV_INDEX_PASSWORD", "uvpass");
        }

        let creds = PyPiRegistry::detect_credentials("https://pypi.example.com/simple");
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.username, "uvuser");
        assert_eq!(creds.password, "uvpass");

        // SAFETY: Test runs in isolation
        unsafe {
            std::env::remove_var("UV_INDEX_USERNAME");
            std::env::remove_var("UV_INDEX_PASSWORD");
        }
    }

    #[test]
    fn test_registry_with_credentials() {
        let creds = PyPiCredentials {
            username: "testuser".to_string(),
            password: "testpass".to_string(),
        };
        // Just verify that the registry can be created with credentials
        let _registry = PyPiRegistry::with_index_url_and_credentials(
            "https://pypi.example.com/simple".to_string(),
            Some(creds),
        );
        // The credentials are used to set default headers in the client,
        // we can't easily verify them without making a request
    }

    #[test]
    fn test_from_url_with_embedded_credentials() {
        // URL with embedded credentials
        let registry = PyPiRegistry::from_url("https://user:pass@pypi.example.com/simple");
        // The URL should be cleaned (credentials removed from URL itself)
        assert_eq!(registry.index_url(), "https://pypi.example.com/simple");
    }

    #[test]
    fn test_from_url_without_credentials() {
        // URL without credentials
        let registry = PyPiRegistry::from_url("https://pypi.example.com/simple");
        assert_eq!(registry.index_url(), "https://pypi.example.com/simple");
    }

    #[test]
    fn test_from_url_strips_trailing_slash() {
        let registry = PyPiRegistry::from_url("https://pypi.example.com/simple/");
        assert_eq!(registry.index_url(), "https://pypi.example.com/simple");
    }
}
