use crate::registry::Registry;
use anyhow::Result;
use async_trait::async_trait;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CACHE_TTL_HOURS: u64 = 24;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Cache {
    #[serde(default)]
    pypi: HashMap<String, CacheEntry>,
    #[serde(default)]
    npm: HashMap<String, CacheEntry>,
    #[serde(default, rename = "crates.io")]
    crates_io: HashMap<String, CacheEntry>,
    #[serde(default, rename = "go-proxy")]
    go_proxy: HashMap<String, CacheEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CacheEntry {
    pub version: String,
    pub fetched_at: u64, // Unix timestamp
}

impl Cache {
    pub fn load() -> Result<Self> {
        let path = Self::cache_path()?;

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)?;
        let cache: Cache = serde_json::from_str(&content)?;
        Ok(cache)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::cache_path()?;

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }

    /// Create a shared cache wrapped in `Arc<Mutex>` for thread-safe access
    pub fn new_shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::load().unwrap_or_default()))
    }

    /// Save a shared cache to disk
    pub fn save_shared(cache: &Arc<Mutex<Cache>>) -> Result<()> {
        cache
            .lock()
            .map_err(|e| anyhow::anyhow!("Cache lock poisoned: {}", e))?
            .save()
    }

    pub fn get(&self, registry: &str, package: &str) -> Option<String> {
        let entries = match registry {
            "pypi" => &self.pypi,
            "npm" => &self.npm,
            "crates.io" => &self.crates_io,
            "go-proxy" => &self.go_proxy,
            _ => return None,
        };

        entries.get(package).and_then(|entry| {
            if Self::is_expired(entry.fetched_at) {
                None
            } else {
                Some(entry.version.clone())
            }
        })
    }

    pub fn set(&mut self, registry: &str, package: &str, version: String) {
        let entries = match registry {
            "pypi" => &mut self.pypi,
            "npm" => &mut self.npm,
            "crates.io" => &mut self.crates_io,
            "go-proxy" => &mut self.go_proxy,
            _ => return,
        };

        let fetched_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        entries.insert(
            package.to_string(),
            CacheEntry {
                version,
                fetched_at,
            },
        );
    }

    pub fn clean() -> Result<()> {
        let path = Self::cache_path()?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    fn cache_path() -> Result<PathBuf> {
        // Check for override via environment variable
        if let Ok(dir) = std::env::var("UPD_CACHE_DIR") {
            return Ok(PathBuf::from(dir).join("versions.json"));
        }

        let proj_dirs = ProjectDirs::from("", "", "upd")
            .ok_or_else(|| anyhow::anyhow!("Could not determine cache directory"))?;

        Ok(proj_dirs.cache_dir().join("versions.json"))
    }

    fn is_expired(fetched_at: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let ttl = Duration::from_secs(CACHE_TTL_HOURS * 3600).as_secs();
        now.saturating_sub(fetched_at) > ttl
    }

    /// Prune expired entries from the cache
    pub fn prune(&mut self) {
        self.pypi
            .retain(|_, entry| !Self::is_expired(entry.fetched_at));
        self.npm
            .retain(|_, entry| !Self::is_expired(entry.fetched_at));
        self.crates_io
            .retain(|_, entry| !Self::is_expired(entry.fetched_at));
        self.go_proxy
            .retain(|_, entry| !Self::is_expired(entry.fetched_at));
    }
}

/// Thread-safe cached registry wrapper that implements the Registry trait.
/// Checks cache before making network requests, storing results for future lookups.
pub struct CachedRegistry<R> {
    inner: R,
    cache: Arc<Mutex<Cache>>,
    enabled: bool,
}

impl<R: Registry> CachedRegistry<R> {
    pub fn new(inner: R, cache: Arc<Mutex<Cache>>, enabled: bool) -> Self {
        Self {
            inner,
            cache,
            enabled,
        }
    }

    /// Get from cache (returns None if disabled, expired, or missing)
    fn cache_get(&self, package: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.cache.lock().ok()?.get(self.inner.name(), package)
    }

    /// Set in cache (no-op if disabled). Does NOT save to disk - caller saves once at end.
    fn cache_set(&self, package: &str, version: &str) {
        if !self.enabled {
            return;
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.set(self.inner.name(), package, version.to_string());
        }
    }
}

#[async_trait]
impl<R: Registry> Registry for CachedRegistry<R> {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        if let Some(v) = self.cache_get(package) {
            return Ok(v);
        }
        let version = self.inner.get_latest_version(package).await?;
        self.cache_set(package, &version);
        Ok(version)
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        // Pre-releases use separate cache key to avoid returning stable when pre-release needed
        let cache_key = format!("{}:prerelease", package);
        if let Some(v) = self.cache_get(&cache_key) {
            return Ok(v);
        }
        let version = self
            .inner
            .get_latest_version_including_prereleases(package)
            .await?;
        self.cache_set(&cache_key, &version);
        Ok(version)
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        // Constraint-matching uses composite key to cache per-constraint results
        let cache_key = format!("{}:match:{}", package, constraints);
        if let Some(v) = self.cache_get(&cache_key) {
            return Ok(v);
        }
        let version = self
            .inner
            .get_latest_version_matching(package, constraints)
            .await?;
        self.cache_set(&cache_key, &version);
        Ok(version)
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}
