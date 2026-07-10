//! package-lock.json / npm-shrinkwrap.json reader (identical formats).

use super::{LockScan, LockedPackage};
use crate::audit::Ecosystem;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// Package name for a `packages`-map key: everything after the LAST
/// `node_modules/` segment. Scoped names (`@scope/name`) are two path
/// components and are kept whole. Returns `None` for keys with no
/// node_modules segment (the root entry and local file:/link targets).
fn name_from_key(key: &str) -> Option<&str> {
    let idx = key.rfind("node_modules/")?;
    let name = &key[idx + "node_modules/".len()..];
    (!name.is_empty()).then_some(name)
}

/// Single forward pass over the raw JSON text, indexing each object key's
/// 1-based line by the key text (quotes stripped): the first line (after
/// leading whitespace) that starts with a `"` has its first quoted token
/// extracted as the key. `packages`-map keys are unique, so this is an
/// exact anchor. Called once per lockfile rather than rescanned per
/// package, so anchoring the whole file is O(lines) instead of
/// O(packages x lines).
fn index_key_lines(content: &str) -> HashMap<String, usize> {
    let mut index = HashMap::new();
    for (idx, line) in content.lines().enumerate() {
        let Some(rest) = line.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        index.entry(rest[..end].to_string()).or_insert(idx + 1);
    }
    index
}

/// Scan a package-lock.json or npm-shrinkwrap.json. lockfileVersion 1
/// (npm v5/v6, `dependencies` tree) is not parsed: it produces a coverage
/// warning instead of silently missing the transitive tree.
pub fn scan_npm_lock(path: &Path) -> Result<LockScan> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc: serde_json::Value =
        serde_json::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

    let mut scan = LockScan::default();
    let version = doc.get("lockfileVersion").and_then(|v| v.as_u64());
    if version == Some(1) {
        scan.warnings.push(format!(
            "{}: legacy package-lock.json (lockfileVersion 1): transitive dependencies not scanned; regenerate with npm >= 7",
            path.display()
        ));
        return Ok(scan);
    }
    let Some(packages) = doc.get("packages").and_then(|p| p.as_object()) else {
        return Ok(scan);
    };
    let key_lines = index_key_lines(&content);
    for (key, entry) in packages {
        let Some(path_name) = name_from_key(key) else {
            continue;
        };
        if entry.get("link").and_then(|l| l.as_bool()) == Some(true) {
            continue;
        }
        let Some(version) = entry.get("version").and_then(|v| v.as_str()) else {
            continue;
        };
        // Alias installs reify under the alias folder; the entry's `name`
        // field carries the real registry name and wins over the path.
        let name = entry
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or(path_name);
        scan.packages.push(LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ecosystem: Ecosystem::Npm,
            lockfile_path: path.to_path_buf(),
            line_number: key_lines.get(key).copied(),
            locator: Some(key.clone()),
        });
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::Ecosystem;

    fn write_lock(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package-lock.json");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    const NPM_LOCK: &str = r#"{
  "name": "t", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
  "packages": {
    "": { "name": "t", "version": "1.0.0" },
    "node_modules/examplepkg": { "version": "4.3.4", "resolved": "https://registry.npmjs.org/examplepkg/-/examplepkg-4.3.4.tgz" },
    "node_modules/@scope/scoped": { "version": "1.1.0", "resolved": "https://registry.npmjs.org/@scope/scoped/-/scoped-1.1.0.tgz" },
    "node_modules/examplepkg/node_modules/@scope/nested": { "version": "0.9.0" },
    "node_modules/my-alias": { "name": "realpkg", "version": "18.0.0" },
    "node_modules/locallink": { "resolved": "../local-lib", "link": true },
    "../local-lib": { "name": "local-lib", "version": "1.0.0" }
  }
}"#;

    #[test]
    fn npm_reader_filters_and_extracts_names() {
        let (_dir, path) = write_lock(NPM_LOCK);
        let scan = scan_npm_lock(&path).unwrap();
        let mut got: Vec<(String, String)> = scan
            .packages
            .iter()
            .map(|p| (p.name.clone(), p.version.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("@scope/nested".to_string(), "0.9.0".to_string()),
                ("@scope/scoped".to_string(), "1.1.0".to_string()),
                ("examplepkg".to_string(), "4.3.4".to_string()),
                ("realpkg".to_string(), "18.0.0".to_string()),
            ],
            "root entry, link entry, and non-node_modules local entry excluded; alias name field wins; scoped names kept whole even nested"
        );
        assert!(scan.packages.iter().all(|p| p.ecosystem == Ecosystem::Npm));
        assert!(scan.warnings.is_empty());
    }

    #[test]
    fn npm_reader_lockfile_version_1_warns_and_scans_nothing() {
        let v1 = r#"{ "name": "t", "version": "1.0.0", "lockfileVersion": 1,
  "dependencies": { "examplepkg": { "version": "4.3.4" } } }"#;
        let (_dir, path) = write_lock(v1);
        let scan = scan_npm_lock(&path).unwrap();
        assert!(scan.packages.is_empty());
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan.warnings[0].contains("lockfileVersion 1"));
        assert!(scan.warnings[0].contains("npm >= 7"));
    }

    #[test]
    fn npm_reader_malformed_json_is_an_error() {
        let (_dir, path) = write_lock("{ not json");
        assert!(scan_npm_lock(&path).is_err());
    }

    #[test]
    fn npm_reader_records_packages_map_key_as_locator() {
        let (_dir, path) = write_lock(NPM_LOCK); // the existing fixture in this file
        let scan = scan_npm_lock(&path).unwrap();
        let nested = scan
            .packages
            .iter()
            .find(|p| p.name == "@scope/nested")
            .expect("nested entry present");
        assert_eq!(
            nested.locator.as_deref(),
            Some("node_modules/examplepkg/node_modules/@scope/nested")
        );
        assert!(scan.packages.iter().all(|p| p.locator.is_some()));
    }
}
