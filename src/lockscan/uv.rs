//! uv.lock reader: `[[package]]` entries whose `source` is a registry.

use super::{LockScan, LockedPackage, index_name_lines};
use crate::audit::Ecosystem;
use anyhow::{Context, Result};
use std::path::Path;

/// Scan a uv.lock, returning its registry-resolved packages. Entries whose
/// `source` is virtual/editable/directory/path/git/url are local or
/// non-registry code and are excluded; entries with no `source` at all are
/// excluded defensively rather than guessed.
pub fn scan_uv_lock(path: &Path) -> Result<LockScan> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc: toml::Table = content
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;

    let mut scan = LockScan::default();
    let Some(packages) = doc.get("package").and_then(|p| p.as_array()) else {
        return Ok(scan);
    };
    let name_lines = index_name_lines(&content);
    for entry in packages {
        let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(version) = entry.get("version").and_then(|v| v.as_str()) else {
            continue;
        };
        let is_registry = entry
            .get("source")
            .and_then(|s| s.as_table())
            .is_some_and(|t| t.contains_key("registry"));
        if !is_registry {
            continue;
        }
        scan.packages.push(LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ecosystem: Ecosystem::PyPI,
            lockfile_path: path.to_path_buf(),
            line_number: name_lines.get(name).copied(),
        });
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::Ecosystem;

    const UV_LOCK: &str = r#"
version = 1
requires-python = ">=3.12"

[[package]]
name = "myproject"
version = "1.0.0"
source = { virtual = "." }
dependencies = [
    { name = "examplepkg" },
]

[[package]]
name = "examplepkg"
version = "2.0.5"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "localdep"
version = "0.1.0"
source = { directory = "../localdep" }

[[package]]
name = "gitdep"
version = "0.2.0"
source = { git = "https://github.com/example/gitdep?rev=abc123#abc123" }

[[package]]
name = "editdep"
version = "0.3.0"
source = { editable = "pkgs/editdep" }
"#;

    fn write_lock(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uv.lock");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn uv_reader_includes_only_registry_packages() {
        let (_dir, path) = write_lock(UV_LOCK);
        let scan = scan_uv_lock(&path).unwrap();
        let names: Vec<&str> = scan.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["examplepkg"],
            "virtual/directory/git/editable excluded"
        );
        let pkg = &scan.packages[0];
        assert_eq!(pkg.version, "2.0.5");
        assert_eq!(pkg.ecosystem, Ecosystem::PyPI);
        assert_eq!(pkg.lockfile_path, path);
        assert!(scan.warnings.is_empty());
    }

    #[test]
    fn uv_reader_records_entry_line() {
        let (_dir, path) = write_lock(UV_LOCK);
        let scan = scan_uv_lock(&path).unwrap();
        let line = scan.packages[0].line_number.expect("line recorded");
        // Line 14 of UV_LOCK is the examplepkg entry's own `name = "examplepkg"`
        // key (inside its `[[package]]` block). The dependency reference
        // `{ name = "examplepkg" }` on line 10 (inside myproject's
        // `dependencies` array) must NOT be the anchor.
        assert_eq!(line, 14);
    }

    #[test]
    fn uv_reader_malformed_toml_is_an_error() {
        let (_dir, path) = write_lock("not [ valid toml");
        assert!(scan_uv_lock(&path).is_err());
    }

    #[test]
    fn uv_reader_missing_source_is_excluded() {
        // A package entry with no source table cannot be attributed to a
        // registry; defensively excluded rather than guessed.
        let (_dir, path) =
            write_lock("version = 1\n\n[[package]]\nname = \"nosource\"\nversion = \"1.0.0\"\n");
        let scan = scan_uv_lock(&path).unwrap();
        assert!(scan.packages.is_empty());
    }
}
