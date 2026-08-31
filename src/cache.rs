use crate::registry::{Registry, TagsAtCommit, VersionMeta, VersionQuery};
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
    #[serde(default, rename = "github-releases")]
    github_releases: HashMap<String, CacheEntry>,
    #[serde(default)]
    rubygems: HashMap<String, CacheEntry>,
    #[serde(default)]
    terraform: HashMap<String, CacheEntry>,
    #[serde(default)]
    nuget: HashMap<String, CacheEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CacheEntry {
    pub version: String,
    pub fetched_at: u64, // Unix timestamp
    /// Full per-version metadata fetched via `list_versions`, when available.
    /// Older cache files predate this field and deserialize with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<Vec<CachedVersionMeta>>,
}

/// Cache-friendly mirror of [`crate::registry::VersionMeta`]. `published_at`
/// is stored as a Unix timestamp (seconds) so the cache file stays stable
/// across `chrono` serde format changes.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CachedVersionMeta {
    pub version: String,
    pub published_at: Option<i64>,
    pub yanked: bool,
    pub prerelease: bool,
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
            "github-releases" => &self.github_releases,
            "rubygems" => &self.rubygems,
            "terraform" => &self.terraform,
            "nuget" => &self.nuget,
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
            "github-releases" => &mut self.github_releases,
            "rubygems" => &mut self.rubygems,
            "terraform" => &mut self.terraform,
            "nuget" => &mut self.nuget,
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
                versions: None,
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
        self.github_releases
            .retain(|_, entry| !Self::is_expired(entry.fetched_at));
        self.rubygems
            .retain(|_, entry| !Self::is_expired(entry.fetched_at));
        self.terraform
            .retain(|_, entry| !Self::is_expired(entry.fetched_at));
        self.nuget
            .retain(|_, entry| !Self::is_expired(entry.fetched_at));
    }
}

/// Thread-safe cached registry wrapper that implements the Registry trait.
/// Checks cache before making network requests, storing results for future lookups.
pub struct CachedRegistry<R> {
    inner: R,
    cache: Arc<Mutex<Cache>>,
    enabled: bool,
    namespace: Option<String>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    revalidated: Mutex<HashMap<String, String>>,
}

impl<R: Registry> CachedRegistry<R> {
    pub fn new(inner: R, cache: Arc<Mutex<Cache>>, enabled: bool) -> Self {
        Self {
            inner,
            cache,
            enabled,
            namespace: None,
            refresh_locks: Mutex::new(HashMap::new()),
            revalidated: Mutex::new(HashMap::new()),
        }
    }

    /// Use a registry-configuration-specific cache namespace.
    ///
    /// Registries in the same ecosystem can serve different releases for the
    /// same package. Keeping their entries separate prevents a result fetched
    /// from one private index (or index chain) being reused for another.
    pub fn with_namespace(
        inner: R,
        cache: Arc<Mutex<Cache>>,
        enabled: bool,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            cache,
            enabled,
            namespace: Some(namespace.into()),
            refresh_locks: Mutex::new(HashMap::new()),
            revalidated: Mutex::new(HashMap::new()),
        }
    }

    fn scoped_key(&self, key: &str) -> String {
        self.namespace.as_ref().map_or_else(
            || key.to_string(),
            |namespace| format!("{namespace}\u{1f}{key}"),
        )
    }

    /// Get from cache (returns None if disabled, expired, or missing)
    fn cache_get(&self, package: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let key = self.scoped_key(package);
        self.cache.lock().ok()?.get(self.inner.name(), &key)
    }

    /// Set in cache (no-op if disabled). Does NOT save to disk - caller saves once at end.
    fn cache_set(&self, package: &str, version: &str) {
        if !self.enabled {
            return;
        }
        let key = self.scoped_key(package);
        if let Ok(mut cache) = self.cache.lock() {
            cache.set(self.inner.name(), &key, version.to_string());
        }
    }

    fn refresh_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .refresh_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            locks
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
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

    async fn revalidate_version(
        &self,
        package: &str,
        query: VersionQuery<'_>,
        stale_version: &str,
    ) -> Result<String> {
        if !self.enabled {
            return self
                .inner
                .revalidate_version(package, query, stale_version)
                .await;
        }

        let cache_key = query.cache_key(package);
        let refresh_key = format!("{}\u{1f}{stale_version}", self.scoped_key(&cache_key));
        if let Some(version) = self
            .revalidated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&refresh_key)
            .cloned()
        {
            return Ok(version);
        }

        // File updates run concurrently. Serialise this exceptional refresh so
        // one stale package appearing in many manifests produces one live
        // request, then let every waiter reuse its answer.
        let refresh_lock = self.refresh_lock(&refresh_key);
        let _guard = refresh_lock.lock().await;

        if let Some(version) = self
            .revalidated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&refresh_key)
            .cloned()
        {
            return Ok(version);
        }

        // Another ordinary lookup may have replaced the stale entry while this
        // task waited. That answer is already live enough for this run.
        if let Some(version) = self.cache_get(&cache_key)
            && version != stale_version
        {
            self.revalidated
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(refresh_key, version.clone());
            return Ok(version);
        }

        let version = self
            .inner
            .revalidate_version(package, query, stale_version)
            .await?;
        self.cache_set(&cache_key, &version);
        self.revalidated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(refresh_key, version.clone());
        Ok(version)
    }

    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        self.inner.list_versions(package).await
    }

    /// Forwarded, not cached: the version cache stores a single version string
    /// per key, and the callers already deduplicate ref lookups per repo within
    /// a run. Forwarding is not optional - a method left to the trait default
    /// here silently answers "no refs" for every wrapped registry, which reads
    /// as a definitive answer rather than a missing implementation.
    async fn list_ref_names(&self, package: &str) -> Result<Vec<String>> {
        self.inner.list_ref_names(package).await
    }

    /// Forwarded without caching. Ref resolution is security-sensitive and a
    /// tag may be moved between runs; the SHA-pin updater must observe GitHub's
    /// current answer before it writes anything.
    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        self.inner.resolve_ref_to_commit(package, reference).await
    }

    /// Forwarded without caching, for the same reason as `resolve_ref_to_commit`:
    /// the answer decides which release a SHA pin is annotated with, and a tag
    /// can be moved onto or off a commit between runs.
    async fn tags_at_commit(&self, package: &str, commit: &str) -> Result<TagsAtCommit> {
        self.inner.tags_at_commit(package, commit).await
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
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

        cache.set("github-releases", "actions/checkout", "v4.2.0".to_string());
        assert_eq!(
            cache.get("github-releases", "actions/checkout"),
            Some("v4.2.0".to_string())
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
                versions: None,
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
                versions: None,
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
    fn test_cache_entry_deserialises_without_versions_field() {
        // Older cache files predate the `versions` field and must still
        // deserialise so upgrades do not invalidate on-disk caches.
        let json = r#"{"version":"1.2.3","fetched_at":1700000000}"#;
        let entry: CacheEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.version, "1.2.3");
        assert_eq!(entry.fetched_at, 1_700_000_000);
        assert!(entry.versions.is_none(), "missing field defaults to None");
    }

    #[test]
    fn test_cache_roundtrip_with_versions() {
        let entry = CacheEntry {
            version: "2.0.0".to_string(),
            fetched_at: 1_700_000_000,
            versions: Some(vec![
                CachedVersionMeta {
                    version: "2.0.0".to_string(),
                    published_at: Some(1_700_000_000),
                    yanked: false,
                    prerelease: false,
                },
                CachedVersionMeta {
                    version: "1.9.0".to_string(),
                    published_at: None,
                    yanked: true,
                    prerelease: false,
                },
            ]),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: CacheEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, entry.version);
        let metas = back.versions.expect("versions must round-trip");
        assert_eq!(metas.len(), 2);
        assert!(metas.iter().any(|m| m.yanked));
        assert!(
            metas
                .iter()
                .any(|m| m.version == "2.0.0" && m.published_at == Some(1_700_000_000))
        );
    }

    #[test]
    fn test_cache_entry_skips_serializing_none_versions() {
        // Ensure the new field is omitted from JSON when absent, keeping
        // file size small and preserving compatibility with older readers.
        let entry = CacheEntry {
            version: "1.0.0".to_string(),
            fetched_at: 1_700_000_000,
            versions: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("versions"),
            "versions field must be omitted when None, got: {json}"
        );
    }

    #[test]
    #[serial]
    fn test_cache_file_operations() {
        use tempfile::tempdir;

        // Capture the current value so we can restore it after the test.
        // `#[serial]` makes this mutually exclusive with every other env-mutating
        // test in the crate, so the process-global UPD_CACHE_DIR write below
        // cannot race under a parallel `cargo test`.
        let original_cache_dir = std::env::var("UPD_CACHE_DIR").ok();

        // Use a temp directory for cache
        let temp = tempdir().unwrap();
        let cache_dir = temp.path().join("upd-test-cache");
        // SAFETY: `#[serial]` guarantees no other env-touching test runs
        // concurrently. The original value is restored unconditionally in the
        // cleanup block below.
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

        // Restore the environment to the state before this test ran.
        // SAFETY: Same `#[serial]` exclusivity as the set above.
        unsafe {
            match original_cache_dir {
                Some(val) => std::env::set_var("UPD_CACHE_DIR", val),
                None => std::env::remove_var("UPD_CACHE_DIR"),
            }
        }
    }

    #[tokio::test]
    async fn test_cached_registry_caches_results() {
        use crate::registry::MockRegistry;
        use std::sync::Mutex;

        let mock = MockRegistry::new("pypi").with_version("flask", "3.0.0");
        // Use an in-memory empty cache so the test never reads from disk.
        let cache: Arc<Mutex<Cache>> = Arc::new(Mutex::new(Cache::default()));
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
    async fn cache_namespaces_separate_same_named_registries() {
        use crate::registry::MockRegistry;

        let cache = Arc::new(Mutex::new(Cache::default()));
        let first = CachedRegistry::with_namespace(
            MockRegistry::new("pypi").with_version("shared-name", "1.0.0"),
            Arc::clone(&cache),
            true,
            "private-a",
        );
        let second = CachedRegistry::with_namespace(
            MockRegistry::new("pypi").with_version("shared-name", "2.0.0"),
            Arc::clone(&cache),
            true,
            "private-b",
        );

        assert_eq!(
            first.get_latest_version("shared-name").await.unwrap(),
            "1.0.0"
        );
        assert_eq!(
            second.get_latest_version("shared-name").await.unwrap(),
            "2.0.0",
            "a package answer from one index must not leak into another"
        );
    }

    #[tokio::test]
    async fn concurrent_revalidation_fetches_once_and_replaces_stale_entry() {
        use crate::registry::{
            TagsAtCommit, no_ref_names, no_version_metadata, ref_resolution_unsupported,
            tags_at_commit_unsupported,
        };
        use futures::future::join_all;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingRegistry {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl Registry for CountingRegistry {
            async fn get_latest_version(&self, _package: &str) -> Result<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok("1.1.2".to_string())
            }

            async fn list_versions(&self, _package: &str) -> Result<Vec<VersionMeta>> {
                no_version_metadata()
            }

            async fn list_ref_names(&self, _package: &str) -> Result<Vec<String>> {
                no_ref_names()
            }

            async fn resolve_ref_to_commit(
                &self,
                package: &str,
                reference: &str,
            ) -> Result<String> {
                Err(ref_resolution_unsupported(self.name(), package, reference))
            }

            async fn tags_at_commit(&self, _package: &str, _commit: &str) -> Result<TagsAtCommit> {
                tags_at_commit_unsupported()
            }

            fn name(&self) -> &'static str {
                "pypi"
            }
        }

        let cache = Arc::new(Mutex::new(Cache::default()));
        cache
            .lock()
            .unwrap()
            .set("pypi", "private-package", "1.1.0".to_string());
        let calls = Arc::new(AtomicUsize::new(0));
        let cached = CachedRegistry::new(
            CountingRegistry {
                calls: Arc::clone(&calls),
            },
            Arc::clone(&cache),
            true,
        );

        let results =
            join_all((0..18).map(|_| {
                cached.revalidate_version("private-package", VersionQuery::Stable, "1.1.0")
            }))
            .await;

        assert!(
            results
                .iter()
                .all(|result| matches!(result, Ok(version) if version == "1.1.2"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            cache.lock().unwrap().get("pypi", "private-package"),
            Some("1.1.2".to_string())
        );
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

/// Every capability method `CachedRegistry` forwards rather than caches. A
/// forward that answers on its own behalf returns a plausible "nothing here"
/// that no caller can distinguish from the real registry's answer, so each one
/// is pinned by a test that fails when the call stops reaching the inner
/// registry.
#[cfg(test)]
mod forwarding_tests {
    use super::*;
    use crate::registry::GitHubReleasesRegistry;
    use crate::registry::MockRegistry;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// CachedRegistry decorates another registry, so every Registry method has
    /// to be forwarded explicitly. A method left to the trait default answers
    /// with the default's "no data" value, which downstream code cannot
    /// distinguish from a real answer - this shipped once as a floating-ref
    /// check that silently never ran because the wrapper swallowed it.
    #[tokio::test]
    async fn cached_registry_forwards_ref_names() {
        let inner = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v4.2.0")
            .with_ref_names("actions/checkout", &["v4.2.0", "v4", "v3"]);
        let cached = CachedRegistry::new(inner, Cache::new_shared(), false);

        let refs = cached.list_ref_names("actions/checkout").await.unwrap();
        assert_eq!(
            refs,
            vec!["v4.2.0", "v4", "v3"],
            "CachedRegistry must forward list_ref_names to the inner registry"
        );
    }

    #[tokio::test]
    async fn cached_registry_forwards_ref_resolution() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let inner = MockRegistry::new("github-releases").with_resolved_ref(
            "actions/checkout",
            "v4.2.2",
            sha,
        );
        let cached = CachedRegistry::new(inner, Cache::new_shared(), true);

        assert_eq!(
            cached
                .resolve_ref_to_commit("actions/checkout", "v4.2.2")
                .await
                .unwrap(),
            sha
        );
    }

    /// The two tests above inject a `MockRegistry`, which cannot see a wiring
    /// bug: it answers whether or not a request was ever made. This one wraps
    /// the production registry over a mock HTTP server, so passing requires the
    /// call to travel through the decorator and out onto the wire. `expect(1)`,
    /// verified when the server drops, is what makes a silent no-op fail: a
    /// method that never asks the inner registry reaches no endpoint.
    ///
    /// `list_versions` is here because it is forwarded uncached and the
    /// cooldown layer reads its empty answer as "this registry publishes no
    /// dates". A forward that returned an empty list of its own would disable
    /// cooldown everywhere and report it as a registry limitation.
    #[tokio::test]
    async fn cached_registry_lookups_reach_the_http_layer() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"tag_name": "v4.2.2", "published_at": "2026-01-02T03:04:05Z", "draft": false, "prerelease": false}]"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"[{"name": "v4.2.2"}, {"name": "v4"}]"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/commits/v4"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(format!(r#"{{"sha": "{sha}"}}"#)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let cached = CachedRegistry::new(
            GitHubReleasesRegistry::with_api_url(server.uri()),
            Cache::new_shared(),
            true,
        );

        assert_eq!(
            cached.list_ref_names("actions/checkout").await.unwrap(),
            vec!["v4.2.2", "v4"],
            "the decorator must return what the HTTP layer served"
        );
        assert_eq!(
            cached
                .resolve_ref_to_commit("actions/checkout", "v4")
                .await
                .unwrap(),
            sha
        );

        let versions = cached.list_versions("actions/checkout").await.unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|v| v.version.as_str())
                .collect::<Vec<_>>(),
            vec!["v4.2.2"],
            "the decorator must return what the HTTP layer served"
        );
        assert!(
            versions[0].published_at.is_some(),
            "the publish date cooldown depends on must survive the decorator"
        );
    }

    #[tokio::test]
    async fn cached_registry_forwards_commit_tag_lookup() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let inner = MockRegistry::new("github-releases")
            .with_resolved_ref("actions/checkout", "v4.2.2", sha)
            .with_resolved_ref("actions/checkout", "v4", sha);
        let cached = CachedRegistry::new(inner, Cache::new_shared(), true);

        assert_eq!(
            cached
                .tags_at_commit("actions/checkout", sha)
                .await
                .unwrap(),
            TagsAtCommit::Known(vec!["v4".to_string(), "v4.2.2".to_string()]),
            "CachedRegistry must forward tags_at_commit to the inner registry"
        );
    }

    /// The companion to the mock test above: this one reaches the wire, so a
    /// forward that quietly answers on its own behalf leaves `expect(1)`
    /// unsatisfied and fails when the server drops. Without it, a `tags_at_commit`
    /// that never consulted the inner registry would return `Known(vec![])` and
    /// every SHA pin in the fleet would report as belonging to no release.
    #[tokio::test]
    async fn cached_registry_commit_tag_lookup_reaches_the_http_layer() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let other = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"[{{"name": "v4.2.2", "commit": {{"sha": "{sha}"}}}},
                    {{"name": "v4", "commit": {{"sha": "{sha}"}}}},
                    {{"name": "v4.2.1", "commit": {{"sha": "{other}"}}}}]"#
            )))
            .expect(1)
            .mount(&server)
            .await;

        let cached = CachedRegistry::new(
            GitHubReleasesRegistry::with_api_url(server.uri()),
            Cache::new_shared(),
            true,
        );

        assert_eq!(
            cached
                .tags_at_commit("actions/checkout", sha)
                .await
                .unwrap(),
            TagsAtCommit::Known(vec!["v4.2.2".to_string(), "v4".to_string()]),
            "the decorator must return the tags the HTTP layer served for this commit"
        );
    }
}
