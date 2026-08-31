//! npm floor writer: package.json `overrides` entries, spliced into the
//! original text so formatting and key order are preserved (serde_json
//! without preserve_order would reorder every key on re-serialization).

use super::{FloorWriteOutcome, NpmOverrideForm};
use crate::updater::{read_file_safe, write_file_atomic};
use anyhow::{Context, Result, bail};
use std::path::Path;

/// Brace depth at byte offset `at`, tracking JSON string/escape state so
/// braces inside string values never count.
fn depth_at(content: &str, at: usize) -> usize {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in content.char_indices() {
        if idx >= at {
            break;
        }
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else {
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    depth
}

/// Byte span (index of `{` ..= index of matching `}`) of the object value
/// of the TOP-LEVEL key `key`. String-aware; a nested key of the same name
/// deeper in the tree is never matched.
fn top_level_object_span(content: &str, key: &str) -> Option<(usize, usize)> {
    let pattern = regex::Regex::new(&format!(r#""{}"\s*:\s*\{{"#, regex::escape(key)))
        .expect("static pattern");
    for m in pattern.find_iter(content) {
        if depth_at(content, m.start()) != 1 {
            continue;
        }
        let open = m.end() - 1;
        // Walk forward to the matching close brace, string-aware.
        let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
        for (idx, ch) in content[open..].char_indices() {
            if in_string {
                match ch {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((open, open + idx));
                    }
                }
                _ => {}
            }
        }
        return None; // unbalanced; caller surfaces a parse error anyway
    }
    None
}

/// Minimum version of a simple npm spec: exact `1.2.3`, `>=1.2.3`,
/// `^1.2.3`, `~1.2.3`. None for everything else ($refs, ranges, ||, etc.).
fn npm_spec_min(spec: &str) -> Option<semver::Version> {
    let spec = spec.trim();
    let stripped = spec
        .strip_prefix(">=")
        .or_else(|| spec.strip_prefix('^'))
        .or_else(|| spec.strip_prefix('~'))
        .unwrap_or(spec);
    if stripped
        .contains(|c: char| c.is_whitespace() || c == '|' || c == ',' || c == '<' || c == '>')
    {
        return None;
    }
    semver::Version::parse(stripped).ok()
}

pub fn write_npm_override_floor(
    package_json: &Path,
    package: &str,
    floor: &str,
    form: NpmOverrideForm,
    dry_run: bool,
) -> Result<FloorWriteOutcome> {
    let content = read_file_safe(package_json)?;
    let doc: serde_json::Value = serde_json::from_str(&content).with_context(|| {
        format!(
            "parsing {}",
            crate::path_display::display_path(package_json)
        )
    })?;
    if !doc.is_object() {
        bail!(
            "{}: root is not a JSON object",
            crate::path_display::display_path(package_json)
        );
    }
    let desired = match form {
        NpmOverrideForm::Range => format!(">={floor}"),
        NpmOverrideForm::DollarName => format!("${package}"),
    };

    // A malformed top-level overrides (string/array) must be refused, not
    // shadowed by a duplicate key: top_level_object_span would return None
    // for it and the create path would write a second "overrides".
    if let Some(overrides) = doc.get("overrides")
        && !overrides.is_object()
    {
        return Ok(FloorWriteOutcome::Unfixable(format!(
            "existing overrides value is not an object; refusing to rewrite it - add \"{package}\": \">={floor}\" manually"
        )));
    }

    let existing = doc.get("overrides").and_then(|o| o.get(package));
    let replace_from: Option<String> = match existing {
        None => None,
        Some(serde_json::Value::String(current)) => {
            if *current == desired {
                return Ok(FloorWriteOutcome::AlreadySatisfied);
            }
            match npm_spec_min(current) {
                Some(min) => {
                    let floor_v = semver::Version::parse(floor)
                        .with_context(|| format!("floor version {floor} is not semver"))?;
                    if min >= floor_v {
                        return Ok(FloorWriteOutcome::AlreadySatisfied);
                    }
                    Some(current.clone())
                }
                None => {
                    return Ok(FloorWriteOutcome::Unfixable(format!(
                        "existing override \"{current}\" is not a simple form (exact/>=/^/~); refusing to replace it - ensure it floors {package} at >={floor}"
                    )));
                }
            }
        }
        Some(_) => {
            return Ok(FloorWriteOutcome::Unfixable(format!(
                "existing overrides entry for {package} is an object (nested override form); refusing to replace it - raise its floor to >={floor} manually"
            )));
        }
    };

    if dry_run {
        return Ok(FloorWriteOutcome::Written);
    }

    let new_content = match (top_level_object_span(&content, "overrides"), replace_from) {
        (Some((open, close)), Some(old_value)) => {
            // Replace the TOP-LEVEL entry inside the overrides span only. A
            // nested object inside overrides can hold a same-named key (and
            // may appear EARLIER in the file), so candidate matches are
            // filtered by depth relative to the span: the span starts at the
            // overrides `{`, so a direct member key sits at depth 1.
            let span = &content[open..=close];
            let entry_re = regex::Regex::new(&format!(
                r#""{}"\s*:\s*"{}""#,
                regex::escape(package),
                regex::escape(&old_value)
            ))
            .expect("escaped pattern");
            let m = entry_re
                .find_iter(span)
                .find(|m| depth_at(span, m.start()) == 1)
                .with_context(|| {
                    format!(
                        "could not locate top-level override entry for {package} in {}",
                        crate::path_display::display_path(package_json)
                    )
                })?;
            format!(
                "{}{}\"{package}\": \"{desired}\"{}",
                &content[..open],
                &span[..m.start()],
                &content[open + m.end()..]
            )
        }
        (Some((open, close)), None) => {
            // Insert a new entry right after the opening brace.
            let inner = &content[open + 1..close];
            let indent = entry_indent(&content, open);
            if inner.trim().is_empty() {
                let closing_indent = indent.strip_suffix("  ").unwrap_or("").to_string();
                format!(
                    "{}{{\n{indent}\"{package}\": \"{desired}\"\n{closing_indent}}}{}",
                    &content[..open],
                    &content[close + 1..]
                )
            } else {
                format!(
                    "{}{{\n{indent}\"{package}\": \"{desired}\",{}{}",
                    &content[..open],
                    inner,
                    &content[close..]
                )
            }
        }
        (None, _) => {
            // Create the overrides object before the root closing brace.
            let root_close = content
                .rfind('}')
                .context("package.json has no closing brace")?;
            let before = content[..root_close].trim_end();
            let needs_comma = !before.ends_with('{');
            let comma = if needs_comma { "," } else { "" };
            let indent = root_member_indent(&content);
            format!(
                "{before}{comma}\n{indent}\"overrides\": {{\n{indent}{indent}\"{package}\": \"{desired}\"\n{indent}}}\n{}",
                &content[root_close..]
            )
        }
    };

    write_file_atomic(package_json, &new_content)?;
    Ok(FloorWriteOutcome::Written)
}

/// Indentation for entries inside the object opening at `open`: one level
/// deeper than the line holding the opening brace.
fn entry_indent(content: &str, open: usize) -> String {
    let line_start = content[..open].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let existing: String = content[line_start..]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    format!("{existing}  ")
}

/// Indentation of the root object's first member (default two spaces).
fn root_member_indent(content: &str) -> String {
    regex::Regex::new(r#"\n([ \t]+)""#)
        .expect("static pattern")
        .captures(content)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "  ".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::{FloorWriteOutcome, NpmOverrideForm};

    fn write_pj(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    const BARE: &str = "{\n  \"name\": \"t\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {\n    \"other\": \"^1.0.0\"\n  }\n}\n";

    #[test]
    fn creates_overrides_object_when_absent() {
        let (_d, path) = write_pj(BARE);
        let out =
            write_npm_override_floor(&path, "lockonly", "2.5.0", NpmOverrideForm::Range, false)
                .unwrap();
        assert_eq!(out, FloorWriteOutcome::Written);
        let content = std::fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).expect("still valid JSON");
        assert_eq!(doc["overrides"]["lockonly"], ">=2.5.0");
        assert!(
            content.contains("\"dependencies\": {\n    \"other\": \"^1.0.0\"\n  }"),
            "existing formatting preserved verbatim: {content}"
        );
    }

    #[test]
    fn inserts_into_existing_overrides_preserving_other_entries() {
        let (_d, path) = write_pj(
            "{\n  \"name\": \"t\",\n  \"overrides\": {\n    \"keepme\": \"1.0.0\"\n  }\n}\n",
        );
        let out =
            write_npm_override_floor(&path, "lockonly", "2.5.0", NpmOverrideForm::Range, false)
                .unwrap();
        assert_eq!(out, FloorWriteOutcome::Written);
        let content = std::fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(doc["overrides"]["lockonly"], ">=2.5.0");
        assert_eq!(doc["overrides"]["keepme"], "1.0.0");
        assert!(
            content.contains("\"keepme\": \"1.0.0\""),
            "untouched bytes: {content}"
        );
    }

    #[test]
    fn replaces_weaker_entry_and_writes_dollar_name_form() {
        let (_d, path) = write_pj(
            "{\n  \"name\": \"t\",\n  \"overrides\": {\n    \"examplepkg\": \">=1.0.0\"\n  }\n}\n",
        );
        let out = write_npm_override_floor(
            &path,
            "examplepkg",
            "2.5.0",
            NpmOverrideForm::DollarName,
            false,
        )
        .unwrap();
        assert_eq!(out, FloorWriteOutcome::Written);
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["overrides"]["examplepkg"], "$examplepkg");
    }

    #[test]
    fn equal_or_stricter_existing_is_already_satisfied_zero_writes() {
        for existing in [">=2.5.0", "2.5.0", "^2.5.0", "~2.6.0", ">=3.0.0"] {
            let (_d, path) = write_pj(&format!(
                "{{\n  \"overrides\": {{\n    \"examplepkg\": \"{existing}\"\n  }}\n}}\n"
            ));
            let before = std::fs::read_to_string(&path).unwrap();
            let out = write_npm_override_floor(
                &path,
                "examplepkg",
                "2.5.0",
                NpmOverrideForm::Range,
                false,
            )
            .unwrap();
            assert_eq!(
                out,
                FloorWriteOutcome::AlreadySatisfied,
                "existing {existing}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }

    #[test]
    fn object_valued_entry_is_unfixable() {
        let (_d, path) =
            write_pj("{\n  \"overrides\": {\n    \"examplepkg\": { \".\": \">=1.0.0\" }\n  }\n}\n");
        let before = std::fs::read_to_string(&path).unwrap();
        match write_npm_override_floor(&path, "examplepkg", "2.5.0", NpmOverrideForm::Range, false)
            .unwrap()
        {
            FloorWriteOutcome::Unfixable(msg) => {
                assert!(msg.contains("object"), "{msg}");
                assert!(msg.contains("2.5.0"), "{msg}");
            }
            other => panic!("expected Unfixable, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn foreign_dollar_reference_is_unfixable() {
        let (_d, path) =
            write_pj("{\n  \"overrides\": {\n    \"examplepkg\": \"$otherpkg\"\n  }\n}\n");
        match write_npm_override_floor(&path, "examplepkg", "2.5.0", NpmOverrideForm::Range, false)
            .unwrap()
        {
            FloorWriteOutcome::Unfixable(msg) => {
                assert!(msg.contains("not a simple form"), "{msg}")
            }
            other => panic!("expected Unfixable, got {other:?}"),
        }
    }

    #[test]
    fn replace_targets_top_level_entry_not_nested_decoy() {
        // The nested decoy (same key, same old value) appears BEFORE the
        // real top-level entry; a naive first-match replace would hit it.
        let (_d, path) = write_pj(
            "{\n  \"overrides\": {\n    \"other\": {\n      \"examplepkg\": \">=1.0.0\"\n    },\n    \"examplepkg\": \">=1.0.0\"\n  }\n}\n",
        );
        let out =
            write_npm_override_floor(&path, "examplepkg", "2.5.0", NpmOverrideForm::Range, false)
                .unwrap();
        assert_eq!(out, FloorWriteOutcome::Written);
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["overrides"]["examplepkg"], ">=2.5.0",
            "top-level entry replaced"
        );
        assert_eq!(
            doc["overrides"]["other"]["examplepkg"], ">=1.0.0",
            "nested decoy untouched"
        );
    }

    #[test]
    fn nested_overrides_key_inside_another_object_is_not_matched() {
        // Decoy: a "scripts" object containing an "overrides" key. The real
        // top-level overrides is absent, so the writer must CREATE one and
        // leave the decoy untouched.
        let (_d, path) = write_pj(
            "{\n  \"name\": \"t\",\n  \"scripts\": {\n    \"overrides\": \"echo hi\"\n  }\n}\n",
        );
        let out =
            write_npm_override_floor(&path, "lockonly", "2.5.0", NpmOverrideForm::Range, false)
                .unwrap();
        assert_eq!(out, FloorWriteOutcome::Written);
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["scripts"]["overrides"], "echo hi", "decoy untouched");
        assert_eq!(doc["overrides"]["lockonly"], ">=2.5.0");
    }

    #[test]
    fn non_object_top_level_overrides_is_unfixable() {
        let (_d, path) = write_pj("{\n  \"name\": \"t\",\n  \"overrides\": \"garbage\"\n}\n");
        let before = std::fs::read_to_string(&path).unwrap();
        match write_npm_override_floor(&path, "examplepkg", "2.5.0", NpmOverrideForm::Range, false)
            .unwrap()
        {
            FloorWriteOutcome::Unfixable(msg) => assert!(msg.contains("not an object"), "{msg}"),
            other => panic!("expected Unfixable, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "never a duplicate key"
        );
    }

    #[test]
    fn dry_run_reports_without_writing() {
        let (_d, path) = write_pj(BARE);
        let before = std::fs::read_to_string(&path).unwrap();
        let out =
            write_npm_override_floor(&path, "lockonly", "2.5.0", NpmOverrideForm::Range, true)
                .unwrap();
        assert_eq!(out, FloorWriteOutcome::Written);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}
