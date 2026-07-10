//! Cargo.lock reader: `[[package]]` entries resolved from a registry.

use super::{LockScan, LockedPackage};
use crate::audit::Ecosystem;
use anyhow::{Context, Result};
use std::path::Path;

/// Scan a Cargo.lock. Only entries whose `source` starts with `registry+`
/// are registry packages; path deps (no source) and git deps are excluded.
/// Duplicate versions of one crate are distinct entries and ALL are kept.
pub fn scan_cargo_lock(path: &Path) -> Result<LockScan> {
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
        let is_registry = entry
            .get("source")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("registry+"));
        if !is_registry {
            continue;
        }
        scan.packages.push(LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ecosystem: Ecosystem::CratesIo,
            lockfile_path: path.to_path_buf(),
            line_number: find_cargo_entry_line(&content, name, version),
        });
    }
    Ok(scan)
}

/// Line of the `name = "<name>"` entry whose block also declares
/// `version = "<version>"`. Cargo.lock holds duplicate versions of one
/// crate as separate `[[package]]` blocks; anchoring by name alone would
/// point every duplicate at the first block.
fn find_cargo_entry_line(content: &str, name: &str, version: &str) -> Option<usize> {
    let name_line = format!("name = \"{name}\"");
    let version_line = format!("version = \"{version}\"");
    let lines: Vec<&str> = content.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        if !line.contains(&name_line) {
            continue;
        }
        let block_matches = lines[idx + 1..]
            .iter()
            .take_while(|l| !l.contains("[[package]]"))
            .any(|l| l.contains(&version_line));
        if block_matches {
            return Some(idx + 1);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::Ecosystem;

    const CARGO_LOCK: &str = r#"
version = 4

[[package]]
name = "myproject"
version = "0.1.0"
dependencies = ["examplecrate"]

[[package]]
name = "examplecrate"
version = "1.2.3"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"

[[package]]
name = "examplecrate"
version = "2.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbbb"

[[package]]
name = "gitcrate"
version = "0.5.0"
source = "git+https://github.com/example/gitcrate#abc123"
"#;

    fn write_lock(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Cargo.lock");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn cargo_reader_includes_registry_entries_including_duplicate_versions() {
        let (_dir, path) = write_lock(CARGO_LOCK);
        let scan = scan_cargo_lock(&path).unwrap();
        let got: Vec<(&str, &str)> = scan
            .packages
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![("examplecrate", "1.2.3"), ("examplecrate", "2.0.1")],
            "path dep (no source) and git dep excluded; BOTH duplicate versions kept"
        );
        assert!(
            scan.packages
                .iter()
                .all(|p| p.ecosystem == Ecosystem::CratesIo)
        );
    }

    #[test]
    fn cargo_reader_anchors_duplicate_versions_to_distinct_lines() {
        let lock = "\n[[package]]\nname = \"dupcrate\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"dupcrate\"\nversion = \"2.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        let (_dir, path) = write_lock(lock);
        let scan = scan_cargo_lock(&path).unwrap();
        assert_eq!(scan.packages.len(), 2);
        assert!(scan.packages[0].line_number.is_some());
        assert!(scan.packages[1].line_number.is_some());
        assert_ne!(
            scan.packages[0].line_number, scan.packages[1].line_number,
            "each duplicate version must anchor to its own block, not both to the first"
        );
    }
}
