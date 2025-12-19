#[cfg(test)]
use super::utils::read_netrc_credentials_from_path;
use super::utils::{base64_encode, read_netrc_credentials};
use super::{Registry, http_error_message};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Client, Response};
use serde::Deserialize;
use std::time::Duration;

/// Maximum number of retry attempts for failed HTTP requests
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (100ms, 200ms, 400ms)
const BASE_DELAY_MS: u64 = 100;

/// Configuration for Go private modules from environment variables
#[derive(Debug, Clone, Default)]
pub struct GoPrivateConfig {
    /// Module path patterns from GOPRIVATE (bypass both proxy and sumdb)
    pub private_patterns: Vec<String>,
    /// Module path patterns from GONOPROXY (bypass proxy only)
    pub noproxy_patterns: Vec<String>,
    /// Module path patterns from GONOSUMDB (bypass sumdb only)
    pub nosumdb_patterns: Vec<String>,
}

impl GoPrivateConfig {
    /// Read Go private module configuration from environment variables
    pub fn from_env() -> Self {
        let private_patterns = std::env::var("GOPRIVATE")
            .ok()
            .map(|s| Self::parse_patterns(&s))
            .unwrap_or_default();

        let noproxy_patterns = std::env::var("GONOPROXY")
            .ok()
            .map(|s| Self::parse_patterns(&s))
            .unwrap_or_default();

        let nosumdb_patterns = std::env::var("GONOSUMDB")
            .ok()
            .map(|s| Self::parse_patterns(&s))
            .unwrap_or_default();

        Self {
            private_patterns,
            noproxy_patterns,
            nosumdb_patterns,
        }
    }

    /// Parse comma-separated glob patterns
    fn parse_patterns(s: &str) -> Vec<String> {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    }

    /// Check if a module path matches a glob pattern
    /// Supports * for glob matching (e.g., "github.com/myorg/*")
    fn matches_pattern(module: &str, pattern: &str) -> bool {
        if pattern.is_empty() {
            return false;
        }

        // Handle glob patterns with *
        if let Some(prefix) = pattern.strip_suffix("/*") {
            // Pattern like "github.com/myorg/*" matches "github.com/myorg/foo"
            // and "github.com/myorg/foo/bar"
            module.starts_with(prefix)
                && module.len() > prefix.len()
                && module.chars().nth(prefix.len()) == Some('/')
        } else if let Some(prefix) = pattern.strip_suffix('*') {
            // Pattern like "github.com/myorg*" matches anything starting with that
            module.starts_with(prefix)
        } else {
            // Exact match or prefix match (Go's default behavior)
            // "github.com/myorg" matches "github.com/myorg" and "github.com/myorg/foo"
            module == pattern || module.starts_with(&format!("{}/", pattern))
        }
    }

    /// Check if a module should bypass the proxy (GOPRIVATE or GONOPROXY)
    pub fn should_bypass_proxy(&self, module: &str) -> bool {
        // Check GOPRIVATE patterns first (they apply to both proxy and sumdb)
        for pattern in &self.private_patterns {
            if Self::matches_pattern(module, pattern) {
                return true;
            }
        }
        // Check GONOPROXY patterns
        for pattern in &self.noproxy_patterns {
            if Self::matches_pattern(module, pattern) {
                return true;
            }
        }
        false
    }

    /// Check if a module is considered private (matches GOPRIVATE)
    pub fn is_private(&self, module: &str) -> bool {
        for pattern in &self.private_patterns {
            if Self::matches_pattern(module, pattern) {
                return true;
            }
        }
        false
    }

    /// Check if there are any private module patterns configured
    pub fn has_private_patterns(&self) -> bool {
        !self.private_patterns.is_empty() || !self.noproxy_patterns.is_empty()
    }
}

/// Read Go private module configuration from environment variables
pub fn read_go_private_config() -> GoPrivateConfig {
    GoPrivateConfig::from_env()
}

/// Credentials for authenticating with a Go proxy or private module host
#[derive(Clone)]
pub struct GoCredentials {
    /// Username for Basic Auth
    pub username: String,
    /// Password for Basic Auth
    pub password: String,
}

impl std::fmt::Debug for GoCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoCredentials")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

pub struct GoProxyRegistry {
    client: Client,
    proxy_url: String,
}

#[derive(Debug, Deserialize)]
struct LatestResponse {
    #[serde(rename = "Version")]
    version: String,
}

impl GoProxyRegistry {
    pub fn new() -> Self {
        Self::with_proxy_url("https://proxy.golang.org".to_string())
    }

    pub fn with_proxy_url(proxy_url: String) -> Self {
        Self::with_proxy_url_and_credentials(proxy_url, None)
    }

    pub fn with_proxy_url_and_credentials(
        proxy_url: String,
        credentials: Option<GoCredentials>,
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
            .expect("Failed to create HTTP client. This usually indicates a TLS/SSL configuration issue on your system.");

        Self { client, proxy_url }
    }

    /// Detect custom proxy URL from environment (GOPROXY)
    pub fn detect_proxy_url() -> Option<String> {
        std::env::var("GOPROXY")
            .ok()
            .and_then(|s| {
                // GOPROXY can be comma-separated, take the first valid one
                s.split(',')
                    .map(|p| p.trim())
                    .find(|p| !p.is_empty() && *p != "direct" && *p != "off")
                    .map(|p| p.to_string())
            })
            .filter(|s| !s.is_empty())
    }

    /// Detect credentials from environment variables or netrc
    pub fn detect_credentials(proxy_url: &str) -> Option<GoCredentials> {
        // Try GOPROXY_USERNAME and GOPROXY_PASSWORD environment variables
        if let (Ok(username), Ok(password)) = (
            std::env::var("GOPROXY_USERNAME"),
            std::env::var("GOPROXY_PASSWORD"),
        ) && !username.is_empty()
            && !password.is_empty()
        {
            return Some(GoCredentials { username, password });
        }

        // Extract host from proxy URL and try netrc
        if let Ok(url) = url::Url::parse(proxy_url)
            && let Some(host) = url.host_str()
            && let Some(creds) = read_netrc_credentials(host)
        {
            return Some(GoCredentials {
                username: creds.login,
                password: creds.password,
            });
        }

        None
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

    /// Fetch list of all versions for a module
    async fn fetch_versions(&self, module: &str) -> Result<Vec<String>> {
        let escaped = Self::escape_module_path(module);
        let url = format!("{}/{}/@v/list", self.proxy_url, escaped);

        let response = self.get_with_retry(&url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Module",
                module,
                Some("For private modules, configure credentials in ~/.netrc or set GOPRIVATE.")
            )));
        }

        let text = response.text().await?;
        Ok(text
            .lines()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
    }

    /// Escape module path for URL (Go proxy convention)
    /// Uppercase letters become !lowercase (e.g., GitHub -> !git!hub)
    fn escape_module_path(module: &str) -> String {
        let mut result = String::with_capacity(module.len() * 2);
        for c in module.chars() {
            if c.is_ascii_uppercase() {
                result.push('!');
                result.push(c.to_ascii_lowercase());
            } else {
                result.push(c);
            }
        }
        result
    }

    /// Parse Go version string (removes 'v' prefix for semver parsing)
    fn parse_version(version: &str) -> Option<semver::Version> {
        let stripped = version.strip_prefix('v').unwrap_or(version);
        // Handle +incompatible suffix
        let without_incompatible = stripped.split('+').next().unwrap_or(stripped);
        semver::Version::parse(without_incompatible).ok()
    }

    fn is_prerelease(version: &str) -> bool {
        Self::parse_version(version)
            .map(|v| !v.pre.is_empty())
            .unwrap_or(false)
    }
}

impl Default for GoProxyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for GoProxyRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        // Try @latest endpoint first (returns latest stable)
        let escaped = Self::escape_module_path(package);
        let url = format!("{}/{}/@latest", self.proxy_url, escaped);

        if let Ok(response) = self.get_with_retry(&url).await
            && response.status().is_success()
            && let Ok(data) = response.json::<LatestResponse>().await
        {
            return Ok(data.version);
        }

        // Fallback: get version list and find latest stable
        let versions = self.fetch_versions(package).await?;

        let mut stable: Vec<_> = versions
            .iter()
            .filter(|v| !Self::is_prerelease(v))
            .filter_map(|v| Self::parse_version(v).map(|parsed| (parsed, v.clone())))
            .collect();

        stable.sort_by(|a, b| b.0.cmp(&a.0));

        stable.first().map(|(_, s)| s.clone()).ok_or_else(|| {
            anyhow!(
                "Module '{}' exists but has no stable versions. Only pre-releases are available.",
                package
            )
        })
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let versions = self.fetch_versions(package).await?;

        let mut all: Vec<_> = versions
            .iter()
            .filter_map(|v| Self::parse_version(v).map(|parsed| (parsed, v.clone())))
            .collect();

        all.sort_by(|a, b| b.0.cmp(&a.0));

        all.first().map(|(_, s)| s.clone()).ok_or_else(|| {
            anyhow!(
                "Module '{}' exists but has no versions available. All versions may be retracted.",
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

        let req = semver::VersionReq::parse(constraints).map_err(|e| {
            anyhow!(
                "Failed to parse version constraints '{}': {}",
                constraints,
                e
            )
        })?;

        let mut matching: Vec<_> = versions
            .iter()
            .filter(|v| !Self::is_prerelease(v))
            .filter_map(|v| Self::parse_version(v).map(|parsed| (parsed, v.clone())))
            .filter(|(parsed, _)| req.matches(parsed))
            .collect();

        matching.sort_by(|a, b| b.0.cmp(&a.0));

        matching.first().map(|(_, s)| s.clone()).ok_or_else(|| {
            anyhow!(
                "No version of '{}' matches constraints '{}'",
                package,
                constraints
            )
        })
    }

    fn name(&self) -> &'static str {
        "go-proxy"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_module_path() {
        assert_eq!(
            GoProxyRegistry::escape_module_path("github.com/foo/bar"),
            "github.com/foo/bar"
        );
        assert_eq!(
            GoProxyRegistry::escape_module_path("github.com/Azure/azure-sdk"),
            "github.com/!azure/azure-sdk"
        );
        assert_eq!(
            GoProxyRegistry::escape_module_path("github.com/BurntSushi/toml"),
            "github.com/!burnt!sushi/toml"
        );
    }

    #[test]
    fn test_parse_version() {
        assert_eq!(
            GoProxyRegistry::parse_version("v1.2.3"),
            Some(semver::Version::new(1, 2, 3))
        );
        assert_eq!(
            GoProxyRegistry::parse_version("v0.10.0"),
            Some(semver::Version::new(0, 10, 0))
        );
        // Handle +incompatible suffix
        assert_eq!(
            GoProxyRegistry::parse_version("v2.0.0+incompatible"),
            Some(semver::Version::new(2, 0, 0))
        );
    }

    #[test]
    fn test_is_prerelease() {
        assert!(!GoProxyRegistry::is_prerelease("v1.0.0"));
        assert!(GoProxyRegistry::is_prerelease("v1.0.0-alpha.1"));
        assert!(GoProxyRegistry::is_prerelease("v1.0.0-rc1"));
        assert!(GoProxyRegistry::is_prerelease("v1.0.0-beta"));
    }

    #[test]
    fn test_detect_proxy_url() {
        // Test with direct/off values
        // SAFETY: Tests run single-threaded by default, no concurrent reads
        unsafe {
            std::env::set_var("GOPROXY", "direct");
        }
        assert_eq!(GoProxyRegistry::detect_proxy_url(), None);

        unsafe {
            std::env::set_var("GOPROXY", "off");
        }
        assert_eq!(GoProxyRegistry::detect_proxy_url(), None);

        // Test with valid proxy
        unsafe {
            std::env::set_var("GOPROXY", "https://proxy.example.com");
        }
        assert_eq!(
            GoProxyRegistry::detect_proxy_url(),
            Some("https://proxy.example.com".to_string())
        );

        // Test with comma-separated (first valid wins)
        unsafe {
            std::env::set_var("GOPROXY", "https://proxy1.com,https://proxy2.com,direct");
        }
        assert_eq!(
            GoProxyRegistry::detect_proxy_url(),
            Some("https://proxy1.com".to_string())
        );

        // Clean up
        unsafe {
            std::env::remove_var("GOPROXY");
        }
    }

    #[test]
    fn test_detect_credentials_from_env() {
        // SAFETY: Tests run single-threaded by default, no concurrent reads
        unsafe {
            std::env::set_var("GOPROXY_USERNAME", "test-user");
            std::env::set_var("GOPROXY_PASSWORD", "test-pass");
        }

        let creds = GoProxyRegistry::detect_credentials("https://proxy.example.com");
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.username, "test-user");
        assert_eq!(creds.password, "test-pass");

        // SAFETY: Tests run single-threaded by default, no concurrent reads
        unsafe {
            std::env::remove_var("GOPROXY_USERNAME");
            std::env::remove_var("GOPROXY_PASSWORD");
        }
    }

    #[test]
    fn test_base64_encode() {
        // Test standard RFC 4648 encoding
        assert_eq!(base64_encode(""), "");
        assert_eq!(base64_encode("f"), "Zg==");
        assert_eq!(base64_encode("fo"), "Zm8=");
        assert_eq!(base64_encode("foo"), "Zm9v");
        assert_eq!(base64_encode("foob"), "Zm9vYg==");
        assert_eq!(base64_encode("fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode("foobar"), "Zm9vYmFy");
        // Test credential-like strings
        assert_eq!(base64_encode("user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn test_read_netrc_credentials() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a temp netrc file
        let mut netrc_file = NamedTempFile::new().unwrap();
        writeln!(
            netrc_file,
            "machine proxy.example.com login myuser password mypassword"
        )
        .unwrap();

        let netrc_path = netrc_file.path().to_path_buf();

        let creds = read_netrc_credentials_from_path(&netrc_path, "proxy.example.com");
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.login, "myuser");
        assert_eq!(creds.password, "mypassword");

        // Test non-existent host
        let creds = read_netrc_credentials_from_path(&netrc_path, "nonexistent.example.com");
        assert!(creds.is_none());
    }

    #[test]
    fn test_registry_with_credentials() {
        let creds = GoCredentials {
            username: "test-user".to_string(),
            password: "test-pass".to_string(),
        };
        // Verify that the registry can be created with credentials
        let _registry = GoProxyRegistry::with_proxy_url_and_credentials(
            "https://proxy.example.com".to_string(),
            Some(creds),
        );
    }

    #[test]
    fn test_go_private_config_parse_patterns() {
        let patterns = GoPrivateConfig::parse_patterns("github.com/myorg, gitlab.com/myteam");
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0], "github.com/myorg");
        assert_eq!(patterns[1], "gitlab.com/myteam");

        let empty = GoPrivateConfig::parse_patterns("");
        assert!(empty.is_empty());

        let single = GoPrivateConfig::parse_patterns("github.com/myorg");
        assert_eq!(single.len(), 1);
    }

    #[test]
    fn test_go_private_config_matches_pattern_exact() {
        // Exact match
        assert!(GoPrivateConfig::matches_pattern(
            "github.com/myorg",
            "github.com/myorg"
        ));
        // Prefix match (Go's default behavior)
        assert!(GoPrivateConfig::matches_pattern(
            "github.com/myorg/foo",
            "github.com/myorg"
        ));
        assert!(GoPrivateConfig::matches_pattern(
            "github.com/myorg/foo/bar",
            "github.com/myorg"
        ));
        // No match
        assert!(!GoPrivateConfig::matches_pattern(
            "github.com/otherorg",
            "github.com/myorg"
        ));
        assert!(!GoPrivateConfig::matches_pattern(
            "github.com/myorgfoo",
            "github.com/myorg"
        ));
    }

    #[test]
    fn test_go_private_config_matches_pattern_glob() {
        // Glob with /*
        assert!(GoPrivateConfig::matches_pattern(
            "github.com/myorg/foo",
            "github.com/myorg/*"
        ));
        assert!(GoPrivateConfig::matches_pattern(
            "github.com/myorg/foo/bar",
            "github.com/myorg/*"
        ));
        // The org itself doesn't match the /* pattern
        assert!(!GoPrivateConfig::matches_pattern(
            "github.com/myorg",
            "github.com/myorg/*"
        ));

        // Glob with trailing *
        assert!(GoPrivateConfig::matches_pattern(
            "github.com/myorgfoo",
            "github.com/myorg*"
        ));
        assert!(GoPrivateConfig::matches_pattern(
            "github.com/myorg/foo",
            "github.com/myorg*"
        ));
    }

    #[test]
    fn test_go_private_config_should_bypass_proxy() {
        let config = GoPrivateConfig {
            private_patterns: vec!["github.com/myorg".to_string()],
            noproxy_patterns: vec!["gitlab.com/myteam".to_string()],
            nosumdb_patterns: vec![],
        };

        // Matches GOPRIVATE
        assert!(config.should_bypass_proxy("github.com/myorg/foo"));
        // Matches GONOPROXY
        assert!(config.should_bypass_proxy("gitlab.com/myteam/bar"));
        // No match
        assert!(!config.should_bypass_proxy("github.com/otherorg/foo"));
    }

    #[test]
    fn test_go_private_config_is_private() {
        let config = GoPrivateConfig {
            private_patterns: vec!["github.com/myorg".to_string()],
            noproxy_patterns: vec!["gitlab.com/myteam".to_string()],
            nosumdb_patterns: vec![],
        };

        // Only GOPRIVATE modules are considered "private"
        assert!(config.is_private("github.com/myorg/foo"));
        // GONOPROXY modules are not considered "private" (just bypass proxy)
        assert!(!config.is_private("gitlab.com/myteam/bar"));
    }

    #[test]
    fn test_go_private_config_has_private_patterns() {
        let empty = GoPrivateConfig::default();
        assert!(!empty.has_private_patterns());

        let with_private = GoPrivateConfig {
            private_patterns: vec!["github.com/myorg".to_string()],
            noproxy_patterns: vec![],
            nosumdb_patterns: vec![],
        };
        assert!(with_private.has_private_patterns());

        let with_noproxy = GoPrivateConfig {
            private_patterns: vec![],
            noproxy_patterns: vec!["gitlab.com/myteam".to_string()],
            nosumdb_patterns: vec![],
        };
        assert!(with_noproxy.has_private_patterns());
    }

    #[test]
    fn test_go_private_config_from_env() {
        // SAFETY: Tests run single-threaded by default
        unsafe {
            std::env::set_var("GOPRIVATE", "github.com/myorg,gitlab.com/myteam");
            std::env::set_var("GONOPROXY", "github.com/noproxy");
            std::env::set_var("GONOSUMDB", "github.com/nosumdb");
        }

        let config = GoPrivateConfig::from_env();

        assert_eq!(config.private_patterns.len(), 2);
        assert_eq!(config.private_patterns[0], "github.com/myorg");
        assert_eq!(config.private_patterns[1], "gitlab.com/myteam");
        assert_eq!(config.noproxy_patterns.len(), 1);
        assert_eq!(config.noproxy_patterns[0], "github.com/noproxy");
        assert_eq!(config.nosumdb_patterns.len(), 1);
        assert_eq!(config.nosumdb_patterns[0], "github.com/nosumdb");

        // Clean up
        unsafe {
            std::env::remove_var("GOPRIVATE");
            std::env::remove_var("GONOPROXY");
            std::env::remove_var("GONOSUMDB");
        }
    }
}
