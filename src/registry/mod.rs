mod crates_io;
mod go_proxy;
mod npm;
mod pypi;

pub use crates_io::CratesIoRegistry;
pub use go_proxy::GoProxyRegistry;
pub use npm::NpmRegistry;
pub use pypi::PyPiRegistry;

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
