#[cfg(test)]
use super::utils::read_netrc_credentials_from_path;
use super::utils::{base64_encode, read_netrc_credentials, read_pip_config};
use super::{Registry, http_error_message};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use pep440_rs::{Version, VersionSpecifiers};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Client, Response};
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
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

/// Authentication method for PyPI registries
#[derive(Clone)]
pub enum PyPiAuth {
    /// Basic authentication with username and password
    Basic(PyPiCredentials),
    /// Bearer token authentication (GitLab, Azure, JFrog, etc.)
    Bearer(String),
}

pub struct PyPiRegistry {
    client: Client,
    index_url: String,
}

#[derive(Debug, Deserialize)]
struct PyPiResponse {
    releases: HashMap<String, Vec<ReleaseFile>>,
}

/// PEP 691 JSON Simple API response format
#[derive(Debug, Clone, Deserialize)]
struct SimpleApiResponse {
    files: Vec<SimpleApiFile>,
}

#[derive(Debug, Clone, Deserialize)]
struct SimpleApiFile {
    filename: String,
    #[serde(default)]
    yanked: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ReleaseFile {
    yanked: Option<bool>,
}

impl PyPiRegistry {
    pub fn new() -> Self {
        Self::with_index_url("https://pypi.org".to_string())
    }

    pub fn with_index_url(index_url: String) -> Self {
        Self::with_index_url_and_credentials(index_url, None)
    }

    /// Convert a Simple API URL to JSON API URL format
    /// Strips "/simple" suffix since JSON API uses base path + /{package}/json
    /// e.g., "https://example.com/repository/pypi/simple" -> "https://example.com/repository/pypi"
    fn normalize_index_url(url: &str) -> String {
        let trimmed = url.trim_end_matches('/');
        if let Some(stripped) = trimmed.strip_suffix("/simple") {
            stripped.to_string()
        } else {
            trimmed.to_string()
        }
    }

    pub fn with_index_url_and_credentials(
        index_url: String,
        credentials: Option<PyPiCredentials>,
    ) -> Self {
        let auth = credentials.map(PyPiAuth::Basic);
        Self::with_index_url_and_auth(index_url, auth)
    }

    /// Create a registry with a bearer token for authentication
    pub fn with_index_url_and_bearer_token(index_url: String, token: String) -> Self {
        Self::with_index_url_and_auth(index_url, Some(PyPiAuth::Bearer(token)))
    }

    /// Create a registry with any authentication method
    pub fn with_index_url_and_auth(index_url: String, auth: Option<PyPiAuth>) -> Self {
        let mut headers = HeaderMap::new();

        // Add authentication header if provided
        if let Some(ref auth_method) = auth {
            match auth_method {
                PyPiAuth::Basic(creds) => {
                    let auth = format!("{}:{}", creds.username, creds.password);
                    let encoded = base64_encode(&auth);
                    if let Ok(header_value) = HeaderValue::from_str(&format!("Basic {}", encoded)) {
                        headers.insert(AUTHORIZATION, header_value);
                    }
                }
                PyPiAuth::Bearer(token) => {
                    if let Ok(header_value) = HeaderValue::from_str(&format!("Bearer {}", token)) {
                        headers.insert(AUTHORIZATION, header_value);
                    }
                }
            }
        }

        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()
            .expect("Failed to create HTTP client. This usually indicates a TLS/SSL configuration issue on your system.");

        Self { client, index_url }
    }

    /// Detect custom index URL from environment or config
    /// Automatically converts Simple API URLs to JSON API format
    /// Checks (in order): env vars, pip.conf/pip.ini files
    pub fn detect_index_url() -> Option<String> {
        // Check environment variables in order of precedence
        for var in ["UV_INDEX_URL", "PIP_INDEX_URL", "PYTHON_INDEX_URL"] {
            if let Ok(url) = std::env::var(var)
                && !url.is_empty()
            {
                return Some(Self::normalize_index_url(&url));
            }
        }

        // Check pip.conf/pip.ini files
        let pip_config = read_pip_config();
        if let Some(url) = pip_config.index_url {
            return Some(Self::normalize_index_url(&url));
        }

        None
    }

    /// Create a registry from a URL that may contain embedded credentials
    /// Supports URLs like: <https://user:pass@private.pypi.com/simple>
    /// Also detects bearer tokens from environment variables
    /// Automatically converts Simple API URLs to JSON API format
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

                let normalized = Self::normalize_index_url(clean_url.as_str());
                return Self::with_index_url_and_credentials(normalized, Some(credentials));
            }

            // No embedded credentials - try to detect auth (bearer token or basic auth)
            let normalized = Self::normalize_index_url(url);
            let auth = Self::detect_auth(&normalized);
            Self::with_index_url_and_auth(normalized, auth)
        } else {
            // Invalid URL - create without credentials
            Self::with_index_url(Self::normalize_index_url(url))
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

    /// Detect bearer token from environment variables
    /// Supports common environment variable names used by various tools:
    /// - UV_INDEX_TOKEN (uv)
    /// - PIP_INDEX_TOKEN (pip-like)
    /// - PYPI_TOKEN (generic)
    /// - POETRY_HTTP_BASIC_PYPI_PASSWORD with empty username (Poetry)
    pub fn detect_bearer_token() -> Option<String> {
        // Check common token environment variables
        for var in [
            "UV_INDEX_TOKEN",
            "PIP_INDEX_TOKEN",
            "PYPI_TOKEN",
            "POETRY_PYPI_TOKEN_PYPI",
        ] {
            if let Ok(token) = std::env::var(var)
                && !token.is_empty()
            {
                return Some(token);
            }
        }

        // Poetry uses POETRY_HTTP_BASIC_<NAME>_PASSWORD with empty username as token
        if let Ok(token) = std::env::var("POETRY_HTTP_BASIC_PYPI_PASSWORD")
            && !token.is_empty()
            && std::env::var("POETRY_HTTP_BASIC_PYPI_USERNAME")
                .map(|u| u.is_empty() || u == "__token__")
                .unwrap_or(true)
        {
            return Some(token);
        }

        None
    }

    /// Detect any form of authentication (Bearer token or Basic auth)
    /// Bearer tokens take precedence over Basic auth
    pub fn detect_auth(index_url: &str) -> Option<PyPiAuth> {
        // Check for bearer token first
        if let Some(token) = Self::detect_bearer_token() {
            return Some(PyPiAuth::Bearer(token));
        }

        // Fall back to basic auth
        Self::detect_credentials(index_url).map(PyPiAuth::Basic)
    }

    /// Execute a GET request with retry and authentication
    async fn get_with_retry(&self, url: &str) -> Result<Response, reqwest::Error> {
        self.get_with_retry_and_headers(url, None).await
    }

    /// Execute a GET request with retry, authentication, and custom headers
    async fn get_with_retry_and_headers(
        &self,
        url: &str,
        headers: Option<HeaderMap>,
    ) -> Result<Response, reqwest::Error> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            let mut request = self.client.get(url);
            if let Some(ref h) = headers {
                request = request.headers(h.clone());
            }

            match request.send().await {
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
    /// Tries Simple API with PEP 691 content negotiation first, falls back to legacy JSON API
    async fn fetch_versions_internal(
        &self,
        package: &str,
        include_prereleases: bool,
    ) -> Result<Vec<(Version, String)>> {
        let normalized = package.to_lowercase().replace('_', "-");

        // Try Simple API with PEP 691 content negotiation
        // Request JSON format, but accept HTML as fallback
        let simple_url = format!("{}/simple/{}/", self.index_url, normalized);

        let mut headers = HeaderMap::new();
        // PEP 691: Request JSON format with HTML fallback
        if let Ok(accept) = HeaderValue::from_str(
            "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html;q=0.9, text/html;q=0.8",
        ) {
            headers.insert(reqwest::header::ACCEPT, accept);
        }

        let simple_response = self
            .get_with_retry_and_headers(&simple_url, Some(headers))
            .await?;

        if simple_response.status().is_success() {
            // Check Content-Type to determine response format
            let content_type = simple_response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            if content_type.contains("application/vnd.pypi.simple") && content_type.contains("json")
            {
                // PEP 691 JSON format
                let data: SimpleApiResponse = simple_response.json().await?;
                return self.parse_simple_api_json_response(data, package, include_prereleases);
            } else {
                // HTML format (standard Simple API or PEP 691 HTML)
                let html = simple_response.text().await?;
                return self.parse_simple_api_response(&html, package, include_prereleases);
            }
        }

        // Simple API failed - try legacy JSON API (PyPI.org style)
        let json_url = format!("{}/pypi/{}/json", self.index_url, normalized);
        let response = self.get_with_retry(&json_url).await?;

        if response.status().is_success() {
            let data: PyPiResponse = response.json().await?;
            return self.parse_json_response(data, include_prereleases);
        }

        Err(anyhow!(http_error_message(
            response.status(),
            "Package",
            package,
            Some(
                "For private PyPI, configure credentials in ~/.netrc or use UV_INDEX_URL with credentials."
            )
        )))
    }

    /// Parse JSON API response from PyPI
    fn parse_json_response(
        &self,
        data: PyPiResponse,
        include_prereleases: bool,
    ) -> Result<Vec<(Version, String)>> {
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

    /// Parse PEP 691 JSON Simple API response
    /// Extracts versions from file list, skipping yanked packages
    fn parse_simple_api_json_response(
        &self,
        data: SimpleApiResponse,
        package: &str,
        include_prereleases: bool,
    ) -> Result<Vec<(Version, String)>> {
        let mut versions: Vec<(Version, String)> = Vec::new();
        let normalized = package.to_lowercase().replace('_', "-");

        for file in &data.files {
            // Skip yanked packages
            if file.yanked.unwrap_or(false) {
                continue;
            }

            let Some(version_str) =
                Self::extract_version_from_filename(&file.filename, &normalized)
            else {
                continue;
            };

            if !include_prereleases && !Self::is_stable_version(&version_str) {
                continue;
            }

            let Ok(version) = version_str.parse::<Version>() else {
                continue;
            };

            // Avoid duplicates
            if !versions.iter().any(|(_, v)| v == &version_str) {
                versions.push((version, version_str));
            }
        }

        if versions.is_empty() {
            return Err(anyhow!(
                "Package '{}' exists but has no suitable versions. All releases may be yanked or pre-release.",
                package
            ));
        }

        versions.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(versions)
    }

    /// Parse Simple API HTML response (for private registries)
    /// Extracts versions from package filenames in anchor tags
    fn parse_simple_api_response(
        &self,
        html: &str,
        package: &str,
        include_prereleases: bool,
    ) -> Result<Vec<(Version, String)>> {
        let mut versions: Vec<(Version, String)> = Vec::new();
        let normalized = package.to_lowercase().replace('_', "-");

        // Extract versions from href attributes in anchor tags
        // Format: <a href="...">package-version.tar.gz</a> or package-version-py3-none-any.whl
        // Yanked packages have data-yanked attribute: <a href="..." data-yanked="">...</a>
        for line in html.lines() {
            // Skip yanked packages (marked with data-yanked attribute)
            if line.contains("data-yanked") {
                continue;
            }

            let Some(start) = line.find('>') else {
                continue;
            };
            let Some(end) = line[start..].find('<') else {
                continue;
            };
            let filename = &line[start + 1..start + end];
            let Some(version_str) = Self::extract_version_from_filename(filename, &normalized)
            else {
                continue;
            };

            if !include_prereleases && !Self::is_stable_version(&version_str) {
                continue;
            }

            let Ok(version) = version_str.parse::<Version>() else {
                continue;
            };

            // Avoid duplicates
            if !versions.iter().any(|(_, v)| v == &version_str) {
                versions.push((version, version_str));
            }
        }

        if versions.is_empty() {
            return Err(anyhow!(
                "Package '{}' exists but has no suitable versions. All releases may be yanked or pre-release.",
                package
            ));
        }

        versions.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(versions)
    }

    /// Extract version from a package filename
    /// Handles: package-1.0.0.tar.gz, package-1.0.0-py3-none-any.whl, etc.
    fn extract_version_from_filename(filename: &str, normalized_package: &str) -> Option<String> {
        // Remove file extension
        let name = filename
            .trim_end_matches(".tar.gz")
            .trim_end_matches(".zip")
            .trim_end_matches(".whl")
            .trim_end_matches(".egg");

        // Package name can have - or _ replaced, normalize for matching
        let name_lower = name.to_lowercase();
        let pkg_with_dash = normalized_package;
        let pkg_with_underscore = normalized_package.replace('-', "_");

        // Find where the version starts (after package name and separator)
        let version_start = if name_lower.starts_with(&format!("{}-", pkg_with_dash)) {
            Some(pkg_with_dash.len() + 1)
        } else if name_lower.starts_with(&format!("{}-", pkg_with_underscore)) {
            Some(pkg_with_underscore.len() + 1)
        } else {
            None
        };

        if let Some(start) = version_start {
            let rest = &name[start..];
            // For wheel files, version ends at first '-' after version
            // For source dists, version is the rest of the name
            let version = if filename.ends_with(".whl") {
                // Wheel format: {distribution}-{version}(-{build tag})?-{python tag}-{abi tag}-{platform tag}.whl
                rest.split('-').next().unwrap_or(rest)
            } else {
                rest
            };
            Some(version.to_string())
        } else {
            None
        }
    }
}

impl Default for PyPiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PyPiRegistry {
    /// Detect extra index URLs from environment variables and pip.conf
    /// Supports UV_EXTRA_INDEX_URL and PIP_EXTRA_INDEX_URL (space or newline separated)
    pub fn detect_extra_index_urls() -> Vec<String> {
        let mut urls = Vec::new();

        // Check environment variables first
        for var in ["UV_EXTRA_INDEX_URL", "PIP_EXTRA_INDEX_URL"] {
            if let Ok(value) = std::env::var(var) {
                for url in value.split([' ', '\n']) {
                    let trimmed = url.trim();
                    if !trimmed.is_empty() {
                        urls.push(trimmed.to_string());
                    }
                }
            }
        }

        // Also check pip.conf/pip.ini files
        let pip_config = read_pip_config();
        for url in pip_config.extra_index_urls {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }

        urls
    }
}

/// A registry that queries multiple PyPI indexes using first-match strategy.
/// Queries indexes in order (primary first, then extras) and returns the first successful result.
/// This is safer than "highest version wins" as it avoids dependency confusion attacks.
pub struct MultiPyPiRegistry {
    registries: Vec<Arc<PyPiRegistry>>,
}

impl MultiPyPiRegistry {
    /// Create a new multi-index registry from a list of registries
    pub fn new(registries: Vec<Arc<PyPiRegistry>>) -> Self {
        Self { registries }
    }

    /// Create from a primary registry and extra index URLs
    pub fn from_primary_and_extras(primary: PyPiRegistry, extra_urls: Vec<String>) -> Self {
        let mut registries: Vec<Arc<PyPiRegistry>> = vec![Arc::new(primary)];

        for url in extra_urls {
            registries.push(Arc::new(PyPiRegistry::from_url(&url)));
        }

        Self { registries }
    }

    /// Get all registries
    pub fn registries(&self) -> &[Arc<PyPiRegistry>] {
        &self.registries
    }
}

#[async_trait]
impl Registry for MultiPyPiRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        if self.registries.is_empty() {
            return Err(anyhow!("No registries configured"));
        }

        // First-match strategy: query indexes in order, return first success
        let mut last_error: Option<anyhow::Error> = None;

        for registry in &self.registries {
            match registry.get_latest_version(package).await {
                Ok(version) => return Ok(version),
                Err(e) => {
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("No versions found for package '{}'", package)))
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        if self.registries.is_empty() {
            return Err(anyhow!("No registries configured"));
        }

        let mut last_error: Option<anyhow::Error> = None;

        for registry in &self.registries {
            match registry
                .get_latest_version_including_prereleases(package)
                .await
            {
                Ok(version) => return Ok(version),
                Err(e) => {
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("No versions found for package '{}'", package)))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        if self.registries.is_empty() {
            return Err(anyhow!("No registries configured"));
        }

        let mut last_error: Option<anyhow::Error> = None;

        for registry in &self.registries {
            match registry
                .get_latest_version_matching(package, constraints)
                .await
            {
                Ok(version) => return Ok(version),
                Err(e) => {
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow!(
                "No version of '{}' matches constraints '{}'",
                package,
                constraints
            )
        }))
    }

    fn name(&self) -> &'static str {
        "pypi"
    }
}

#[async_trait]
impl Registry for PyPiRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let versions = self.fetch_versions(package).await?;

        versions.first().map(|(_, s)| s.clone()).ok_or_else(|| {
            anyhow!(
                "Package '{}' exists but has no stable versions. Only pre-releases are available.",
                package
            )
        })
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let versions = self.fetch_all_versions(package).await?;

        versions.first().map(|(_, s)| s.clone()).ok_or_else(|| {
            anyhow!(
                "Package '{}' exists but has no versions available. All versions may be yanked.",
                package
            )
        })
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
    use serial_test::serial;
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
    fn test_extract_version_from_filename() {
        // Source distributions
        assert_eq!(
            PyPiRegistry::extract_version_from_filename("my-package-1.2.3.tar.gz", "my-package"),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            PyPiRegistry::extract_version_from_filename("my_package-1.2.3.tar.gz", "my-package"),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            PyPiRegistry::extract_version_from_filename("requests-2.31.0.tar.gz", "requests"),
            Some("2.31.0".to_string())
        );

        // Wheel files
        assert_eq!(
            PyPiRegistry::extract_version_from_filename(
                "my_package-1.2.3-py3-none-any.whl",
                "my-package"
            ),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            PyPiRegistry::extract_version_from_filename(
                "requests-2.31.0-py3-none-any.whl",
                "requests"
            ),
            Some("2.31.0".to_string())
        );

        // Pre-release versions
        assert_eq!(
            PyPiRegistry::extract_version_from_filename("mypackage-1.0.0a1.tar.gz", "mypackage"),
            Some("1.0.0a1".to_string())
        );

        // Non-matching package
        assert_eq!(
            PyPiRegistry::extract_version_from_filename("other-package-1.0.0.tar.gz", "mypackage"),
            None
        );
    }

    #[test]
    fn test_parse_simple_api_response() {
        let registry = PyPiRegistry::new();
        let html = r#"
<!DOCTYPE html>
<html>
  <head><title>Links for my-package</title></head>
  <body>
    <a href="../../packages/my_package-1.0.0.tar.gz">my_package-1.0.0.tar.gz</a>
    <a href="../../packages/my_package-1.1.0.tar.gz">my_package-1.1.0.tar.gz</a>
    <a href="../../packages/my_package-1.2.0-py3-none-any.whl">my_package-1.2.0-py3-none-any.whl</a>
    <a href="../../packages/my_package-2.0.0a1.tar.gz">my_package-2.0.0a1.tar.gz</a>
  </body>
</html>
"#;
        // Stable versions only
        let versions = registry
            .parse_simple_api_response(html, "my-package", false)
            .unwrap();
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].1, "1.2.0"); // Highest stable
        assert_eq!(versions[1].1, "1.1.0");
        assert_eq!(versions[2].1, "1.0.0");

        // Including prereleases
        let versions_with_pre = registry
            .parse_simple_api_response(html, "my-package", true)
            .unwrap();
        assert_eq!(versions_with_pre.len(), 4);
        assert_eq!(versions_with_pre[0].1, "2.0.0a1"); // Highest including prerelease
    }

    #[test]
    fn test_parse_simple_api_response_skips_yanked() {
        let registry = PyPiRegistry::new();
        let html = r#"
<!DOCTYPE html>
<html>
  <body>
    <a href="../../packages/my_package-1.0.0.tar.gz">my_package-1.0.0.tar.gz</a>
    <a href="../../packages/my_package-1.1.0.tar.gz" data-yanked="">my_package-1.1.0.tar.gz</a>
    <a href="../../packages/my_package-1.2.0.tar.gz" data-yanked="security issue">my_package-1.2.0.tar.gz</a>
    <a href="../../packages/my_package-1.3.0.tar.gz">my_package-1.3.0.tar.gz</a>
  </body>
</html>
"#;
        let versions = registry
            .parse_simple_api_response(html, "my-package", false)
            .unwrap();
        // Should only have 1.0.0 and 1.3.0 (1.1.0 and 1.2.0 are yanked)
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].1, "1.3.0");
        assert_eq!(versions[1].1, "1.0.0");
    }

    #[test]
    fn test_parse_simple_api_json_response() {
        let registry = PyPiRegistry::new();
        let data = SimpleApiResponse {
            files: vec![
                SimpleApiFile {
                    filename: "my_package-1.0.0.tar.gz".to_string(),
                    yanked: Some(false),
                },
                SimpleApiFile {
                    filename: "my_package-1.1.0.tar.gz".to_string(),
                    yanked: None,
                },
                SimpleApiFile {
                    filename: "my_package-1.2.0-py3-none-any.whl".to_string(),
                    yanked: Some(false),
                },
                SimpleApiFile {
                    filename: "my_package-2.0.0a1.tar.gz".to_string(),
                    yanked: Some(false),
                },
            ],
        };

        // Stable versions only
        let versions = registry
            .parse_simple_api_json_response(data.clone(), "my-package", false)
            .unwrap();
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].1, "1.2.0"); // Highest stable
        assert_eq!(versions[1].1, "1.1.0");
        assert_eq!(versions[2].1, "1.0.0");

        // Including prereleases
        let versions_with_pre = registry
            .parse_simple_api_json_response(data, "my-package", true)
            .unwrap();
        assert_eq!(versions_with_pre.len(), 4);
        assert_eq!(versions_with_pre[0].1, "2.0.0a1"); // Highest including prerelease
    }

    #[test]
    fn test_parse_simple_api_json_response_skips_yanked() {
        let registry = PyPiRegistry::new();
        let data = SimpleApiResponse {
            files: vec![
                SimpleApiFile {
                    filename: "my_package-1.0.0.tar.gz".to_string(),
                    yanked: Some(false),
                },
                SimpleApiFile {
                    filename: "my_package-1.1.0.tar.gz".to_string(),
                    yanked: Some(true), // Yanked
                },
                SimpleApiFile {
                    filename: "my_package-1.2.0.tar.gz".to_string(),
                    yanked: Some(true), // Yanked
                },
                SimpleApiFile {
                    filename: "my_package-1.3.0.tar.gz".to_string(),
                    yanked: Some(false),
                },
            ],
        };

        let versions = registry
            .parse_simple_api_json_response(data, "my-package", false)
            .unwrap();
        // Should only have 1.0.0 and 1.3.0 (1.1.0 and 1.2.0 are yanked)
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].1, "1.3.0");
        assert_eq!(versions[1].1, "1.0.0");
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
    #[serial]
    fn test_detect_credentials_from_env() {
        // Set UV_INDEX_* credentials
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::set_var("UV_INDEX_USERNAME", "uvuser");
            std::env::set_var("UV_INDEX_PASSWORD", "uvpass");
        }

        let creds = PyPiRegistry::detect_credentials("https://pypi.example.com/simple");
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.username, "uvuser");
        assert_eq!(creds.password, "uvpass");

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("UV_INDEX_USERNAME");
            std::env::remove_var("UV_INDEX_PASSWORD");
        }
    }

    #[test]
    #[serial]
    fn test_detect_bearer_token_uv() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::set_var("UV_INDEX_TOKEN", "test-token-123");
            std::env::remove_var("PIP_INDEX_TOKEN");
            std::env::remove_var("PYPI_TOKEN");
        }

        let token = PyPiRegistry::detect_bearer_token();
        assert!(token.is_some());
        assert_eq!(token.unwrap(), "test-token-123");

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("UV_INDEX_TOKEN");
        }
    }

    #[test]
    #[serial]
    fn test_detect_bearer_token_pip() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::remove_var("UV_INDEX_TOKEN");
            std::env::set_var("PIP_INDEX_TOKEN", "pip-token-456");
            std::env::remove_var("PYPI_TOKEN");
        }

        let token = PyPiRegistry::detect_bearer_token();
        assert!(token.is_some());
        assert_eq!(token.unwrap(), "pip-token-456");

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("PIP_INDEX_TOKEN");
        }
    }

    #[test]
    #[serial]
    fn test_detect_bearer_token_pypi() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::remove_var("UV_INDEX_TOKEN");
            std::env::remove_var("PIP_INDEX_TOKEN");
            std::env::set_var("PYPI_TOKEN", "pypi-token-789");
        }

        let token = PyPiRegistry::detect_bearer_token();
        assert!(token.is_some());
        assert_eq!(token.unwrap(), "pypi-token-789");

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("PYPI_TOKEN");
        }
    }

    #[test]
    #[serial]
    fn test_detect_bearer_token_none() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::remove_var("UV_INDEX_TOKEN");
            std::env::remove_var("PIP_INDEX_TOKEN");
            std::env::remove_var("PYPI_TOKEN");
            std::env::remove_var("POETRY_PYPI_TOKEN_PYPI");
            std::env::remove_var("POETRY_HTTP_BASIC_PYPI_PASSWORD");
        }

        let token = PyPiRegistry::detect_bearer_token();
        assert!(token.is_none());
    }

    #[test]
    #[serial]
    fn test_detect_auth_prefers_bearer() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            // Set both bearer token and basic auth
            std::env::set_var("UV_INDEX_TOKEN", "bearer-token");
            std::env::set_var("UV_INDEX_USERNAME", "username");
            std::env::set_var("UV_INDEX_PASSWORD", "password");
        }

        let auth = PyPiRegistry::detect_auth("https://pypi.example.com");
        assert!(matches!(auth, Some(PyPiAuth::Bearer(_))));

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("UV_INDEX_TOKEN");
            std::env::remove_var("UV_INDEX_USERNAME");
            std::env::remove_var("UV_INDEX_PASSWORD");
        }
    }

    #[test]
    #[serial]
    fn test_detect_auth_falls_back_to_basic() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            // No bearer token, only basic auth
            std::env::remove_var("UV_INDEX_TOKEN");
            std::env::remove_var("PIP_INDEX_TOKEN");
            std::env::remove_var("PYPI_TOKEN");
            std::env::set_var("UV_INDEX_USERNAME", "username");
            std::env::set_var("UV_INDEX_PASSWORD", "password");
        }

        let auth = PyPiRegistry::detect_auth("https://pypi.example.com");
        assert!(matches!(auth, Some(PyPiAuth::Basic(_))));

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("UV_INDEX_USERNAME");
            std::env::remove_var("UV_INDEX_PASSWORD");
        }
    }

    #[test]
    fn test_registry_with_bearer_token() {
        // Just verify that the registry can be created with a bearer token
        let _registry = PyPiRegistry::with_index_url_and_bearer_token(
            "https://pypi.example.com".to_string(),
            "test-token".to_string(),
        );
        // The token is used to set default headers in the client
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
        // URL with embedded credentials - /simple suffix stripped
        let registry = PyPiRegistry::from_url("https://user:pass@pypi.example.com/simple");
        // The URL should be cleaned (credentials removed) and normalized
        assert_eq!(registry.index_url(), "https://pypi.example.com");
    }

    #[test]
    fn test_from_url_without_credentials() {
        // URL without credentials - /simple suffix stripped
        let registry = PyPiRegistry::from_url("https://pypi.example.com/simple");
        assert_eq!(registry.index_url(), "https://pypi.example.com");
    }

    #[test]
    fn test_from_url_strips_trailing_slash() {
        let registry = PyPiRegistry::from_url("https://pypi.example.com/simple/");
        assert_eq!(registry.index_url(), "https://pypi.example.com");
    }

    #[test]
    fn test_from_url_already_json_api() {
        // URL already in JSON API format should not be changed
        let registry = PyPiRegistry::from_url("https://pypi.example.com/pypi");
        assert_eq!(registry.index_url(), "https://pypi.example.com/pypi");
    }

    #[test]
    fn test_from_url_nexus_style() {
        // Nexus Repository Manager style URL
        let registry =
            PyPiRegistry::from_url("https://nexus.example.com/repository/pypi-private/simple");
        assert_eq!(
            registry.index_url(),
            "https://nexus.example.com/repository/pypi-private"
        );
    }

    #[test]
    fn test_normalize_index_url() {
        // Simple API -> strips /simple suffix
        assert_eq!(
            PyPiRegistry::normalize_index_url("https://example.com/simple"),
            "https://example.com"
        );
        assert_eq!(
            PyPiRegistry::normalize_index_url("https://example.com/simple/"),
            "https://example.com"
        );
        // Nexus-style path
        assert_eq!(
            PyPiRegistry::normalize_index_url("https://nexus.example.com/repository/pypi/simple"),
            "https://nexus.example.com/repository/pypi"
        );
        // Already without /simple - unchanged
        assert_eq!(
            PyPiRegistry::normalize_index_url("https://example.com/pypi"),
            "https://example.com/pypi"
        );
        // Other paths - unchanged
        assert_eq!(
            PyPiRegistry::normalize_index_url("https://example.com/custom"),
            "https://example.com/custom"
        );
    }

    // Tests for extra index URLs functionality

    #[test]
    #[serial]
    fn test_detect_extra_index_urls_empty() {
        // Ensure env vars are unset
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::remove_var("UV_EXTRA_INDEX_URL");
            std::env::remove_var("PIP_EXTRA_INDEX_URL");
        }

        let urls = PyPiRegistry::detect_extra_index_urls();
        assert!(urls.is_empty());
    }

    #[test]
    #[serial]
    fn test_detect_extra_index_urls_single() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::set_var("UV_EXTRA_INDEX_URL", "https://extra1.pypi.org/simple");
            std::env::remove_var("PIP_EXTRA_INDEX_URL");
        }

        let urls = PyPiRegistry::detect_extra_index_urls();
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "https://extra1.pypi.org/simple");

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("UV_EXTRA_INDEX_URL");
        }
    }

    #[test]
    #[serial]
    fn test_detect_extra_index_urls_space_separated() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::set_var(
                "UV_EXTRA_INDEX_URL",
                "https://extra1.pypi.org/simple https://extra2.pypi.org/simple",
            );
            std::env::remove_var("PIP_EXTRA_INDEX_URL");
        }

        let urls = PyPiRegistry::detect_extra_index_urls();
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://extra1.pypi.org/simple");
        assert_eq!(urls[1], "https://extra2.pypi.org/simple");

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("UV_EXTRA_INDEX_URL");
        }
    }

    #[test]
    #[serial]
    fn test_detect_extra_index_urls_newline_separated() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::set_var(
                "PIP_EXTRA_INDEX_URL",
                "https://extra1.pypi.org/simple\nhttps://extra2.pypi.org/simple",
            );
            std::env::remove_var("UV_EXTRA_INDEX_URL");
        }

        let urls = PyPiRegistry::detect_extra_index_urls();
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://extra1.pypi.org/simple");
        assert_eq!(urls[1], "https://extra2.pypi.org/simple");

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("PIP_EXTRA_INDEX_URL");
        }
    }

    #[test]
    #[serial]
    fn test_detect_extra_index_urls_combined() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::set_var("UV_EXTRA_INDEX_URL", "https://uv-extra.pypi.org/simple");
            std::env::set_var("PIP_EXTRA_INDEX_URL", "https://pip-extra.pypi.org/simple");
        }

        let urls = PyPiRegistry::detect_extra_index_urls();
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"https://uv-extra.pypi.org/simple".to_string()));
        assert!(urls.contains(&"https://pip-extra.pypi.org/simple".to_string()));

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("UV_EXTRA_INDEX_URL");
            std::env::remove_var("PIP_EXTRA_INDEX_URL");
        }
    }

    #[test]
    #[serial]
    fn test_detect_extra_index_urls_trims_whitespace() {
        // SAFETY: Test runs in isolation with #[serial]
        unsafe {
            std::env::set_var(
                "UV_EXTRA_INDEX_URL",
                "  https://extra1.pypi.org/simple  \n  https://extra2.pypi.org/simple  ",
            );
            std::env::remove_var("PIP_EXTRA_INDEX_URL");
        }

        let urls = PyPiRegistry::detect_extra_index_urls();
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://extra1.pypi.org/simple");
        assert_eq!(urls[1], "https://extra2.pypi.org/simple");

        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("UV_EXTRA_INDEX_URL");
        }
    }

    // Tests for MultiPyPiRegistry

    #[test]
    fn test_multi_registry_from_primary_and_extras() {
        let primary = PyPiRegistry::new();
        let extras = vec![
            "https://extra1.pypi.org/simple".to_string(),
            "https://extra2.pypi.org/simple".to_string(),
        ];

        let multi = MultiPyPiRegistry::from_primary_and_extras(primary, extras);
        assert_eq!(multi.registries().len(), 3); // 1 primary + 2 extras
    }

    #[test]
    fn test_multi_registry_no_extras() {
        let primary = PyPiRegistry::new();
        let extras: Vec<String> = vec![];

        let multi = MultiPyPiRegistry::from_primary_and_extras(primary, extras);
        assert_eq!(multi.registries().len(), 1); // Just the primary
    }

    #[test]
    fn test_multi_registry_name() {
        let primary = PyPiRegistry::new();
        let multi = MultiPyPiRegistry::from_primary_and_extras(primary, vec![]);
        assert_eq!(multi.name(), "pypi");
    }

    mod multi_registry_integration {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn test_multi_registry_first_match_returns_primary() {
            let mock_server1 = MockServer::start().await;

            // Simple API returns 404 (fallback to JSON API)
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server1)
                .await;

            // Server 1 (primary) has version 1.0.0
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(r#"{"releases": {"1.0.0": [{"yanked": false}]}}"#),
                )
                .expect(1) // Should be called exactly once
                .mount(&mock_server1)
                .await;

            // Extra index URL that won't be queried (first-match stops at primary)
            let primary = PyPiRegistry::with_index_url(mock_server1.uri());
            let extras = vec!["https://unused-extra.example.com/simple".to_string()];

            let multi = MultiPyPiRegistry::from_primary_and_extras(primary, extras);
            let version = multi.get_latest_version("testpkg").await.unwrap();

            // First-match: returns primary's version, extra not queried
            assert_eq!(version, "1.0.0");
        }

        #[tokio::test]
        async fn test_multi_registry_falls_back_on_failure() {
            let mock_server1 = MockServer::start().await;
            let mock_server2 = MockServer::start().await;

            // Server 1 (primary): Simple API returns 404
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server1)
                .await;

            // Server 1 (primary): JSON API also returns 404 (package not found)
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .respond_with(ResponseTemplate::new(404))
                .expect(1) // Primary is tried first
                .mount(&mock_server1)
                .await;

            // Server 2 (extra): Simple API returns 404
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server2)
                .await;

            // Server 2 (extra): JSON API has version 1.5.0
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(r#"{"releases": {"1.5.0": [{"yanked": false}]}}"#),
                )
                .expect(1) // Falls back to extra when primary fails
                .mount(&mock_server2)
                .await;

            let primary = PyPiRegistry::with_index_url(mock_server1.uri());
            let extras = vec![mock_server2.uri()];

            let multi = MultiPyPiRegistry::from_primary_and_extras(primary, extras);
            let version = multi.get_latest_version("testpkg").await.unwrap();

            // First-match with fallback: returns extra's version when primary fails
            assert_eq!(version, "1.5.0");
        }

        #[tokio::test]
        async fn test_multi_registry_all_fail_returns_error() {
            let mock_server1 = MockServer::start().await;
            let mock_server2 = MockServer::start().await;

            // Both servers return 404
            Mock::given(method("GET"))
                .and(path("/testpkg/json"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server1)
                .await;

            Mock::given(method("GET"))
                .and(path("/testpkg/json"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server2)
                .await;

            let primary = PyPiRegistry::with_index_url(mock_server1.uri());
            let extras = vec![mock_server2.uri()];

            let multi = MultiPyPiRegistry::from_primary_and_extras(primary, extras);
            let result = multi.get_latest_version("testpkg").await;

            assert!(result.is_err());
        }

        #[tokio::test]
        async fn test_multi_registry_empty_fails() {
            let multi = MultiPyPiRegistry::new(vec![]);
            let result = multi.get_latest_version("testpkg").await;

            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("No registries configured")
            );
        }

        #[tokio::test]
        async fn test_multi_registry_prereleases_first_match() {
            let mock_server1 = MockServer::start().await;

            // Simple API returns 404 (fallback to JSON API)
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server1)
                .await;

            // Server 1 (primary) has prerelease version
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(r#"{"releases": {"2.0.0a1": [{"yanked": false}]}}"#),
                )
                .expect(1)
                .mount(&mock_server1)
                .await;

            let primary = PyPiRegistry::with_index_url(mock_server1.uri());
            let extras = vec!["https://unused-extra.example.com/simple".to_string()];

            let multi = MultiPyPiRegistry::from_primary_and_extras(primary, extras);
            let version = multi
                .get_latest_version_including_prereleases("testpkg")
                .await
                .unwrap();

            assert_eq!(version, "2.0.0a1");
        }

        #[tokio::test]
        async fn test_multi_registry_prereleases_fallback() {
            let mock_server1 = MockServer::start().await;
            let mock_server2 = MockServer::start().await;

            // Server 1 (primary): Simple API returns 404
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server1)
                .await;

            // Server 1 (primary): JSON API returns 404
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .respond_with(ResponseTemplate::new(404))
                .expect(1)
                .mount(&mock_server1)
                .await;

            // Server 2 (extra): Simple API returns 404
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server2)
                .await;

            // Server 2 (extra): JSON API has prerelease version
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(r#"{"releases": {"3.0.0b2": [{"yanked": false}]}}"#),
                )
                .expect(1)
                .mount(&mock_server2)
                .await;

            let primary = PyPiRegistry::with_index_url(mock_server1.uri());
            let extras = vec![mock_server2.uri()];

            let multi = MultiPyPiRegistry::from_primary_and_extras(primary, extras);
            let version = multi
                .get_latest_version_including_prereleases("testpkg")
                .await
                .unwrap();

            assert_eq!(version, "3.0.0b2");
        }

        #[tokio::test]
        async fn test_multi_registry_matching_first_match() {
            let mock_server1 = MockServer::start().await;

            // Simple API returns 404 (fallback to JSON API)
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server1)
                .await;

            // Server 1 (primary) has versions 1.0.0 and 2.0.0
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"releases": {"1.0.0": [{"yanked": false}], "2.0.0": [{"yanked": false}]}}"#,
                ))
                .expect(1)
                .mount(&mock_server1)
                .await;

            let primary = PyPiRegistry::with_index_url(mock_server1.uri());
            let extras = vec!["https://unused-extra.example.com/simple".to_string()];

            let multi = MultiPyPiRegistry::from_primary_and_extras(primary, extras);
            let version = multi
                .get_latest_version_matching("testpkg", ">=1.0.0,<2.0.0")
                .await
                .unwrap();

            assert_eq!(version, "1.0.0");
        }

        #[tokio::test]
        async fn test_multi_registry_matching_fallback() {
            let mock_server1 = MockServer::start().await;
            let mock_server2 = MockServer::start().await;

            // Server 1 (primary): Simple API returns 404
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server1)
                .await;

            // Server 1 (primary): JSON API returns 404
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .respond_with(ResponseTemplate::new(404))
                .expect(1)
                .mount(&mock_server1)
                .await;

            // Server 2 (extra): Simple API returns 404
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server2)
                .await;

            // Server 2 (extra): JSON API has version that matches constraint
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"releases": {"1.5.0": [{"yanked": false}], "3.0.0": [{"yanked": false}]}}"#,
                ))
                .expect(1)
                .mount(&mock_server2)
                .await;

            let primary = PyPiRegistry::with_index_url(mock_server1.uri());
            let extras = vec![mock_server2.uri()];

            let multi = MultiPyPiRegistry::from_primary_and_extras(primary, extras);
            let version = multi
                .get_latest_version_matching("testpkg", ">=1.0.0,<2.0.0")
                .await
                .unwrap();

            assert_eq!(version, "1.5.0");
        }

        #[tokio::test]
        async fn test_pep691_json_format() {
            let mock_server = MockServer::start().await;

            // Server responds with PEP 691 JSON format
            // Use set_body_raw to properly set Content-Type (insert_header doesn't work after set_body_string)
            let json_body = r#"{"files": [{"filename": "testpkg-1.0.0.tar.gz", "yanked": false}, {"filename": "testpkg-2.0.0.tar.gz", "yanked": false}]}"#;
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(json_body.as_bytes(), "application/vnd.pypi.simple.v1+json"),
                )
                .expect(1)
                .mount(&mock_server)
                .await;

            let registry = PyPiRegistry::with_index_url(mock_server.uri());
            let version = registry.get_latest_version("testpkg").await.unwrap();

            assert_eq!(version, "2.0.0");
        }

        #[tokio::test]
        async fn test_pep691_json_skips_yanked() {
            let mock_server = MockServer::start().await;

            // Server responds with PEP 691 JSON format including yanked versions
            let json_body = r#"{"files": [{"filename": "testpkg-1.0.0.tar.gz", "yanked": false}, {"filename": "testpkg-2.0.0.tar.gz", "yanked": true}]}"#;
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(json_body.as_bytes(), "application/vnd.pypi.simple.v1+json"),
                )
                .expect(1)
                .mount(&mock_server)
                .await;

            let registry = PyPiRegistry::with_index_url(mock_server.uri());
            let version = registry.get_latest_version("testpkg").await.unwrap();

            // Should return 1.0.0 since 2.0.0 is yanked
            assert_eq!(version, "1.0.0");
        }

        #[tokio::test]
        async fn test_simple_api_html_fallback() {
            let mock_server = MockServer::start().await;

            // Server responds with HTML (no PEP 691 support)
            // Each <a> tag needs to be on its own line for the parser
            let html = r#"<!DOCTYPE html>
<html>
<body>
<a href="testpkg-1.0.0.tar.gz">testpkg-1.0.0.tar.gz</a>
<a href="testpkg-2.0.0.tar.gz">testpkg-2.0.0.tar.gz</a>
</body>
</html>"#;
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
                .expect(1)
                .mount(&mock_server)
                .await;

            let registry = PyPiRegistry::with_index_url(mock_server.uri());
            let version = registry.get_latest_version("testpkg").await.unwrap();

            assert_eq!(version, "2.0.0");
        }
    }
}
