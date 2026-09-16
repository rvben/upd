//! poetry.lock reader: `[[package]]` entries from PyPI or a legacy
//! (alternate registry index) source.

use super::{LockScan, LockedPackage, index_name_lines};
use crate::audit::Ecosystem;
use anyhow::{Context, Result};
use std::path::Path;

/// Scan a poetry.lock. Entries with no `[package.source]` table resolve from
/// the default registry (PyPI); `type = "legacy"` is an alternate registry
/// index. `git`/`directory`/`file`/`url` sources are not registry packages
/// and are excluded.
pub fn scan_poetry_lock(path: &Path) -> Result<LockScan> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", crate::path_display::display_path(path)))?;
    parse_poetry_lock(path, &content)
}

/// Read poetry.lock `content`, attributing its packages to `path`.
pub(crate) fn parse_poetry_lock(path: &Path, content: &str) -> Result<LockScan> {
    let doc: toml::Table = content
        .parse()
        .with_context(|| format!("parsing {}", crate::path_display::display_path(path)))?;

    let mut scan = LockScan::default();
    let Some(packages) = doc.get("package").and_then(|p| p.as_array()) else {
        return Ok(scan);
    };
    let name_lines = index_name_lines(content);
    for entry in packages {
        let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(version) = entry.get("version").and_then(|v| v.as_str()) else {
            continue;
        };
        let index = match entry.get("source").and_then(|s| s.as_table()) {
            None => None,
            Some(source) if source.get("type").and_then(|t| t.as_str()) == Some("legacy") => source
                .get("url")
                .and_then(|u| u.as_str())
                .map(str::to_string),
            Some(_) => continue,
        };
        scan.packages.push(LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ecosystem: Ecosystem::PyPI,
            lockfile_path: path.to_path_buf(),
            line_number: name_lines.get(name).copied(),
            locator: None,
            index,
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
        let indexes: Vec<Option<&str>> = scan.packages.iter().map(|p| p.index.as_deref()).collect();
        assert_eq!(indexes, [None, Some("https://pypi.org/simple")]);
    }

    #[test]
    fn poetry_reader_excludes_file_and_url_sources() {
        let lock = "[[package]]\nname = \"filepkg\"\nversion = \"1.0.0\"\noptional = false\npython-versions = \"*\"\n\n[package.source]\ntype = \"file\"\nurl = \"pkg.whl\"\n";
        let (_dir, path) = write_lock(lock);
        let scan = scan_poetry_lock(&path).unwrap();
        assert!(scan.packages.is_empty());
    }
}
