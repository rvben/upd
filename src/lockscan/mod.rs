//! Parse-only lockfile readers feeding the audit pipeline.
//!
//! Readers extract the REGISTRY-RESOLVED package set from a lockfile so
//! audit can check transitive dependencies that appear in no manifest.
//! They never write lockfiles and never shell out; regeneration stays in
//! `crate::lockfile`.

pub mod cargo;
pub mod discover;
pub mod npm;
pub mod poetry;
pub mod provenance;
pub mod uv;

use crate::audit::Ecosystem;
use crate::updater::FileType;
use std::collections::HashMap;
use std::path::PathBuf;

/// A registry-resolved package read from a lockfile.
#[derive(Debug, Clone)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub ecosystem: Ecosystem,
    pub lockfile_path: PathBuf,
    /// Best-effort 1-based line of the package entry, for SARIF anchoring.
    pub line_number: Option<usize>,
    /// Format-specific locator of the entry inside the lockfile. npm: the
    /// `packages`-map key (encodes nesting, drives positional provenance).
    /// Other formats: None.
    pub locator: Option<String>,
}

/// Result of scanning one lockfile.
#[derive(Debug, Default)]
pub struct LockScan {
    pub packages: Vec<LockedPackage>,
    /// Incomplete-coverage warnings (merged into `AuditResult.warnings`).
    pub warnings: Vec<String>,
}

/// Single forward pass over a TOML lockfile's lines, indexing package name
/// to the 1-based line of its first `name = "..."` key (after leading
/// whitespace) - entry keys start their line, while references inside
/// arrays/inline tables (e.g. `{ name = "x" }`) do not and are skipped.
///
/// Used by uv.lock and poetry.lock readers, whose `[[package]]` entries both
/// carry a top-level `name = "..."` key. Called once per lockfile rather
/// than rescanned per package, so anchoring the whole file is O(lines)
/// instead of O(packages x lines).
pub(crate) fn index_name_lines(content: &str) -> HashMap<String, usize> {
    let mut index = HashMap::new();
    for (idx, line) in content.lines().enumerate() {
        let Some(rest) = line.trim_start().strip_prefix("name = \"") else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        index.entry(rest[..end].to_string()).or_insert(idx + 1);
    }
    index
}

/// Discover and scan all scannable lockfiles for the discovered manifests.
/// Reader errors (malformed lockfiles) become warnings, not hard failures -
/// a broken lockfile must not abort the manifest audit.
pub fn scan_locks(files: &[(PathBuf, FileType)], scan_roots: &[PathBuf]) -> LockScan {
    let discovery = discover::discover_locks(files, scan_roots);
    let mut result = LockScan {
        packages: Vec::new(),
        warnings: discovery.warnings,
    };
    for lock in discovery.locks {
        let scanned = match lock.kind {
            discover::LockKind::Uv => uv::scan_uv_lock(&lock.path),
            discover::LockKind::Poetry => poetry::scan_poetry_lock(&lock.path),
            discover::LockKind::Npm => npm::scan_npm_lock(&lock.path),
            discover::LockKind::Cargo => cargo::scan_cargo_lock(&lock.path),
        };
        match scanned {
            Ok(mut scan) => {
                result.packages.append(&mut scan.packages);
                result.warnings.append(&mut scan.warnings);
            }
            Err(e) => result.warnings.push(format!(
                "{}: could not scan lockfile: {e:#}",
                lock.path.display()
            )),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_name_lines_is_one_based_first_match() {
        let content = "a\nb\nname = \"x\"\nname = \"x\"\n";
        let index = index_name_lines(content);
        assert_eq!(index.get("x"), Some(&3));
        assert_eq!(index.get("absent"), None);
    }

    #[test]
    fn index_name_lines_ignores_indented_references() {
        // A reference inside an array/inline table (indented, not at line
        // start) must not shadow the real entry key further down.
        let content = "deps = [\n  { name = \"x\" },\n]\nname = \"x\"\n";
        let index = index_name_lines(content);
        assert_eq!(index.get("x"), Some(&4));
    }
}
