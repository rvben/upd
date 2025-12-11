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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_cache_get_set() {
        let mut cache = Cache::default();

        // Initially empty
        assert!(cache.get("pypi", "requests").is_none());

        // Set and retrieve
        cache.set("pypi", "requests", "2.31.0".to_string());
        assert_eq!(cache.get("pypi", "requests"), Some("2.31.0".to_string()));

        // Different registries are isolated
        assert!(cache.get("npm", "requests").is_none());

        // Set for different registries
        cache.set("npm", "lodash", "4.17.21".to_string());
        cache.set("crates.io", "serde", "1.0.200".to_string());
        cache.set("go-proxy", "golang.org/x/sync", "v0.7.0".to_string());

        assert_eq!(cache.get("npm", "lodash"), Some("4.17.21".to_string()));
        assert_eq!(cache.get("crates.io", "serde"), Some("1.0.200".to_string()));
        assert_eq!(
            cache.get("go-proxy", "golang.org/x/sync"),
            Some("v0.7.0".to_string())
        );
    }

    #[test]
    fn test_cache_expiration() {
        let mut cache = Cache::default();

        // Set with current timestamp
        cache.set("pypi", "fresh", "1.0.0".to_string());

        // Should be retrievable (not expired)
        assert_eq!(cache.get("pypi", "fresh"), Some("1.0.0".to_string()));

        // Manually insert an expired entry (25 hours ago)
        let expired_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - (25 * 3600);

        cache.pypi.insert(
            "old".to_string(),
            CacheEntry {
                version: "0.1.0".to_string(),
                fetched_at: expired_time,
            },
        );

        // Expired entry should return None
        assert!(cache.get("pypi", "old").is_none());
    }

    #[test]
    fn test_cache_prune() {
        let mut cache = Cache::default();

        // Add fresh entry
        cache.set("pypi", "fresh", "1.0.0".to_string());

        // Add expired entry
        let expired_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - (25 * 3600);

        cache.pypi.insert(
            "old".to_string(),
            CacheEntry {
                version: "0.1.0".to_string(),
                fetched_at: expired_time,
            },
        );

        // Before prune: both entries exist in the map
        assert!(cache.pypi.contains_key("fresh"));
        assert!(cache.pypi.contains_key("old"));

        // Prune removes expired entries
        cache.prune();

        assert!(cache.pypi.contains_key("fresh"));
        assert!(!cache.pypi.contains_key("old"));
    }

    #[test]
    fn test_cache_unknown_registry() {
        let mut cache = Cache::default();

        // Unknown registry returns None
        assert!(cache.get("unknown", "package").is_none());

        // Setting for unknown registry is a no-op
        cache.set("unknown", "package", "1.0.0".to_string());
        assert!(cache.get("unknown", "package").is_none());
    }

    #[test]
    fn test_shared_cache() {
        let cache = Cache::new_shared();

        // Set value through lock
        {
            let mut c = cache.lock().unwrap();
            c.set("npm", "react", "18.2.0".to_string());
        }

        // Retrieve through a new lock
        {
            let c = cache.lock().unwrap();
            assert_eq!(c.get("npm", "react"), Some("18.2.0".to_string()));
        }
    }

    #[test]
    fn test_cache_serialization() {
        let mut cache = Cache::default();
        cache.set("pypi", "requests", "2.31.0".to_string());
        cache.set("npm", "lodash", "4.17.21".to_string());

        // Serialize to JSON
        let json = serde_json::to_string(&cache).unwrap();

        // Deserialize back
        let restored: Cache = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.get("pypi", "requests"), Some("2.31.0".to_string()));
        assert_eq!(restored.get("npm", "lodash"), Some("4.17.21".to_string()));
    }

    #[test]
    fn test_cache_file_operations() {
        use tempfile::tempdir;

        // Use a temp directory for cache
        let temp = tempdir().unwrap();
        let cache_dir = temp.path().join("upd-test-cache");
        // SAFETY: This is a single-threaded test and we restore the var at the end
        unsafe {
            std::env::set_var("UPD_CACHE_DIR", &cache_dir);
        }

        // Initially cache doesn't exist, load returns default
        let cache = Cache::load().unwrap();
        assert!(cache.pypi.is_empty());

        // Save cache with data
        let mut cache = Cache::default();
        cache.set("pypi", "test-pkg", "1.0.0".to_string());
        cache.save().unwrap();

        // Reload and verify
        let loaded = Cache::load().unwrap();
        assert_eq!(loaded.get("pypi", "test-pkg"), Some("1.0.0".to_string()));

        // Clean cache
        Cache::clean().unwrap();

        // After clean, load returns default
        let after_clean = Cache::load().unwrap();
        assert!(after_clean.pypi.is_empty());

        // Clean up
        // SAFETY: Restoring the environment variable after test
        unsafe {
            std::env::remove_var("UPD_CACHE_DIR");
        }
    }

    #[tokio::test]
    async fn test_cached_registry_caches_results() {
        use crate::registry::MockRegistry;

        let mock = MockRegistry::new("pypi").with_version("flask", "3.0.0");
        let cache = Cache::new_shared();
        let cached = CachedRegistry::new(mock, cache.clone(), true);

        // First call - not in cache, should fetch from registry
        let version = cached.get_latest_version("flask").await.unwrap();
        assert_eq!(version, "3.0.0");

        // Verify it was cached
        let c = cache.lock().unwrap();
        assert_eq!(c.get("pypi", "flask"), Some("3.0.0".to_string()));
    }

    #[tokio::test]
    async fn test_cached_registry_returns_cached_value() {
        use crate::registry::MockRegistry;

        // Pre-populate cache
        let cache = Cache::new_shared();
        {
            let mut c = cache.lock().unwrap();
            c.set("pypi", "requests", "2.31.0".to_string());
        }

        // Create registry WITHOUT the package - only cache has it
        let mock = MockRegistry::new("pypi");
        let cached = CachedRegistry::new(mock, cache, true);

        // Should return cached value without hitting registry
        let version = cached.get_latest_version("requests").await.unwrap();
        assert_eq!(version, "2.31.0");
    }

    #[tokio::test]
    async fn test_cached_registry_disabled() {
        use crate::registry::MockRegistry;

        // Use a unique package name to avoid interference from other tests
        // that may have populated the shared cache
        let unique_pkg = "test-pkg-disabled-cache-xyz";
        let mock = MockRegistry::new("pypi").with_version(unique_pkg, "5.0.0");

        // Create a fresh cache (not shared) to ensure test isolation
        let cache = Arc::new(Mutex::new(Cache::default()));

        // Create cached registry with caching DISABLED
        let cached = CachedRegistry::new(mock, cache.clone(), false);

        // Should fetch from registry
        let version = cached.get_latest_version(unique_pkg).await.unwrap();
        assert_eq!(version, "5.0.0");

        // Cache should NOT be populated when disabled
        let c = cache.lock().unwrap();
        assert!(c.get("pypi", unique_pkg).is_none());
    }

    #[tokio::test]
    async fn test_cached_registry_prerelease_separate_cache() {
        use crate::registry::MockRegistry;

        let mock = MockRegistry::new("pypi")
            .with_version("ty", "1.0.0")
            .with_prerelease("ty", "1.0.0", "1.1.0a5");
        let cache = Cache::new_shared();
        let cached = CachedRegistry::new(mock, cache.clone(), true);

        // Fetch stable version
        let stable = cached.get_latest_version("ty").await.unwrap();
        assert_eq!(stable, "1.0.0");

        // Fetch prerelease version (should use separate cache key)
        let prerelease = cached
            .get_latest_version_including_prereleases("ty")
            .await
            .unwrap();
        assert_eq!(prerelease, "1.1.0a5");

        // Both should be cached separately
        let c = cache.lock().unwrap();
        assert_eq!(c.get("pypi", "ty"), Some("1.0.0".to_string()));
        assert_eq!(c.get("pypi", "ty:prerelease"), Some("1.1.0a5".to_string()));
    }

    #[tokio::test]
    async fn test_cached_registry_constraint_matching() {
        use crate::registry::MockRegistry;

        let mock = MockRegistry::new("pypi")
            .with_version("click", "8.1.7")
            .with_constrained("click", ">=7.0,<8.0", "7.1.2");
        let cache = Cache::new_shared();
        let cached = CachedRegistry::new(mock, cache.clone(), true);

        // Fetch with constraints
        let constrained = cached
            .get_latest_version_matching("click", ">=7.0,<8.0")
            .await
            .unwrap();
        assert_eq!(constrained, "7.1.2");

        // Should be cached with constraint key
        let c = cache.lock().unwrap();
        assert_eq!(
            c.get("pypi", "click:match:>=7.0,<8.0"),
            Some("7.1.2".to_string())
        );
    }

    #[test]
    fn test_cached_registry_name() {
        use crate::registry::MockRegistry;

        let mock = MockRegistry::new("npm");
        let cache = Cache::new_shared();
        let cached = CachedRegistry::new(mock, cache, true);

        assert_eq!(cached.name(), "npm");
    }
}
