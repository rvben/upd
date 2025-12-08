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
