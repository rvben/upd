//! uv floor writer: [tool.uv] constraint-dependencies entries.

use super::FloorWriteOutcome;
use crate::normalize::pep503_normalize;
use crate::updater::{read_file_safe, write_file_atomic};
use anyhow::{Context, Result};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Value};

/// Split a PEP 508-ish constraint entry into (name, operator, version) for
/// the simple single-clause forms ==X / >=X / ~=X. None for anything else.
fn parse_simple_constraint(entry: &str) -> Option<(&str, &str, &str)> {
    for op in ["==", ">=", "~="] {
        if let Some(idx) = entry.find(op) {
            let name = entry[..idx].trim();
            let version = entry[idx + op.len()..].trim();
            if name.is_empty()
                || version.is_empty()
                || version.contains(',')
                || version.contains(' ')
            {
                return None;
            }
            return Some((name, op, version));
        }
    }
    None
}

/// Write `{package}>={floor}` into `[tool.uv] constraint-dependencies` of
/// `pyproject` (the caller passes the pyproject adjacent to the uv.lock it
/// is flooring, the only place uv honors the setting). Never weakens an
/// existing entry: a simple `==`/`>=`/`~=` entry already at or above
/// `floor` returns `AlreadySatisfied` with zero writes; anything unparseable
/// or multi-clause returns `Unfixable` with guidance rather than being
/// clobbered.
pub fn write_uv_constraint_floor(
    pyproject: &Path,
    package: &str,
    floor: &str,
    dry_run: bool,
) -> Result<FloorWriteOutcome> {
    let content = read_file_safe(pyproject)?;
    let mut doc: DocumentMut = content
        .parse()
        .with_context(|| format!("parsing {}", pyproject.display()))?;
    let unfixable_shape = || {
        FloorWriteOutcome::Unfixable(format!(
            "existing [tool.uv] constraint-dependencies has an unexpected shape; refusing to rewrite it - add {package}>={floor} manually"
        ))
    };

    // tool and tool.uv as implicit tables so the output renders [tool.uv].
    let tool = doc
        .entry("tool")
        .or_insert(Item::Table(toml_edit::Table::new()));
    let Some(tool_table) = tool.as_table_mut() else {
        return Ok(unfixable_shape());
    };
    tool_table.set_implicit(true);
    let uv = tool_table
        .entry("uv")
        .or_insert(Item::Table(toml_edit::Table::new()));
    let Some(uv_table) = uv.as_table_mut() else {
        return Ok(unfixable_shape());
    };
    let constraints = uv_table
        .entry("constraint-dependencies")
        .or_insert(toml_edit::value(toml_edit::Array::new()));
    let Some(array) = constraints.as_value_mut().and_then(Value::as_array_mut) else {
        return Ok(unfixable_shape());
    };

    let target_norm = pep503_normalize(package);
    let mut existing_idx: Option<usize> = None;
    for (idx, item) in array.iter().enumerate() {
        let Some(entry) = item.as_str() else { continue };
        let name_end = entry
            .find(|c: char| "=<>!~".contains(c))
            .unwrap_or(entry.len());
        if pep503_normalize(entry[..name_end].trim()) != target_norm {
            continue;
        }
        match parse_simple_constraint(entry) {
            Some((_, _, existing_version)) => {
                match crate::version::pep440::compare_versions(existing_version, floor) {
                    Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal) => {
                        return Ok(FloorWriteOutcome::AlreadySatisfied);
                    }
                    Some(std::cmp::Ordering::Less) => existing_idx = Some(idx),
                    None => {
                        return Ok(FloorWriteOutcome::Unfixable(format!(
                            "existing constraint \"{entry}\" is not a simple form (==/>=/~=); refusing to replace it - ensure it floors {package} at >={floor}"
                        )));
                    }
                }
            }
            None => {
                return Ok(FloorWriteOutcome::Unfixable(format!(
                    "existing constraint \"{entry}\" is not a simple form (==/>=/~=); refusing to replace it - ensure it floors {package} at >={floor}"
                )));
            }
        }
        break;
    }

    if !dry_run {
        let new_entry = format!("{package}>={floor}");
        match existing_idx {
            Some(idx) => {
                array.replace(idx, new_entry);
            }
            None => array.push(new_entry),
        }
        write_file_atomic(pyproject, &doc.to_string())?;
    }
    Ok(FloorWriteOutcome::Written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::FloorWriteOutcome;

    fn write_pyproject(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pyproject.toml");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    const BARE: &str = "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n";

    #[test]
    fn creates_tool_uv_table_and_array_when_absent() {
        let (_d, path) = write_pyproject(BARE);
        let out = write_uv_constraint_floor(&path, "lockonly", "0.49.1", false).unwrap();
        assert_eq!(out, FloorWriteOutcome::Written);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("[tool.uv]"),
            "explicit [tool.uv] header: {content}"
        );
        assert!(
            !content.contains("[tool]\n"),
            "no bare [tool] super-table: {content}"
        );
        assert!(
            content.contains(r#"constraint-dependencies = ["lockonly>=0.49.1"]"#),
            "{content}"
        );
        assert!(
            content.starts_with("[project]"),
            "existing content preserved: {content}"
        );
    }

    #[test]
    fn replaces_weaker_entry_matching_pep503_variant() {
        // "LockOnly" is a genuine PEP 503 variant of "lockonly": normalization
        // lowercases both to the same string ("lockonly"). ("Lock_Only" is a
        // NAME PEP 503 normalizes to "lock-only" - a different registry name
        // from "lockonly", since PEP 503 collapses separator runs rather than
        // removing them - so it would not be a case this writer should match.)
        let (_d, path) = write_pyproject(
            "[project]\nname = \"t\"\nversion = \"1.0.0\"\n\n[tool.uv]\nconstraint-dependencies = [\"LockOnly>=0.30.0\", \"other>=1.0\"]\n",
        );
        let out = write_uv_constraint_floor(&path, "lockonly", "0.49.1", false).unwrap();
        assert_eq!(out, FloorWriteOutcome::Written);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("lockonly>=0.49.1"), "{content}");
        assert!(
            !content.contains("LockOnly>=0.30.0"),
            "weaker variant replaced: {content}"
        );
        assert!(
            content.contains("other>=1.0"),
            "unrelated entries preserved: {content}"
        );
    }

    #[test]
    fn equal_or_stricter_existing_floor_is_already_satisfied_with_zero_writes() {
        for existing in ["lockonly>=0.49.1", "lockonly>=0.50.0", "lockonly==0.50.0"] {
            let (_d, path) = write_pyproject(&format!(
                "[project]\nname = \"t\"\nversion = \"1.0.0\"\n\n[tool.uv]\nconstraint-dependencies = [\"{existing}\"]\n"
            ));
            let before = std::fs::read_to_string(&path).unwrap();
            let out = write_uv_constraint_floor(&path, "lockonly", "0.49.1", false).unwrap();
            assert_eq!(
                out,
                FloorWriteOutcome::AlreadySatisfied,
                "existing {existing}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                before,
                "no bytes written"
            );
        }
    }

    #[test]
    fn complex_existing_entry_is_unfixable_with_guidance() {
        let (_d, path) = write_pyproject(
            "[project]\nname = \"t\"\nversion = \"1.0.0\"\n\n[tool.uv]\nconstraint-dependencies = [\"lockonly>=0.30,<0.40\"]\n",
        );
        let before = std::fs::read_to_string(&path).unwrap();
        let out = write_uv_constraint_floor(&path, "lockonly", "0.49.1", false).unwrap();
        match out {
            FloorWriteOutcome::Unfixable(msg) => {
                assert!(msg.contains("not a simple form"), "{msg}");
                assert!(msg.contains("lockonly"), "{msg}");
                assert!(msg.contains("0.49.1"), "{msg}");
            }
            other => panic!("expected Unfixable, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn dry_run_reports_written_without_writing() {
        let (_d, path) = write_pyproject(BARE);
        let before = std::fs::read_to_string(&path).unwrap();
        let out = write_uv_constraint_floor(&path, "lockonly", "0.49.1", true).unwrap();
        assert_eq!(out, FloorWriteOutcome::Written);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}
