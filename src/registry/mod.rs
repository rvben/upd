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

    /// Registry name for display
    fn name(&self) -> &'static str;
}
