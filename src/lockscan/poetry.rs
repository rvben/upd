//! poetry.lock reader: `[[package]]` entries from PyPI or a legacy
//! (alternate registry index) source.

use super::{LockScan, LockedPackage, find_entry_line};
use crate::audit::Ecosystem;
use anyhow::{Context, Result};
use std::path::Path;

/// Scan a poetry.lock. Entries with no `[package.source]` table resolve from
/// the default registry (PyPI); `type = "legacy"` is an alternate registry
/// index. `git`/`directory`/`file`/`url` sources are not registry packages
/// and are excluded.
pub fn scan_poetry_lock(path: &Path) -> Result<LockScan> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc: toml::Table = content
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;

    let mut scan = LockScan::default();
    let Some(packages) = doc.get("package").and_then(|p| p.as_array()) else {
        return Ok(scan);
    };
    for entry in packages {
        let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(version) = entry.get("version").and_then(|v| v.as_str()) else {
            continue;
        };
        let registry_like = match entry.get("source").and_then(|s| s.as_table()) {
            None => true,
            Some(source) => source
                .get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t == "legacy"),
        };
        if !registry_like {
            continue;
        }
        scan.packages.push(LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ecosystem: Ecosystem::PyPI,
            lockfile_path: path.to_path_buf(),
            line_number: find_entry_line(&content, &format!("name = \"{name}\"")),
        });
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::Ecosystem;

    const POETRY_LOCK: &str = r#"
[[package]]
name = "examplepkg"
version = "2.0.5"
description = "test"
optional = false
python-versions = ">=3.8"

[[package]]
name = "legacypkg"
version = "1.1.0"
description = "alt index"
optional = false
python-versions = ">=3.8"

[package.source]
type = "legacy"
url = "https://pypi.org/simple"
reference = "pypi"

[[package]]
name = "gitpkg"
version = "0.9.0"
description = "from git"
optional = false
python-versions = ">=3.8"

[package.source]
type = "git"
url = "https://github.com/example/gitpkg"
reference = "main"
resolved_reference = "abc123"

[[package]]
name = "dirpkg"
version = "0.1.0"
description = "local"
optional = false
python-versions = ">=3.8"

[package.source]
type = "directory"
url = "../dirpkg"

[metadata]
lock-version = "2.0"
python-versions = ">=3.8"
content-hash = "0000"
"#;

    fn write_lock(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("poetry.lock");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn poetry_reader_includes_default_and_legacy_sources_only() {
        let (_dir, path) = write_lock(POETRY_LOCK);
        let scan = scan_poetry_lock(&path).unwrap();
        let names: Vec<&str> = scan.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["examplepkg", "legacypkg"],
            "git/directory excluded"
        );
        assert!(scan.packages.iter().all(|p| p.ecosystem == Ecosystem::PyPI));
        assert!(scan.packages.iter().all(|p| p.line_number.is_some()));
    }

    #[test]
    fn poetry_reader_excludes_file_and_url_sources() {
        let lock = "[[package]]\nname = \"filepkg\"\nversion = \"1.0.0\"\noptional = false\npython-versions = \"*\"\n\n[package.source]\ntype = \"file\"\nurl = \"pkg.whl\"\n";
        let (_dir, path) = write_lock(lock);
        let scan = scan_poetry_lock(&path).unwrap();
        assert!(scan.packages.is_empty());
    }
}
