//! Static Gradle catalogs and literal plugin declarations. Never executes a build.
use super::{
    CooldownOutcome, FileType, Lang, ParsedDependency, UpdateOptions, UpdateResult, Updater,
    apply_cooldown, downgrade_warning, read_file_safe, write_file_atomic,
};
use crate::registry::Registry;
use crate::version::gradle::{compare, is_literal, is_prerelease};
use anyhow::{Result, anyhow, bail};
use std::{collections::BTreeMap, ops::Range, path::Path};

#[derive(Default)]
pub struct GradleUpdater;
#[derive(Debug)]
struct Entry {
    package: String,
    version: String,
    span: Range<usize>,
    line: usize,
}
#[derive(Default)]
struct Scan {
    entries: Vec<Entry>,
    errors: Vec<String>,
}

fn line_at(content: &str, offset: usize) -> usize {
    content[..offset].bytes().filter(|b| *b == b'\n').count() + 1
}
fn string_span(content: &str, item: &toml_edit::Item) -> Result<Range<usize>> {
    let value = item
        .as_str()
        .ok_or_else(|| anyhow!("expected a literal version string"))?;
    let span = item.span().ok_or_else(|| anyhow!("missing source span"))?;
    let raw = &content[span.clone()];
    if raw.len() != value.len() + 2
        || !matches!(raw.as_bytes()[0], b'\'' | b'"')
        || &raw[1..raw.len() - 1] != value
    {
        bail!("escaped or multiline catalog strings are unsupported");
    }
    Ok(span.start + 1..span.end - 1)
}

fn catalog(content: &str) -> Result<Scan> {
    let doc = toml_edit::Document::parse(content)?;
    let mut scan = Scan::default();
    for section in ["libraries", "plugins"] {
        let Some(items) = doc.get(section).and_then(|i| i.as_table_like()) else {
            continue;
        };
        for (alias, item) in items.iter() {
            let parsed = (|| -> Result<Entry> {
                let (package, version_item, shorthand) = if let Some(value) = item.as_str() {
                    if section == "plugins" {
                        bail!("plugin requires an id and version");
                    }
                    let p: Vec<_> = value.split(':').collect();
                    if p.len() != 3 {
                        bail!("expected group:artifact:version");
                    }
                    (format!("{}:{}", p[0], p[1]), item, true)
                } else {
                    let package = if section == "plugins" {
                        format!(
                            "gradle-plugin:{}",
                            item.get("id")
                                .and_then(|i| i.as_str())
                                .ok_or_else(|| anyhow!("missing plugin id"))?
                        )
                    } else if let Some(module) = item.get("module").and_then(|i| i.as_str()) {
                        module.to_string()
                    } else {
                        format!(
                            "{}:{}",
                            item.get("group")
                                .and_then(|i| i.as_str())
                                .ok_or_else(|| anyhow!("missing group"))?,
                            item.get("name")
                                .and_then(|i| i.as_str())
                                .ok_or_else(|| anyhow!("missing artifact name"))?
                        )
                    };
                    let v = item.get("version").ok_or_else(|| {
                        anyhow!("version is managed externally; no literal version")
                    })?;
                    let v = if let Some(reference) = v.get("ref").and_then(|i| i.as_str()) {
                        // Extra rich constraints affect the shared value too. Refuse the
                        // catalog rather than updating a value with an unexamined consumer.
                        if v.as_table_like().is_none_or(|t| t.len() != 1) {
                            bail!("version.ref combined with rich constraints is unsupported");
                        }
                        doc.get("versions")
                            .and_then(|v| v.get(reference))
                            .ok_or_else(|| anyhow!("missing version.ref '{reference}'"))?
                    } else {
                        v
                    };
                    (package, v, false)
                };
                let mut span = string_span(content, version_item)?;
                if shorthand {
                    span.start += content[span.clone()].rfind(':').unwrap() + 1;
                }
                let version = content[span.clone()].to_string();
                Ok(Entry {
                    package,
                    version,
                    line: line_at(content, span.start),
                    span,
                })
            })();
            match parsed {
                Ok(entry) => scan.entries.push(entry),
                Err(e) => {
                    // An unsupported consumer of a shared version must not be
                    // invisible to the group's consistency check.
                    if item.get("version").and_then(|v| v.get("ref")).is_some() {
                        bail!("{section}.{alias}: {e}");
                    }
                    scan.errors.push(format!("{section}.{alias}: {e}"));
                }
            }
        }
    }
    Ok(scan)
}

#[derive(Debug)]
struct Token<'a> {
    text: &'a str,
    span: Range<usize>,
    literal: bool,
    quoted: bool,
}
// Comments and strings are consumed as units, so example code inside them
// cannot masquerade as a plugins block. Offsets always refer to original bytes.
fn tokens(content: &str) -> Result<Vec<Token<'_>>> {
    let b = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if content[i..].starts_with("//") {
            i += content[i..].find('\n').unwrap_or(b.len() - i);
            continue;
        }
        if content[i..].starts_with("/*") {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if content[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if content[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += content[i..].chars().next().unwrap().len_utf8();
                }
            }
            if depth != 0 {
                bail!("unterminated Gradle comment");
            }
            continue;
        }
        if b[i] == b'/' {
            bail!("unsupported Gradle slash expression");
        }
        let start = i;
        if matches!(b[i], b'\'' | b'"') {
            let quote = b[i];
            let triple = b.get(i..i + 3).is_some_and(|s| s == [quote, quote, quote]);
            let width = if triple { 3 } else { 1 };
            i += width;
            let value_start = i;
            let mut literal = !triple;
            loop {
                if i >= b.len() {
                    bail!("unterminated Gradle string");
                }
                if b[i] == quote
                    && (!triple || b.get(i..i + 3).is_some_and(|s| s == [quote, quote, quote]))
                {
                    break;
                }
                if b[i] == b'$' {
                    literal = false;
                }
                if !triple && b[i] == b'\\' {
                    literal = false;
                    i += 1;
                }
                if i < b.len() {
                    i += content[i..].chars().next().unwrap().len_utf8();
                }
            }
            out.push(Token {
                text: &content[value_start..i],
                span: value_start..i,
                literal,
                quoted: true,
            });
            i += width;
        } else if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Token {
                text: &content[start..i],
                span: start..i,
                literal: false,
                quoted: false,
            });
        } else {
            i += content[i..].chars().next().unwrap().len_utf8();
            out.push(Token {
                text: &content[start..i],
                span: start..i,
                literal: false,
                quoted: false,
            });
        }
    }
    Ok(out)
}

fn script(content: &str) -> Result<Scan> {
    let ts = tokens(content)?;
    let mut scan = Scan::default();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < ts.len() {
        let t = &ts[i];
        if t.text == "{" && !t.quoted {
            blocks.push(i > 0 && ts[i - 1].text == "plugins" && !ts[i - 1].quoted);
        }
        if t.text == "}" && !t.quoted {
            blocks.pop();
        }
        if blocks.last() == Some(&true) && !t.quoted && matches!(t.text, "id" | "kotlin") {
            let start = i;
            let kotlin = t.text == "kotlin";
            let mut j = i + 1;
            let paren = ts.get(j).is_some_and(|t| t.text == "(");
            if paren {
                j += 1;
            }
            let Some(id) = ts.get(j).filter(|t| t.literal) else {
                i += 1;
                continue;
            };
            j += 1;
            if paren {
                if ts.get(j).is_none_or(|t| t.text != ")") {
                    i += 1;
                    continue;
                }
                j += 1;
            }
            if ts.get(j).is_none_or(|t| t.text != "version" || t.literal) {
                i += 1;
                continue;
            }
            j += 1;
            let version_paren = ts.get(j).is_some_and(|t| t.text == "(");
            if version_paren {
                j += 1;
            }
            let version = ts.get(j);
            j += 1;
            let closing = !version_paren || ts.get(j).is_some_and(|t| t.text == ")");
            if version_paren {
                j += 1;
            }
            let ends = ts.get(j).is_none_or(|next| {
                matches!(next.text, "}" | ";" | "apply")
                    || version.is_some_and(|v| {
                        content[v.span.end..next.span.start].contains('\n')
                            && !matches!(next.text, "+" | "." | "?")
                    })
            });
            if let Some(v) = version.filter(|v| v.literal && closing && ends) {
                scan.entries.push(Entry {
                    package: format!(
                        "gradle-plugin:{}{}",
                        if kotlin { "org.jetbrains.kotlin." } else { "" },
                        id.text
                    ),
                    version: v.text.into(),
                    span: v.span.clone(),
                    line: line_at(content, v.span.start),
                });
                i = j;
                continue;
            } else {
                scan.errors.push(format!(
                    "line {}: plugin {} has a computed or unsupported version",
                    line_at(content, ts[start].span.start),
                    id.text
                ));
            }
        }
        i += 1;
    }
    Ok(scan)
}

impl GradleUpdater {
    pub fn new() -> Self {
        Self
    }
    fn scan(content: &str, file_type: FileType) -> Result<Scan> {
        if file_type == FileType::GradleCatalog {
            catalog(content)
        } else {
            script(content)
        }
    }
    /// Re-parse approved edits together: every consumer of a shared catalog
    /// value must be selected, and all targets must agree.
    pub fn apply_approved_updates(
        content: &str,
        file_type: FileType,
        edits: &[(&str, &str, &str, Option<usize>)],
    ) -> Result<String> {
        let scan = Self::scan(content, file_type)?;
        let mut replacements = BTreeMap::new();
        for &(package, old, new, line) in edits {
            if !is_literal(new) {
                bail!("unsupported Gradle target: {new}");
            }
            let matches: Vec<_> = scan
                .entries
                .iter()
                .filter(|e| {
                    e.package == package && e.version == old && line.is_none_or(|l| e.line == l)
                })
                .collect();
            if matches.is_empty() {
                bail!("Gradle declaration changed: {package}");
            }
            for entry in matches {
                for peer in scan.entries.iter().filter(|p| p.span == entry.span) {
                    if !edits.iter().any(|&(p, o, n, l)| {
                        p == peer.package
                            && o == peer.version
                            && n == new
                            && l.is_none_or(|l| l == peer.line)
                    }) {
                        bail!(
                            "shared Gradle version requires matching approval for {}",
                            peer.package
                        );
                    }
                }
                replacements.insert((entry.span.start, entry.span.end), new.to_string());
            }
        }
        let mut result = content.to_string();
        for ((start, end), new) in replacements.into_iter().rev() {
            result.replace_range(start..end, &new);
        }
        Ok(result)
    }
}

#[async_trait::async_trait]
impl Updater for GradleUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let file_type =
            FileType::detect(path).ok_or_else(|| anyhow!("unrecognized Gradle file"))?;
        let scan = Self::scan(&content, file_type)?;
        let mut result = UpdateResult {
            errors: scan.errors,
            ..Default::default()
        };
        let mut groups: BTreeMap<(usize, usize), Vec<&Entry>> = BTreeMap::new();
        for entry in &scan.entries {
            groups
                .entry((entry.span.start, entry.span.end))
                .or_default()
                .push(entry);
        }
        let mut replacements = BTreeMap::new();
        for (span, entries) in groups {
            let mut proposals = Vec::new();
            for entry in &entries {
                let e = *entry;
                if options.is_package_filtered_out(&e.package) {
                    result.unchanged += 1;
                    continue;
                }
                if options.should_ignore(&e.package) {
                    result
                        .ignored
                        .push((e.package.clone(), e.version.clone(), Some(e.line)));
                    continue;
                }
                if !is_literal(&e.version) {
                    result.errors.push(format!(
                        "{}: unsupported Gradle version '{}'",
                        e.package, e.version
                    ));
                    continue;
                }
                let pin = options.get_pinned_version(&e.package);
                let resolved = if let Some(pin) = pin {
                    Ok(pin.to_string())
                } else if is_prerelease(&e.version) {
                    registry
                        .get_latest_version_including_prereleases(&e.package)
                        .await
                } else {
                    registry.get_latest_version(&e.package).await
                };
                let mut target = match resolved {
                    Ok(v) => v,
                    Err(err) => {
                        result.errors.push(format!("{}: {err}", e.package));
                        continue;
                    }
                };
                if !is_literal(&target) {
                    result.errors.push(format!(
                        "{}: unsupported Gradle target '{target}'",
                        e.package
                    ));
                    continue;
                }
                let mut held = None;
                if pin.is_none() && compare(&target, &e.version).is_gt() {
                    let (outcome, note) = apply_cooldown(
                        registry,
                        &e.package,
                        &e.version,
                        &target,
                        None,
                        is_prerelease(&e.version),
                        &options,
                    )
                    .await;
                    if let Some(note) = note {
                        options.note_cooldown_unavailable(&note);
                    }
                    match outcome {
                        CooldownOutcome::Unchanged(v) => target = v,
                        CooldownOutcome::HeldBack {
                            chosen,
                            skipped_version,
                            skipped_published_at,
                        } => {
                            target = chosen;
                            held = Some((skipped_version, skipped_published_at));
                        }
                        CooldownOutcome::Skipped {
                            skipped_version,
                            skipped_published_at,
                        } => {
                            result.skipped_by_cooldown.push((
                                e.package.clone(),
                                e.version.clone(),
                                skipped_version,
                                skipped_published_at,
                            ));
                            continue;
                        }
                    }
                }
                if target == e.version {
                    result.unchanged += 1;
                    continue;
                }
                if pin.is_none() && !compare(&target, &e.version).is_gt() {
                    result
                        .warnings
                        .push(downgrade_warning(&e.package, &target, &e.version));
                    result.unchanged += 1;
                    continue;
                }
                if pin.is_none() && !options.allows_bump_for(Lang::Gradle, &e.version, &target) {
                    result.record_capped(&e.package, &e.version, &target, Some(e.line));
                    continue;
                }
                proposals.push((e, target, pin.is_some(), held));
            }
            if proposals.is_empty() {
                continue;
            }
            if proposals.len() != entries.len() || proposals.iter().any(|p| p.1 != proposals[0].1) {
                result.warnings.push(format!("shared Gradle version at line {} left unchanged: all consumers must allow the same target ({})", entries[0].line,
                    entries.iter().map(|e|e.package.as_str()).collect::<Vec<_>>().join(", ")));
                continue;
            }
            replacements.insert(span, proposals[0].1.clone());
            for (e, target, pinned, held) in proposals {
                if pinned {
                    result.pinned.push((
                        e.package.clone(),
                        e.version.clone(),
                        target,
                        Some(e.line),
                    ));
                } else {
                    result.updated.push((
                        e.package.clone(),
                        e.version.clone(),
                        target.clone(),
                        Some(e.line),
                    ));
                    if let Some((skipped, date)) = held {
                        result.held_back.push((
                            e.package.clone(),
                            e.version.clone(),
                            target,
                            skipped,
                            date,
                        ));
                    }
                }
            }
        }
        result.set_update_lang(Lang::Gradle);
        if !options.dry_run && !replacements.is_empty() {
            let mut rewritten = content;
            for ((start, end), new) in replacements.into_iter().rev() {
                rewritten.replace_range(start..end, &new);
            }
            write_file_atomic(path, &rewritten)?;
        }
        Ok(result)
    }
    fn handles(&self, file_type: FileType) -> bool {
        matches!(file_type, FileType::GradleCatalog | FileType::GradleScript)
    }
    fn parse_dependencies(&self, _path: &Path) -> Result<Vec<ParsedDependency>> {
        // Align/audit need catalog-aware grouping and Maven identities. Until
        // those commands support it, do not expose unsafe alignment targets.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::UpdConfig, registry::MockRegistry};
    use std::sync::Arc;
    const CATALOG: &str = "[versions]\nshared = '1.0' # keep\n[libraries]\na = { module = 'g:a', version.ref = 'shared' }\nb = { group = 'g', name = 'b', version.ref = 'shared' }\n[plugins]\np = { id = 'org.example', version = '2.0' }\n";
    async fn run(
        content: &str,
        name: &str,
        registry: MockRegistry,
        options: UpdateOptions,
    ) -> (UpdateResult, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        let result = GradleUpdater::new()
            .update(&path, &registry, options)
            .await
            .unwrap();
        (result, std::fs::read_to_string(path).unwrap())
    }
    fn registry() -> MockRegistry {
        MockRegistry::new("gradle")
            .with_version("g:a", "1.2.3")
            .with_version("g:b", "1.2.3")
            .with_version("gradle-plugin:org.example", "2.1.0")
    }
    #[tokio::test]
    async fn catalog_shared_refs_and_full_versions_preserve_bytes() {
        let (r, written) = run(
            CATALOG,
            "libs.versions.toml",
            registry(),
            UpdateOptions::new(false, false),
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.updated.len(), 3);
        assert_eq!(
            written,
            CATALOG
                .replace("shared = '1.0'", "shared = '1.2.3'")
                .replace("version = '2.0'", "version = '2.1.0'")
        );
        let (_, dry) = run(
            CATALOG,
            "libs.versions.toml",
            registry(),
            UpdateOptions::new(true, false),
        )
        .await;
        assert_eq!(dry, CATALOG);
    }
    #[tokio::test]
    async fn shared_reference_cannot_override_ignored_pinned_filtered_or_failed_peer() {
        let configs = [
            UpdConfig {
                ignore: vec!["g:b".into()],
                ..Default::default()
            },
            UpdConfig {
                pin: [("g:b".into(), "1.0".into())].into(),
                ..Default::default()
            },
        ];
        for config in configs {
            let (r, w) = run(
                CATALOG,
                "libs.versions.toml",
                registry(),
                UpdateOptions::new(false, false).with_config(Arc::new(config)),
            )
            .await;
            assert!(w.contains("shared = '1.0'"));
            assert!(r.warnings.iter().any(|w| w.contains("shared Gradle")));
        }
        let (r, w) = run(
            CATALOG,
            "libs.versions.toml",
            registry(),
            UpdateOptions::new(false, false).with_packages(vec!["g:a".into()]),
        )
        .await;
        assert_eq!(w, CATALOG);
        assert!(r.updated.is_empty());
        let (r, w) = run(
            CATALOG,
            "libs.versions.toml",
            MockRegistry::new("gradle").with_version("g:a", "1.2.3"),
            UpdateOptions::new(false, false),
        )
        .await;
        assert_eq!(w, CATALOG);
        assert!(!r.errors.is_empty());
        let (_, w) = run(
            CATALOG,
            "libs.versions.toml",
            registry().with_version("g:b", "1.3.0"),
            UpdateOptions::new(false, false),
        )
        .await;
        assert!(w.contains("shared = '1.0'"));
    }
    #[tokio::test]
    async fn shorthand_inline_and_dynamic_versions() {
        let input = "[libraries]\na = 'g:a:1.0' # keep\nb = { module = 'g:b', version = '1.+' }\nc = { module = 'g:c', version = { strictly = '1.0' } }\n";
        let (r, w) = run(
            input,
            "libs.versions.toml",
            registry(),
            UpdateOptions::new(false, false),
        )
        .await;
        assert_eq!(r.updated.len(), 1);
        assert_eq!(r.errors.len(), 2);
        assert_eq!(w, input.replace("g:a:1.0", "g:a:1.2.3"));
    }
    #[test]
    fn scripts_ignore_comments_and_strings_and_refuse_computed_versions() {
        let input = r#"
// plugins { id("fake") version "1.0" }
/* outer /* nested */ plugins { id("fake") version "1.0" } */
val example = """plugins { id("fake") version "1.0" }"""
plugins {
    id("org.example") version "2.0" apply false
    id 'other.example' version '1.0'
    kotlin("jvm") version("2.2.0")
    id("computed") version "1.0" + suffix
    id("variable") version pluginVersion
    alias(libs.plugins.kotlin)
    id("java")
}
"#;
        let scan = script(input).unwrap();
        assert_eq!(scan.entries.len(), 3, "{:?}", scan.entries);
        assert_eq!(scan.errors.len(), 2, "{:?}", scan.errors);
        assert_eq!(
            scan.entries[2].package,
            "gradle-plugin:org.jetbrains.kotlin.jvm"
        );
    }
    #[tokio::test]
    async fn plugin_rewrite_preserves_bom_crlf_and_identical_unrelated_literals() {
        let input = "\u{feff}val unrelated = \"2.0\"\r\nplugins {\r\n id(\"org.example\") version \"2.0\" // keep\r\n}\r\n";
        let (r, w) = run(
            input,
            "settings.gradle.kts",
            registry(),
            UpdateOptions::new(false, false),
        )
        .await;
        assert_eq!(r.updated.len(), 1, "{:?}", r.errors);
        assert_eq!(w, input.replace("version \"2.0\"", "version \"2.1.0\""));
    }
    #[test]
    fn approvals_are_atomic_and_reject_partial_shared_edits() {
        let edits = [("g:a", "1.0", "1.2.3", None), ("g:b", "1.0", "1.2.3", None)];
        assert!(
            GradleUpdater::apply_approved_updates(CATALOG, FileType::GradleCatalog, &edits[..1])
                .is_err()
        );
        let w = GradleUpdater::apply_approved_updates(CATALOG, FileType::GradleCatalog, &edits)
            .unwrap();
        assert_eq!(w, CATALOG.replace("shared = '1.0'", "shared = '1.2.3'"));
    }
    #[test]
    fn discovery() {
        for name in ["gradle/libs.versions.toml", "gradle/test.versions.toml"] {
            assert_eq!(
                FileType::detect(Path::new(name)),
                Some(FileType::GradleCatalog)
            );
        }
        for name in [
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ] {
            assert_eq!(
                FileType::detect(Path::new(name)),
                Some(FileType::GradleScript)
            );
        }
        assert_eq!(FileType::GradleCatalog.lang(), Lang::Gradle);
    }
}
