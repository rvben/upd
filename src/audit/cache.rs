//! Disk-backed cache for OSV audit responses.
//!
//! Cache key: `(ecosystem, name, version)`. Cache value: the list of
//! `Vulnerability` records, or an empty list representing "safe" (no
//! known vulnerabilities). Entries expire after 24 hours.

use crate::audit::Vulnerability;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CACHE_TTL_HOURS: u64 = 24;

/// Composite key that uniquely identifies a package version within an ecosystem.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AuditKey {
    pub ecosystem: String,
    pub name: String,
    pub version: String,
}

impl AuditKey {
    pub fn new(ecosystem: &str, name: &str, version: &str) -> Self {
        Self {
            ecosystem: ecosystem.to_string(),
            name: name.to_string(),
            version: version.to_string(),
        }
    }
}

/// A single cached audit result for one package version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditCacheEntry {
    /// Vulnerabilities found; empty list means the package is known-safe.
    pub vulnerabilities: Vec<Vulnerability>,
    /// Unix timestamp (seconds) when this entry was fetched from OSV.
    pub fetched_at: u64,
}

/// On-disk audit cache. Keyed by `ecosystem::name::version` string to remain
/// JSON-serialisable without a custom map-key type.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AuditCache {
    #[serde(default)]
    entries: HashMap<String, AuditCacheEntry>,
}

impl AuditCache {
    /// Load the cache from disk. Returns an empty cache if the file does not
    /// exist or cannot be parsed (treated as a cache miss, not an error).
    pub fn load() -> Result<Self> {
        let path = Self::cache_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path)?;
        let cache: AuditCache = serde_json::from_str(&content)?;
        Ok(cache)
    }

    /// Persist the cache to disk, creating the parent directory if needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::cache_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }

    /// Create a shared cache instance ready for concurrent access.
    pub fn new_shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::load().unwrap_or_default()))
    }

    /// Persist a shared cache to disk.
    pub fn save_shared(cache: &Arc<Mutex<AuditCache>>) -> Result<()> {
        cache
            .lock()
            .map_err(|e| anyhow::anyhow!("Audit cache lock poisoned: {}", e))?
            .save()
    }

    /// Look up a cache entry. Returns `None` when missing or expired.
    pub fn get(&self, key: &AuditKey) -> Option<&AuditCacheEntry> {
        let k = Self::map_key(key);
        self.entries.get(&k).and_then(|entry| {
            if Self::is_expired(entry.fetched_at) {
                None
            } else {
                Some(entry)
            }
        })
    }

    /// Insert or overwrite a cache entry with the current timestamp.
    pub fn set(&mut self, key: &AuditKey, vulnerabilities: Vec<Vulnerability>) {
        let fetched_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.entries.insert(
            Self::map_key(key),
            AuditCacheEntry {
                vulnerabilities,
                fetched_at,
            },
        );
    }

    /// Returns `true` when `fetched_at` is older than the 24-hour TTL.
    pub fn is_expired(fetched_at: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let ttl = Duration::from_secs(CACHE_TTL_HOURS * 3600).as_secs();
        now.saturating_sub(fetched_at) > ttl
    }

    /// Resolve the path for `audit.json` in the cache directory.
    ///
    /// Honors `UPD_CACHE_DIR` env var, falling back to the platform-specific
    /// application cache directory (same resolution as `src/cache.rs`).
    pub fn cache_path() -> Result<PathBuf> {
        if let Ok(dir) = std::env::var("UPD_CACHE_DIR") {
            return Ok(PathBuf::from(dir).join("audit.json"));
        }
        let proj_dirs = directories::ProjectDirs::from("", "", "upd")
            .ok_or_else(|| anyhow::anyhow!("Could not determine cache directory"))?;
        Ok(proj_dirs.cache_dir().join("audit.json"))
    }

    /// Stable string key used in the on-disk HashMap.
    fn map_key(key: &AuditKey) -> String {
        format!("{}::{}::{}", key.ecosystem, key.name, key.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_vuln(id: &str) -> Vulnerability {
        Vulnerability {
            id: id.to_string(),
            summary: None,
            severity: None,
            url: None,
            fixed_version: None,
        }
    }

    fn fresh_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn expired_timestamp() -> u64 {
        // 25 hours in the past — clearly beyond the 24-hour TTL
        fresh_timestamp() - (25 * 3600)
    }

    // ── is_expired ────────────────────────────────────────────────────────────

    #[test]
    fn is_expired_returns_false_for_fresh_entry() {
        assert!(!AuditCache::is_expired(fresh_timestamp()));
    }

    #[test]
    fn is_expired_returns_true_for_stale_entry() {
        assert!(AuditCache::is_expired(expired_timestamp()));
    }

    #[test]
    fn is_expired_boundary_exactly_24h_is_not_expired() {
        // Exactly at 24h boundary: now - (24*3600) is NOT > ttl, so not expired.
        let exactly_24h_ago = fresh_timestamp() - (24 * 3600);
        assert!(!AuditCache::is_expired(exactly_24h_ago));
    }

    #[test]
    fn is_expired_one_second_past_boundary_is_expired() {
        // 24h + 1 second crosses the boundary.
        let just_past = fresh_timestamp() - (24 * 3600) - 1;
        assert!(AuditCache::is_expired(just_past));
    }

    // ── get / set ─────────────────────────────────────────────────────────────

    #[test]
    fn get_returns_none_for_missing_entry() {
        let cache = AuditCache::default();
        let key = AuditKey::new("PyPI", "requests", "2.31.0");
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn get_returns_entry_for_fresh_data() {
        let mut cache = AuditCache::default();
        let key = AuditKey::new("PyPI", "requests", "2.31.0");
        cache.set(&key, vec![make_vuln("CVE-2024-001")]);

        let entry = cache.get(&key).expect("entry should be present");
        assert_eq!(entry.vulnerabilities.len(), 1);
        assert_eq!(entry.vulnerabilities[0].id, "CVE-2024-001");
    }

    #[test]
    fn get_returns_none_for_expired_entry() {
        let mut cache = AuditCache::default();
        let key = AuditKey::new("PyPI", "requests", "2.31.0");
        let map_key = "PyPI::requests::2.31.0".to_string();
        cache.entries.insert(
            map_key,
            AuditCacheEntry {
                vulnerabilities: vec![make_vuln("CVE-OLD")],
                fetched_at: expired_timestamp(),
            },
        );
        assert!(
            cache.get(&key).is_none(),
            "expired entry must not be returned"
        );
    }

    #[test]
    fn set_overwrites_existing_entry() {
        let mut cache = AuditCache::default();
        let key = AuditKey::new("npm", "lodash", "4.17.20");
        cache.set(&key, vec![make_vuln("CVE-A")]);
        cache.set(&key, vec![make_vuln("CVE-B"), make_vuln("CVE-C")]);

        let entry = cache.get(&key).unwrap();
        assert_eq!(entry.vulnerabilities.len(), 2);
    }

    #[test]
    fn set_empty_vec_represents_known_safe() {
        let mut cache = AuditCache::default();
        let key = AuditKey::new("crates.io", "serde", "1.0.200");
        cache.set(&key, vec![]);

        let entry = cache.get(&key).expect("safe entry should be present");
        assert!(entry.vulnerabilities.is_empty());
    }

    // ── serialization round-trip ───────────────────────────────────────────────

    #[test]
    fn round_trip_serialize_deserialize() {
        let mut cache = AuditCache::default();
        let key = AuditKey::new("PyPI", "django", "3.2.0");
        cache.set(
            &key,
            vec![Vulnerability {
                id: "GHSA-test".to_string(),
                summary: Some("Test".to_string()),
                severity: Some("High".to_string()),
                url: Some("https://example.com".to_string()),
                fixed_version: Some("3.2.1".to_string()),
            }],
        );

        let json = serde_json::to_string(&cache).unwrap();
        let restored: AuditCache = serde_json::from_str(&json).unwrap();

        let entry = restored.get(&key).expect("entry must survive round-trip");
        assert_eq!(entry.vulnerabilities.len(), 1);
        assert_eq!(entry.vulnerabilities[0].id, "GHSA-test");
        assert_eq!(
            entry.vulnerabilities[0].fixed_version.as_deref(),
            Some("3.2.1")
        );
    }

    // ── file I/O ──────────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn load_missing_file_returns_default() {
        use tempfile::tempdir;
        let original = std::env::var("UPD_CACHE_DIR").ok();

        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("audit-cache-test");
        unsafe {
            std::env::set_var("UPD_CACHE_DIR", &dir);
        }

        let cache = AuditCache::load().unwrap();
        assert!(cache.entries.is_empty());

        unsafe {
            match original {
                Some(v) => std::env::set_var("UPD_CACHE_DIR", v),
                None => std::env::remove_var("UPD_CACHE_DIR"),
            }
        }
    }

    #[test]
    #[serial]
    fn save_and_load_roundtrip_via_disk() {
        use tempfile::tempdir;
        let original = std::env::var("UPD_CACHE_DIR").ok();

        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("audit-save-test");
        unsafe {
            std::env::set_var("UPD_CACHE_DIR", &dir);
        }

        let mut cache = AuditCache::default();
        let key = AuditKey::new("Go", "golang.org/x/net", "v0.0.1");
        cache.set(&key, vec![make_vuln("GO-2024-001")]);
        cache.save().unwrap();

        let loaded = AuditCache::load().unwrap();
        let entry = loaded.get(&key).expect("entry must be loadable from disk");
        assert_eq!(entry.vulnerabilities[0].id, "GO-2024-001");

        unsafe {
            match original {
                Some(v) => std::env::set_var("UPD_CACHE_DIR", v),
                None => std::env::remove_var("UPD_CACHE_DIR"),
            }
        }
    }
}
