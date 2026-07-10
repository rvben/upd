//! Positional provenance inputs: direct dependency declarations read from
//! manifests, keyed by DEPENDENCY KEY. Provenance is positional, never
//! inferred from range admission alone: the Manifest tag applies only to
//! the lock entry that IS the direct resolved target of a manifest
//! dependency (Task 3 consumes these inputs for classification).

use anyhow::{Context, Result};
use std::path::Path;

/// A direct dependency declaration read from a manifest.
#[derive(Debug, Clone)]
pub struct DirectDep {
    /// Manifest-side dependency key (package.json key / Cargo.toml key).
    pub key: String,
    /// Registry package name: the alias target for npm `npm:` specs, the
    /// `package` field for Cargo renames; equals `key` otherwise.
    pub package: String,
    /// Raw spec / version-requirement fragment as written in the manifest.
    pub spec: String,
}

/// Registry name and range of an npm alias spec: `npm:react@^18` splits to
/// ("react", "^18"), `npm:@scope/name@1.2.3` to ("@scope/name", "1.2.3").
/// The range separator is the LAST `@` so scoped alias targets keep their
/// leading `@scope/`. Returns None for non-alias specs.
pub(crate) fn split_npm_alias(spec: &str) -> Option<(&str, &str)> {
    let rest = spec.strip_prefix("npm:")?;
    match rest.rfind('@') {
        Some(idx) if idx > 0 => Some((&rest[..idx], &rest[idx + 1..])),
        _ => Some((rest, "")),
    }
}

/// Direct dependency declarations of a package.json, across the same
/// sections the updater reads (src/updater/package_json.rs
/// DEPENDENCY_SECTIONS): dependencies, devDependencies, peerDependencies,
/// optionalDependencies. Non-string specs are skipped.
pub fn npm_direct_deps(package_json: &Path) -> Result<Vec<DirectDep>> {
    let content = std::fs::read_to_string(package_json)
        .with_context(|| format!("reading {}", package_json.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", package_json.display()))?;
    let mut deps = Vec::new();
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        let Some(table) = doc.get(section).and_then(|v| v.as_object()) else {
            continue;
        };
        for (key, value) in table {
            let Some(spec) = value.as_str() else { continue };
            let package = match split_npm_alias(spec) {
                Some((target, _)) => target.to_string(),
                None => key.clone(),
            };
            deps.push(DirectDep {
                key: key.clone(),
                package,
                spec: spec.to_string(),
            });
        }
    }
    Ok(deps)
}

/// Direct dependency declarations of a Cargo.toml, across the same sections
/// parse_dependencies walks (src/updater/cargo_toml.rs): [dependencies],
/// [dev-dependencies], [build-dependencies], [workspace.dependencies], and
/// target.*.{dependencies,dev-dependencies,build-dependencies}. Path/git
/// dependencies are skipped (no registry identity). Renamed entries record
/// both the TOML key and the real package name.
pub fn cargo_direct_deps(cargo_toml: &Path) -> Result<Vec<DirectDep>> {
    let content = std::fs::read_to_string(cargo_toml)
        .with_context(|| format!("reading {}", cargo_toml.display()))?;
    let doc: toml::Table = content
        .parse()
        .with_context(|| format!("parsing {}", cargo_toml.display()))?;

    fn parse_table(table: &toml::Table, deps: &mut Vec<DirectDep>) {
        for (key, item) in table {
            match item {
                toml::Value::String(spec) => deps.push(DirectDep {
                    key: key.clone(),
                    package: key.clone(),
                    spec: spec.clone(),
                }),
                toml::Value::Table(t) => {
                    if t.contains_key("path") || t.contains_key("git") {
                        continue;
                    }
                    let Some(spec) = t.get("version").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let package = t
                        .get("package")
                        .and_then(|p| p.as_str())
                        .unwrap_or(key)
                        .to_string();
                    deps.push(DirectDep {
                        key: key.clone(),
                        package,
                        spec: spec.to_string(),
                    });
                }
                _ => {}
            }
        }
    }

    let mut deps = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(toml::Value::Table(t)) = doc.get(section) {
            parse_table(t, &mut deps);
        }
    }
    if let Some(toml::Value::Table(ws)) = doc.get("workspace")
        && let Some(toml::Value::Table(t)) = ws.get("dependencies")
    {
        parse_table(t, &mut deps);
    }
    if let Some(toml::Value::Table(targets)) = doc.get("target") {
        for target_item in targets.values() {
            if let toml::Value::Table(tt) = target_item {
                for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(toml::Value::Table(t)) = tt.get(section) {
                        parse_table(t, &mut deps);
                    }
                }
            }
        }
    }
    Ok(deps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn split_npm_alias_handles_plain_scoped_and_rangeless() {
        assert_eq!(split_npm_alias("npm:react@^18"), Some(("react", "^18")));
        assert_eq!(
            split_npm_alias("npm:@scope/name@1.2.3"),
            Some(("@scope/name", "1.2.3")),
            "range separator is the LAST @ so scoped targets keep @scope/"
        );
        assert_eq!(split_npm_alias("npm:react"), Some(("react", "")));
        assert_eq!(split_npm_alias("^18.0.0"), None);
    }

    #[test]
    fn npm_direct_deps_records_keys_aliases_and_specs() {
        let dir = tempfile::tempdir().unwrap();
        let pj = write(
            &dir,
            "package.json",
            r#"{
  "name": "t",
  "dependencies": {
    "examplepkg": "^4.0.0",
    "my-react": "npm:realpkg@^18",
    "@scope/direct": "1.1.0"
  },
  "devDependencies": { "devtool": "~2.0.0" }
}"#,
        );
        let deps = npm_direct_deps(&pj).unwrap();
        let by_key = |k: &str| deps.iter().find(|d| d.key == k).unwrap();
        assert_eq!(by_key("examplepkg").package, "examplepkg");
        assert_eq!(
            by_key("my-react").package,
            "realpkg",
            "alias target is the registry name"
        );
        assert_eq!(by_key("my-react").spec, "npm:realpkg@^18");
        assert_eq!(by_key("@scope/direct").package, "@scope/direct");
        assert_eq!(
            by_key("devtool").package,
            "devtool",
            "devDependencies are read too"
        );
        assert_eq!(deps.len(), 4);
    }

    #[test]
    fn cargo_direct_deps_records_key_and_package_for_renames() {
        let dir = tempfile::tempdir().unwrap();
        let ct = write(
            &dir,
            "Cargo.toml",
            r#"[package]
name = "t"
version = "0.1.0"

[dependencies]
plain = "1.0"
old_serde = { package = "serde", version = "1.0" }
local = { path = "../local" }

[dev-dependencies]
devcrate = "0.5"

[workspace.dependencies]
shared = "2.0"

[target.'cfg(unix)'.dependencies]
unixdep = "0.3"
"#,
        );
        let deps = cargo_direct_deps(&ct).unwrap();
        let by_key = |k: &str| deps.iter().find(|d| d.key == k).unwrap();
        assert_eq!(by_key("plain").package, "plain");
        assert_eq!(by_key("plain").spec, "1.0");
        assert_eq!(
            by_key("old_serde").package,
            "serde",
            "rename resolves the real package name"
        );
        assert_eq!(by_key("old_serde").spec, "1.0");
        assert!(
            deps.iter().all(|d| d.key != "local"),
            "path deps have no registry identity"
        );
        assert_eq!(by_key("devcrate").package, "devcrate");
        assert_eq!(by_key("shared").package, "shared");
        assert_eq!(by_key("unixdep").package, "unixdep");
    }
}
