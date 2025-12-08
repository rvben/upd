mod npm;
mod pypi;

pub use npm::NpmRegistry;
pub use pypi::PyPiRegistry;

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Registry: Send + Sync {
    /// Get the latest stable version of a package
    async fn get_latest_version(&self, package: &str) -> Result<String>;

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
