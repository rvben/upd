use anyhow::Result;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
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

/// A cached registry wrapper that checks cache before making network requests
pub struct CachedRegistry<R> {
    inner: R,
    cache: Cache,
    registry_name: &'static str,
    enabled: bool,
}

impl<R: crate::registry::Registry> CachedRegistry<R> {
    pub fn new(inner: R, enabled: bool) -> Self {
        let cache = Cache::load().unwrap_or_default();
        let registry_name = inner.name();

        Self {
            inner,
            cache,
            registry_name,
            enabled,
        }
    }

    pub async fn get_latest_version(&mut self, package: &str) -> anyhow::Result<String> {
        // Check cache first if enabled
        if self.enabled
            && let Some(version) = self.cache.get(self.registry_name, package)
        {
            return Ok(version);
        }

        // Fetch from registry
        let version = self.inner.get_latest_version(package).await?;

        // Update cache if enabled
        if self.enabled {
            self.cache.set(self.registry_name, package, version.clone());
            // Best effort save - don't fail the operation if cache save fails
            let _ = self.cache.save();
        }

        Ok(version)
    }
}
