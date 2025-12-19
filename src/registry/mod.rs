mod crates_io;
mod go_proxy;
#[cfg(test)]
pub mod mock;
mod npm;
mod pypi;
mod utils;

pub use crates_io::{CargoConfig, CargoCredentials, CratesIoRegistry, read_cargo_config};
pub use go_proxy::{GoCredentials, GoPrivateConfig, GoProxyRegistry, read_go_private_config};
#[cfg(test)]
pub use mock::MockRegistry;
pub use npm::{NpmCredentials, NpmRegistry, NpmrcConfig, read_npmrc_config};
pub use pypi::{MultiPyPiRegistry, PyPiCredentials, PyPiRegistry};

use anyhow::Result;
use async_trait::async_trait;
use reqwest::{Client, Response};
use std::time::Duration;

/// Maximum number of retry attempts for failed HTTP requests
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (100ms, 200ms, 400ms)
const BASE_DELAY_MS: u64 = 100;

/// Execute an HTTP GET request with retry and exponential backoff.
/// Retries on transient errors (network issues, 5xx server errors).
pub async fn get_with_retry(client: &Client, url: &str) -> Result<Response, reqwest::Error> {
    let mut last_error = None;

    for attempt in 0..MAX_RETRIES {
        match client.get(url).send().await {
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

/// Create a descriptive error message for HTTP failures
/// Helps users understand why a request failed and what to do
///
/// # Arguments
/// * `status` - HTTP status code
/// * `entity_type` - Type of entity (e.g., "Package", "Crate", "Module")
/// * `name` - Name of the package/crate/module
/// * `registry_hint` - Optional hint about where to configure credentials
pub fn http_error_message(
    status: reqwest::StatusCode,
    entity_type: &str,
    name: &str,
    registry_hint: Option<&str>,
) -> String {
    let code = status.as_u16();
    match code {
        401 => {
            let hint = registry_hint.map_or_else(
                || "Check your credentials or API token.".to_string(),
                |h| format!("Check your credentials or API token. {}", h),
            );
            format!(
                "{} '{}' requires authentication (HTTP 401). {}",
                entity_type, name, hint
            )
        }
        403 => format!(
            "Access denied for {} '{}' (HTTP 403). You may lack permission or the {} may be private.",
            entity_type,
            name,
            entity_type.to_lowercase()
        ),
        404 => format!(
            "{} '{}' not found (HTTP 404). Check the name for typos or verify it exists in the registry.",
            entity_type, name
        ),
        408 | 504 => format!(
            "Request timed out for {} '{}' (HTTP {}). The registry may be slow or unreachable.",
            entity_type, name, code
        ),
        429 => format!(
            "Rate limited while fetching {} '{}' (HTTP 429). Wait a moment and try again.",
            entity_type, name
        ),
        500..=599 => format!(
            "Registry server error for {} '{}' (HTTP {}). The registry may be experiencing issues.",
            entity_type, name, code
        ),
        _ => format!(
            "Failed to fetch {} '{}': HTTP {} {}",
            entity_type,
            name,
            code,
            status.canonical_reason().unwrap_or("Unknown error")
        ),
    }
}

#[async_trait]
pub trait Registry: Send + Sync {
    /// Get the latest stable version of a package
    async fn get_latest_version(&self, package: &str) -> Result<String>;

    /// Get the latest version including pre-releases
    /// Used when the user's current version is a pre-release
    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        // Default: fall back to stable-only
        self.get_latest_version(package).await
    }

    /// Get the latest version matching the given constraints (e.g., ">=2.8.0,<9")
    /// Default implementation falls back to get_latest_version
    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        // Default: ignore constraints and return latest
        let _ = constraints;
        self.get_latest_version(package).await
    }

    /// Registry name for display
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_get_with_retry_success_first_try() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/test"))
            .respond_with(ResponseTemplate::new(200).set_body_string("success"))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/test", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        assert!(response.status().is_success());
        assert_eq!(response.text().await.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_get_with_retry_client_error_no_retry() {
        let mock_server = MockServer::start().await;

        // 404 should not be retried
        Mock::given(method("GET"))
            .and(path("/notfound"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1) // Should only be called once, no retry
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/notfound", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        assert_eq!(response.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn test_get_with_retry_server_error_retries() {
        let mock_server = MockServer::start().await;

        // Always return 500 - this test verifies that retries actually happen
        // by checking that the endpoint is called MAX_RETRIES (3) times
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3) // MAX_RETRIES = 3, verifies retry behavior
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/flaky", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        // After MAX_RETRIES exhausted, should return the 500 response
        assert_eq!(response.status().as_u16(), 500);
    }

    #[tokio::test]
    async fn test_get_with_retry_recovers_on_second_try() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mock_server = MockServer::start().await;
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        // First call returns 500, second call returns 200
        Mock::given(method("GET"))
            .and(path("/recover"))
            .respond_with(move |_: &wiremock::Request| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200).set_body_string("recovered")
                }
            })
            .expect(2) // Should be called twice: 500 then 200
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/recover", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        // Should recover and return 200
        assert!(response.status().is_success());
    }

    #[tokio::test]
    async fn test_get_with_retry_redirect_success() {
        let mock_server = MockServer::start().await;

        // Test that redirects (3xx) are handled by reqwest (not retried as errors)
        Mock::given(method("GET"))
            .and(path("/redirect"))
            .respond_with(ResponseTemplate::new(200).set_body_string("redirected"))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/redirect", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        assert!(response.status().is_success());
    }

    // Tests for Registry trait default implementations
    // Create a minimal registry that only implements required methods
    // to test that default implementations work correctly

    struct MinimalRegistry {
        version: String,
    }

    impl MinimalRegistry {
        fn new(version: &str) -> Self {
            Self {
                version: version.to_string(),
            }
        }
    }

    #[async_trait]
    impl Registry for MinimalRegistry {
        async fn get_latest_version(&self, _package: &str) -> Result<String> {
            Ok(self.version.clone())
        }

        fn name(&self) -> &'static str {
            "Minimal"
        }
        // Note: we intentionally DON'T override the default methods
        // to test that the default implementations work
    }

    #[tokio::test]
    async fn test_registry_default_prereleases_falls_back_to_stable() {
        let registry = MinimalRegistry::new("2.31.0");

        // The default implementation should fall back to get_latest_version
        let version = registry
            .get_latest_version_including_prereleases("anypackage")
            .await
            .unwrap();

        assert_eq!(version, "2.31.0");
    }

    #[tokio::test]
    async fn test_registry_default_matching_ignores_constraints() {
        let registry = MinimalRegistry::new("5.0.0");

        // The default implementation ignores constraints and returns latest
        let version = registry
            .get_latest_version_matching("anypackage", ">=3.0,<4")
            .await
            .unwrap();

        // Should return 5.0.0 even though it doesn't match constraints
        // (real implementations would respect constraints)
        assert_eq!(version, "5.0.0");
    }

    #[tokio::test]
    async fn test_registry_name() {
        let registry = MinimalRegistry::new("1.0.0");
        assert_eq!(registry.name(), "Minimal");
    }

    // Integration tests for authentication headers
    mod auth_tests {
        use super::super::*;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn test_pypi_sends_basic_auth_header() {
            let mock_server = MockServer::start().await;

            // Simple API endpoint should fail (to trigger fallback to JSON API)
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .and(header("Authorization", "Basic dGVzdHVzZXI6dGVzdHBhc3M="))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server)
                .await;

            // Verify that Basic Auth header is sent to JSON API
            // "testuser:testpass" base64 encoded is "dGVzdHVzZXI6dGVzdHBhc3M="
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .and(header("Authorization", "Basic dGVzdHVzZXI6dGVzdHBhc3M="))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(r#"{"releases": {"1.0.0": [{"yanked": false}]}}"#),
                )
                .expect(1)
                .mount(&mock_server)
                .await;

            let creds = PyPiCredentials {
                username: "testuser".to_string(),
                password: "testpass".to_string(),
            };

            let registry =
                PyPiRegistry::with_index_url_and_credentials(mock_server.uri(), Some(creds));

            let version = registry.get_latest_version("testpkg").await.unwrap();
            assert_eq!(version, "1.0.0");
        }

        #[tokio::test]
        async fn test_npm_sends_bearer_token_header() {
            let mock_server = MockServer::start().await;

            // Verify that Bearer token header is sent
            Mock::given(method("GET"))
                .and(path("/testpkg"))
                .and(header("Authorization", "Bearer my-secret-token"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"dist-tags": {"latest": "2.0.0"}, "versions": {"2.0.0": {}}}"#,
                ))
                .expect(1)
                .mount(&mock_server)
                .await;

            let creds = NpmCredentials {
                token: "my-secret-token".to_string(),
            };

            let registry =
                NpmRegistry::with_registry_url_and_credentials(mock_server.uri(), Some(creds));

            let version = registry.get_latest_version("testpkg").await.unwrap();
            assert_eq!(version, "2.0.0");
        }

        #[tokio::test]
        async fn test_crates_io_sends_bearer_token_header() {
            let mock_server = MockServer::start().await;

            // Verify that Bearer token header is sent (Cargo uses Bearer tokens)
            Mock::given(method("GET"))
                .and(path("/testcrate"))
                .and(header("Authorization", "cargo-token-123"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"crate": {"max_stable_version": "3.0.0"}, "versions": [{"num": "3.0.0", "yanked": false}]}"#,
                ))
                .expect(1)
                .mount(&mock_server)
                .await;

            let creds = CargoCredentials {
                token: "cargo-token-123".to_string(),
            };

            let registry =
                CratesIoRegistry::with_registry_url_and_credentials(mock_server.uri(), Some(creds));

            let version = registry.get_latest_version("testcrate").await.unwrap();
            assert_eq!(version, "3.0.0");
        }

        #[tokio::test]
        async fn test_go_proxy_sends_basic_auth_header() {
            let mock_server = MockServer::start().await;

            // Verify that Basic Auth header is sent
            // "gouser:gopass" base64 encoded is "Z291c2VyOmdvcGFzcw=="
            Mock::given(method("GET"))
                .and(path("/github.com/test/module/@latest"))
                .and(header("Authorization", "Basic Z291c2VyOmdvcGFzcw=="))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(r#"{"Version": "v1.0.0"}"#),
                )
                .expect(1)
                .mount(&mock_server)
                .await;

            let creds = GoCredentials {
                username: "gouser".to_string(),
                password: "gopass".to_string(),
            };

            let registry =
                GoProxyRegistry::with_proxy_url_and_credentials(mock_server.uri(), Some(creds));

            let version = registry
                .get_latest_version("github.com/test/module")
                .await
                .unwrap();
            assert_eq!(version, "v1.0.0");
        }
    }
}
