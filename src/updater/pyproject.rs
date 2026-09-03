use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, apply_cooldown_without_floor,
    downgrade_warning, is_pep440_version, pep440_admits, pep440_floor,
    python_version_with_revalidation, read_file_safe, replace_string_value, specifier_floor,
    unpinnable_error, unreadable_error, unrewritable_warning, write_file_atomic,
};
use crate::align::compare_versions;
use crate::config::SpecifierOperator;
use crate::normalize::pep503_normalize;
use crate::registry::{DeclaredIndex, IndexChain, Registry, VersionQuery};
use crate::updater::{Lang, NormalizedSpec};
use crate::version::{is_prerelease_pep440, is_stable_pep440, match_version_precision};
use anyhow::{Result, anyhow};
use futures::future::join_all;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use toml_edit::{Array, DocumentMut, Formatted, Item, TableLike, Value};

/// Package indexes declared by a manifest: the query chain and the packages
/// pinned to a named index (package name -> declared index name).
#[derive(Debug, Default, PartialEq, Eq)]
struct DeclaredIndexes {
    chain: Vec<DeclaredIndex>,
    pins: HashMap<String, String>,
}

fn table_str<'t>(table: &'t dyn TableLike, key: &str) -> Option<&'t str> {
    match table.get(key) {
        Some(Item::Value(Value::String(s))) if !s.value().is_empty() => Some(s.value()),
        _ => None,
    }
}

fn table_bool(table: &dyn TableLike, key: &str) -> bool {
    matches!(table.get(key), Some(Item::Value(Value::Boolean(b))) if *b.value())
}

/// The string entries of an array value; a missing key or a non-array is empty.
fn table_str_array(table: &dyn TableLike, key: &str) -> Vec<String> {
    match table.get(key) {
        Some(Item::Value(Value::Array(items))) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn table_entries<'t>(table: &'t dyn TableLike, key: &str) -> Vec<&'t dyn TableLike> {
    match table.get(key) {
        Some(Item::ArrayOfTables(tables)) => {
            tables.iter().map(|entry| entry as &dyn TableLike).collect()
        }
        Some(Item::Value(Value::Array(values))) => values
            .iter()
            .filter_map(Value::as_inline_table)
            .map(|entry| entry as &dyn TableLike)
            .collect(),
        _ => Vec::new(),
    }
}

fn item_at<'a>(doc: &'a DocumentMut, path: &[&str]) -> Option<&'a Item> {
    let (first, rest) = path.split_first()?;
    let mut item = doc.get(first)?;
    for key in rest {
        item = item.as_table_like()?.get(key)?;
    }
    Some(item)
}

fn array_at<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Option<&'a mut Array> {
    let (first, rest) = path.split_first()?;
    let mut item = doc.get_mut(first)?;
    for key in rest {
        item = item.as_table_like_mut()?.get_mut(key)?;
    }
    item.as_array_mut()
}

fn keys_at(doc: &DocumentMut, path: &[&str]) -> Vec<String> {
    item_at(doc, path)
        .and_then(Item::as_table_like)
        .map(|table| table.iter().map(|(key, _)| key.to_string()).collect())
        .unwrap_or_default()
}

pub struct PyProjectUpdater {
    // Regex to extract version from dependency string
    // Matches: package==1.0.0, package>=1.0.0, package[extra]>=1.0.0, etc.
    version_re: Regex,
    // Regex to capture the full constraint including additional constraints after commas
    // E.g., ">=2.8.0,<9" or ">=1.0.0,!=1.5.0,<2.0.0"
    constraint_re: Regex,
    /// A name with optional extras and marker but no version specifier.
    bare_re: Regex,
    /// PEP 508's parenthesized form, `foo (>=1, <2)`.
    paren_re: Regex,
}

#[derive(Default)]
struct PyProjectLineIndex {
    lines_by_section: HashMap<String, HashMap<String, usize>>,
    /// Section -> exact dependency value -> physical lines, in document order.
    lines_by_entry: HashMap<String, HashMap<String, Vec<usize>>>,
}

/// One PEP 508 dependency string, read.
struct ParsedDep {
    package: String,
    /// The version the specifier is anchored at, for display and comparison.
    version: String,
    /// The full constraint string (e.g. `">=2.8.0,<9"`).
    full_constraint: String,
    /// Whether `version` is a floor an update may carry forward, rather than a
    /// ceiling or a version the specifier rules out. See [`specifier_floor`].
    raisable: bool,
}

/// Parsed coordinates and semantics for one dependency normalization target.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizeTarget {
    package: String,
    name_end: usize,
    spec_start: usize,
    spec_end: usize,
    previous_spec: Option<String>,
    version: Option<String>,
    single_clause: Option<(String, String)>,
    /// Greatest inclusive lower bound that parses as a PEP 440 version.
    anchor: Option<String>,
    /// Version the ordinary shape-preserving path can act on.
    interactive_version: Option<String>,
}

pub(crate) fn has_local_label(version: &str) -> bool {
    version
        .parse::<pep440_rs::Version>()
        .is_ok_and(|version| version.is_local())
}

struct ManifestContext<'a> {
    line_index: &'a PyProjectLineIndex,
    non_registry: HashSet<String>,
}

struct NormalizeSection<'a> {
    path: &'a str,
    operator: SpecifierOperator,
}

enum Rewrite {
    Unchanged,
    SameShape,
    NewShape,
}

fn classify_rewrite(target: &NormalizeTarget, operator: &str, version: &str) -> Rewrite {
    match &target.single_clause {
        Some((existing_operator, existing_version))
            if existing_operator == operator && existing_version == version =>
        {
            Rewrite::Unchanged
        }
        Some((existing_operator, _))
            if existing_operator == operator && target.anchor.is_some() =>
        {
            Rewrite::SameShape
        }
        _ => Rewrite::NewShape,
    }
}

fn section_operator(section_path: &str, options: &UpdateOptions) -> Option<SpecifierOperator> {
    let pyproject = options.config.as_ref()?.normalize?.pyproject?;
    if section_path == "project.dependencies" {
        pyproject.dependencies
    } else if section_path.starts_with("project.optional-dependencies.") {
        pyproject.optional_dependencies
    } else if section_path.starts_with("dependency-groups.") {
        pyproject.dependency_groups
    } else {
        None
    }
}

fn split_clause(clause: &str) -> Option<(String, String)> {
    let op_len = if clause.starts_with("===") {
        3
    } else if ["==", ">=", "<=", "~=", "!="]
        .iter()
        .any(|op| clause.starts_with(op))
    {
        2
    } else if clause.starts_with('>') || clause.starts_with('<') {
        1
    } else {
        return None;
    };
    let operand = &clause[op_len..];
    (!operand.is_empty()).then(|| (clause[..op_len].to_string(), operand.to_string()))
}

fn greatest_pep440_floor(clauses: &[String]) -> Option<String> {
    clauses
        .iter()
        .filter_map(|clause| split_clause(clause))
        .filter(|(operator, _)| crate::updater::operator_is_raisable(operator))
        .filter_map(|(_, operand)| {
            operand
                .parse::<pep440_rs::Version>()
                .ok()
                .map(|parsed| (parsed, operand))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, operand)| operand)
}

fn specifier_target(
    package: &str,
    (name_end, spec_start): (usize, usize),
    spec: &str,
    spec_end: usize,
) -> Option<NormalizeTarget> {
    let clauses: Vec<String> = spec
        .split(',')
        .map(|clause| clause.chars().filter(|c| !c.is_whitespace()).collect())
        .collect();
    let parsed: Vec<(String, String)> = clauses
        .iter()
        .map(|clause| split_clause(clause))
        .collect::<Option<_>>()?;
    let single_clause = match parsed.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    };
    let anchor = greatest_pep440_floor(&clauses);
    let interactive_version = match specifier_floor(spec, 0) {
        Some(floor) if floor.raisable => Some(spec[floor.range].to_string()),
        Some(_) => None,
        None => parsed.first().and_then(|(operator, operand)| {
            crate::updater::operator_is_raisable(operator).then(|| operand.clone())
        }),
    };
    let version = anchor.clone().unwrap_or_else(|| parsed[0].1.clone());
    Some(NormalizeTarget {
        package: package.to_string(),
        name_end,
        spec_start,
        spec_end,
        previous_spec: Some(spec.to_string()),
        version: Some(version),
        single_clause: single_clause.filter(|(_, operand)| is_pep440_version(operand)),
        anchor,
        interactive_version,
    })
}

fn with_specifier(dep: &str, target: &NormalizeTarget, spec: &str) -> String {
    let mut out = String::with_capacity(dep.len() + spec.len());
    out.push_str(&dep[..target.name_end]);
    out.push_str(dep[target.name_end..target.spec_start].trim_start());
    out.push_str(spec);
    out.push_str(&dep[target.spec_end..]);
    out
}

fn name_bounds(caps: &regex::Captures<'_>) -> (usize, usize) {
    let name_end = caps.get(1).unwrap().end();
    (name_end, caps.get(2).map_or(name_end, |m| m.end()))
}

/// Registry-inapplicable dependencies declared through uv or Poetry sources.
fn non_registry_sources(doc: &DocumentMut) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(tool) = doc.get("tool").and_then(Item::as_table_like) else {
        return names;
    };

    if let Some(sources) = tool
        .get("uv")
        .and_then(Item::as_table_like)
        .and_then(|uv| uv.get("sources"))
        .and_then(Item::as_table_like)
    {
        for (package, source) in sources.iter() {
            let non_index = match source {
                // A list represents marker-qualified alternatives. Choosing one
                // index's version for the shared PEP 621 declaration could make
                // the other environment unresolvable, so leave it untouched.
                Item::Value(Value::Array(_)) => true,
                other => other.as_table_like().is_some_and(|table| {
                    table.get("index").is_none() || table.get("marker").is_some()
                }),
            };
            if non_index {
                names.insert(pep503_normalize(package));
            }
        }
    }

    if let Some(deps) = tool
        .get("poetry")
        .and_then(Item::as_table_like)
        .and_then(|poetry| poetry.get("dependencies"))
        .and_then(Item::as_table_like)
    {
        const SOURCE_KEYS: [&str; 4] = ["path", "git", "url", "source"];
        for (package, dep) in deps.iter() {
            let sourced_elsewhere = match dep {
                Item::Value(Value::Array(alternatives)) => alternatives.iter().any(|alt| {
                    alt.as_inline_table()
                        .is_none_or(|table| SOURCE_KEYS.iter().any(|key| table.get(key).is_some()))
                }),
                other => other
                    .as_table_like()
                    .is_some_and(|table| SOURCE_KEYS.iter().any(|key| table.get(key).is_some())),
            };
            if sourced_elsewhere {
                names.insert(pep503_normalize(package));
            }
        }
    }
    names
}

/// Apply the shape-changing rewrites discovered by a dry-run scan.
///
/// Interactive mode applies ordinary version transitions through its generic
/// writer. Bare and compound PEP 508 entries need the pyproject parser itself,
/// because their whole specifier changes rather than one version token.
pub fn apply_normalized_specs(content: &str, specs: &[NormalizedSpec]) -> Result<String> {
    if specs.is_empty() {
        return Ok(content.to_string());
    }

    let mut doc: DocumentMut = content.parse().map_err(|error: toml_edit::TomlError| {
        anyhow!("failed to parse pyproject.toml while applying normalization: {error}")
    })?;
    let updater = PyProjectUpdater::new();
    let mut applied = vec![false; specs.len()];

    let mut rewrite_array = |section: &str, array: &mut toml_edit::Array| {
        for value in array.iter_mut() {
            let Some(dep) = value.as_str() else {
                continue;
            };
            let Some(target) = updater.parse_normalizable(dep) else {
                continue;
            };
            let Some((index, spec)) = specs.iter().enumerate().find(|(index, spec)| {
                !applied[*index]
                    && spec.section == section
                    && pep503_normalize(&spec.package) == pep503_normalize(&target.package)
                    && spec.previous_spec == target.previous_spec
            }) else {
                continue;
            };
            let rewritten = with_specifier(dep, &target, &spec.new_spec);
            if let Value::String(formatted) = value {
                replace_string_value(formatted, rewritten);
                applied[index] = true;
            }
        }
    };

    if let Some(dependencies) = array_at(&mut doc, &["project", "dependencies"]) {
        rewrite_array("project.dependencies", dependencies);
    }
    for group in keys_at(&doc, &["project", "optional-dependencies"]) {
        if let Some(dependencies) = array_at(
            &mut doc,
            &["project", "optional-dependencies", group.as_str()],
        ) {
            rewrite_array(
                &format!("project.optional-dependencies.{group}"),
                dependencies,
            );
        }
    }
    for group in keys_at(&doc, &["dependency-groups"]) {
        if let Some(dependencies) = array_at(&mut doc, &["dependency-groups", group.as_str()]) {
            rewrite_array(&format!("dependency-groups.{group}"), dependencies);
        }
    }

    if let Some(index) = applied.iter().position(|done| !done) {
        let spec = &specs[index];
        return Err(anyhow!(
            "could not find '{}' with specifier {} while applying normalization",
            spec.package,
            spec.previous_spec.as_deref().unwrap_or("(unpinned)")
        ));
    }
    Ok(doc.to_string())
}

#[derive(Clone)]
struct ArraySectionState {
    section_path: String,
    depth: usize,
    table_depth: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ArrayBracketCounts {
    opening: usize,
    closing: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MultiLineString {
    Outside,
    Basic,
    Literal,
}

impl PyProjectUpdater {
    pub fn new() -> Self {
        let version_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)\s*(\[[^\]]+\])?\s*(==|>=|<=|~=|!=|>|<)\s*([^\s,;]+)",
        )
        .expect("Invalid regex");

        // Match the full constraint including additional constraints after commas
        // E.g., ">=2.8.0,<9" or ">=1.0.0,!=1.5.0,<2.0.0". PEP 508 allows
        // whitespace between an operator and its version, so ">= 2.0, < 3" is
        // the same set of clauses; each version runs to the next separator.
        let constraint_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)\s*(\[[^\]]+\])?\s*((?:===|==|>=|<=|~=|!=|>|<)\s*[^\s;,]+(?:\s*,\s*(?:===|==|>=|<=|~=|!=|>|<)\s*[^\s;,]+)*)",
        )
        .expect("Invalid regex");

        let bare_re = Regex::new(r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)\s*(\[[^\]]+\])?\s*(;.*)?$")
            .expect("Invalid regex");
        let paren_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)\s*(\[[^\]]+\])?\s*\(\s*([^)]*?)\s*(\))\s*(?:;.*)?$",
        )
        .expect("Invalid regex");

        Self {
            version_re,
            constraint_re,
            bare_re,
            paren_re,
        }
    }

    /// Where this dependency's floor version sits, and whether it is one an
    /// update may carry forward.
    ///
    /// Falls back to the single-operator match for anything [`pep440_floor`]
    /// cannot place, which keeps a dependency with no recognizable constraint
    /// reading as it always has. That fallback reads its own operator rather
    /// than assuming a floor, so `pkg<v2` is no more rewritable than `pkg<2`.
    /// It never sees a prefix match: `6.*` is digit-led, so the clause reader
    /// places it and answers for it.
    fn floor(&self, dep: &str) -> Option<super::SpecifierFloor> {
        let constraint = self.constraint_re.captures(dep).and_then(|c| c.get(3));
        constraint
            .and_then(|m| pep440_floor(m.as_str(), m.start()))
            .or_else(|| {
                let caps = self.version_re.captures(dep)?;
                Some(super::SpecifierFloor {
                    range: caps.get(4).unwrap().range(),
                    raisable: matches!(caps.get(3).map(|m| m.as_str()), Some("==" | ">=" | "~=")),
                })
            })
    }

    /// Parse a PEP 508 dependency string into the pieces an update needs.
    fn parse_dependency(&self, dep: &str) -> Option<ParsedDep> {
        // First get the full constraint
        let full_constraint = self
            .constraint_re
            .captures(dep)
            .and_then(|c| c.get(3))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();

        let floor = self.floor(dep);
        self.version_re.captures(dep).map(|caps| ParsedDep {
            package: caps.get(1).unwrap().as_str().to_string(),
            version: floor.as_ref().map_or_else(
                || caps.get(4).unwrap().as_str().to_string(),
                |f| dep[f.range.clone()].to_string(),
            ),
            raisable: floor.is_some_and(|f| f.raisable),
            full_constraint,
        })
    }

    /// Check if constraint is simple (no upper bounds that could be violated)
    fn is_simple_constraint(constraint: &str) -> bool {
        // If there are multiple constraints (comma-separated), need constraint-aware lookup
        if constraint.contains(',') {
            return false;
        }

        // If the constraint has an upper-bound operator, need constraint-aware lookup
        if constraint.starts_with('<')
            || constraint.starts_with("<=")
            || constraint.starts_with("~=")
        {
            return false;
        }

        // Also check for != which could affect version selection
        if constraint.starts_with("!=") {
            return false;
        }

        // Simple constraints like "==1.0.0", ">=1.0.0", ">1.0.0" are fine
        true
    }

    fn update_dependency(&self, dep: &str, new_version: &str) -> String {
        if let Some(range) = self.floor(dep).map(|f| f.range) {
            // Only replace the floor version itself, preserving everything else
            // (package name, extras, operator, AND any other constraints like ,<6)
            let mut result = dep.to_string();
            result.replace_range(range, new_version);
            result
        } else {
            dep.to_string()
        }
    }

    /// Parse every registry-shaped PEP 508 entry, including a bare name and
    /// parenthesized specifier, and locate the complete specifier for replacement.
    fn parse_normalizable(&self, dep: &str) -> Option<NormalizeTarget> {
        let lead = dep.len() - dep.trim_start().len();
        let trimmed = &dep[lead..];
        let mut target = if let Some(caps) = self.constraint_re.captures(trimmed) {
            let spec = caps.get(3).unwrap();
            specifier_target(
                caps.get(1).unwrap().as_str(),
                name_bounds(&caps),
                spec.as_str(),
                spec.end(),
            )?
        } else if let Some(caps) = self.paren_re.captures(trimmed) {
            let close = caps.get(4).unwrap().end();
            let spec = caps.get(3).unwrap();
            let mut target = specifier_target(
                caps.get(1).unwrap().as_str(),
                name_bounds(&caps),
                spec.as_str(),
                close,
            )?;
            let open = trimmed[..spec.start()].rfind('(')?;
            target.previous_spec = Some(trimmed[open..close].to_string());
            target.single_clause = None;
            target.interactive_version = None;
            target
        } else {
            let caps = self.bare_re.captures(trimmed)?;
            let (name_end, spec_start) = name_bounds(&caps);
            NormalizeTarget {
                package: caps.get(1).unwrap().as_str().to_string(),
                name_end,
                spec_start,
                spec_end: spec_start,
                previous_spec: None,
                version: None,
                single_clause: None,
                anchor: None,
                interactive_version: None,
            }
        };
        target.name_end += lead;
        target.spec_start += lead;
        target.spec_end += lead;
        Some(target)
    }

    fn assignment_parts(line: &str) -> Option<(String, &str)> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
            return None;
        }

        let (key, value) = trimmed.split_once('=')?;
        Some((
            key.trim().trim_matches('"').trim_matches('\'').to_string(),
            value.trim(),
        ))
    }

    /// The package indexes a pyproject.toml declares, in query order, plus the
    /// packages it pins to a named index.
    ///
    /// Each tool has its own rules for how a declared index relates to the
    /// default one, and the manifest that resolves is the one whose tool table
    /// is consulted: uv, then Poetry, then PDM. A project that keeps a stale
    /// `[tool.poetry]` around after moving to uv is resolved by uv.
    fn declared_indexes(doc: &DocumentMut) -> DeclaredIndexes {
        let Some(tool) = doc.get("tool").and_then(Item::as_table_like) else {
            return DeclaredIndexes::default();
        };

        if let Some(uv) = tool.get("uv").and_then(Item::as_table_like) {
            let declared = Self::uv_indexes(uv);
            if !declared.chain.is_empty() {
                return declared;
            }
        }
        if let Some(poetry) = tool.get("poetry").and_then(Item::as_table_like) {
            let declared = Self::poetry_indexes(poetry);
            if !declared.chain.is_empty() {
                return declared;
            }
        }
        if let Some(pdm) = tool.get("pdm").and_then(Item::as_table_like) {
            let declared = Self::pdm_indexes(pdm);
            if !declared.chain.is_empty() {
                return declared;
            }
        }
        DeclaredIndexes::default()
    }

    /// uv: every `[[tool.uv.index]]` entry is consulted before the default index
    /// in declaration order. Only `default = true` replaces PyPI; `explicit =
    /// true` restricts an index to packages pinned to it through
    /// `[tool.uv.sources]`. The legacy `[tool.uv] index-url` and
    /// `extra-index-url` keys are the unnamed forms of the same two roles.
    fn uv_indexes(uv: &dyn TableLike) -> DeclaredIndexes {
        let mut before_default: Vec<DeclaredIndex> = Vec::new();
        let mut explicit: Vec<DeclaredIndex> = Vec::new();
        let mut default_index: Option<DeclaredIndex> = None;
        let mut default_replaced = false;

        for index in table_entries(uv, "index") {
            let Some(url) = table_str(index, "url") else {
                continue;
            };
            let name = table_str(index, "name");
            let is_default = table_bool(index, "default");
            let is_explicit = table_bool(index, "explicit");
            let declared = DeclaredIndex::url(name, url);

            if is_explicit {
                // A default+explicit index removes PyPI as the default and
                // is still only reachable through pins.
                default_replaced |= is_default;
                explicit.push(declared.explicit());
            } else if is_default && default_index.is_none() {
                default_index = Some(declared);
            } else {
                before_default.push(declared);
            }
        }

        if let Some(Item::Value(Value::Array(urls))) = uv.get("extra-index-url") {
            for url in urls.iter().filter_map(|u| u.as_str()) {
                if !url.is_empty() {
                    before_default.push(DeclaredIndex::url(None, url));
                }
            }
        }
        if let Some(Item::Value(Value::String(url))) = uv.get("index-url")
            && !url.value().is_empty()
            && default_index.is_none()
            && !default_replaced
        {
            default_index = Some(DeclaredIndex::url(None, url.value()));
        }

        if before_default.is_empty()
            && explicit.is_empty()
            && default_index.is_none()
            && !default_replaced
        {
            return DeclaredIndexes::default();
        }

        let mut chain = before_default;
        match default_index {
            Some(index) => chain.push(index),
            None if !default_replaced => chain.push(DeclaredIndex::default_registry()),
            None => {}
        }
        chain.extend(explicit);

        DeclaredIndexes {
            chain,
            pins: Self::uv_source_pins(uv),
        }
    }

    /// `[tool.uv.sources]` entries of the form `pkg = { index = "name" }`, or a
    /// list of such tables (the first `index` entry wins; markers are not
    /// evaluated). Git, path and URL sources are not index pins.
    fn uv_source_pins(uv: &dyn TableLike) -> HashMap<String, String> {
        let mut pins = HashMap::new();
        let Some(sources) = uv.get("sources").and_then(|s| s.as_table_like()) else {
            return pins;
        };
        for (package, source) in sources.iter() {
            let index = match source {
                Item::Value(Value::InlineTable(table)) => table
                    .get("index")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                Item::Table(table) => table_str(table, "index").map(str::to_string),
                Item::Value(Value::Array(alternatives)) => alternatives.iter().find_map(|alt| {
                    alt.as_inline_table()
                        .and_then(|t| t.get("index"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                }),
                _ => None,
            };
            if let Some(index) = index {
                pins.insert(package.to_string(), index);
            }
        }
        pins
    }

    /// Poetry: `[[tool.poetry.source]]` entries are searched by priority.
    /// Primary sources (`priority = "primary"`, the default when omitted, or the
    /// legacy `default = true`) come first and disable the implicit PyPI, unless
    /// PyPI itself is listed as a primary source (by name, without a URL).
    /// Supplemental sources (`priority = "supplemental"` or the legacy
    /// `secondary = true`) follow PyPI. Explicit sources are only used for
    /// dependencies that name them, which the updater does not model, so they
    /// stay out of the chain.
    fn poetry_indexes(poetry: &dyn TableLike) -> DeclaredIndexes {
        let mut primary: Vec<DeclaredIndex> = Vec::new();
        let mut supplemental: Vec<DeclaredIndex> = Vec::new();
        let mut explicit: Vec<DeclaredIndex> = Vec::new();

        for source in table_entries(poetry, "source") {
            let name = table_str(source, "name");
            let declared = match table_str(source, "url") {
                Some(url) => DeclaredIndex::url(name, url),
                None if name.is_some_and(|n| n.eq_ignore_ascii_case("pypi")) => DeclaredIndex {
                    name: name.map(str::to_string),
                    ..DeclaredIndex::default_registry()
                },
                None => continue,
            };

            let priority = table_str(source, "priority").map(str::to_ascii_lowercase);
            let priority = match priority.as_deref() {
                Some(p) => p.to_string(),
                None if table_bool(source, "secondary") => "supplemental".to_string(),
                None => "primary".to_string(),
            };
            match priority.as_str() {
                "supplemental" | "secondary" => supplemental.push(declared),
                "explicit" => explicit.push(declared.explicit()),
                _ => primary.push(declared),
            }
        }

        if primary.is_empty() && supplemental.is_empty() && explicit.is_empty() {
            return DeclaredIndexes::default();
        }

        let mut chain = primary;
        if chain.is_empty() {
            chain.push(DeclaredIndex::default_registry());
        }
        chain.extend(supplemental);
        chain.extend(explicit);
        DeclaredIndexes {
            chain,
            pins: HashMap::new(),
        }
    }

    /// PDM: the default PyPI comes first, then every `[[tool.pdm.source]]` of
    /// type `index` in declaration order. A source named `pypi` replaces the
    /// default and takes its declared position; `find_links` sources are not
    /// indexes.
    fn pdm_indexes(pdm: &dyn TableLike) -> DeclaredIndexes {
        let mut declared: Vec<DeclaredIndex> = Vec::new();
        let mut replaces_default = false;
        for source in table_entries(pdm, "source") {
            if table_str(source, "type").is_some_and(|t| t == "find_links") {
                continue;
            }
            let Some(url) = table_str(source, "url") else {
                continue;
            };
            let name = table_str(source, "name");
            replaces_default |= name.is_some_and(|n| n.eq_ignore_ascii_case("pypi"));
            declared.push(DeclaredIndex::url(name, url).with_package_filters(
                table_str_array(source, "include_packages"),
                table_str_array(source, "exclude_packages"),
            ));
        }

        if declared.is_empty() {
            return DeclaredIndexes::default();
        }

        let mut chain = Vec::new();
        if !replaces_default {
            chain.push(DeclaredIndex::default_registry());
        }
        chain.extend(declared);
        DeclaredIndexes {
            chain,
            pins: HashMap::new(),
        }
    }

    async fn update_array_deps(
        &self,
        array: &mut toml_edit::Array,
        registry: &dyn Registry,
        result: &mut UpdateResult,
        manifest: &ManifestContext<'_>,
        section_path: &str,
        options: &UpdateOptions,
    ) {
        if let Some(operator) = section_operator(section_path, options) {
            self.normalize_array_deps(
                array,
                registry,
                result,
                manifest,
                options,
                NormalizeSection {
                    path: section_path,
                    operator,
                },
            )
            .await;
            return;
        }

        // First pass: collect all dependencies and separate by config status
        let mut ignored_deps: Vec<(String, String, Option<usize>)> = Vec::new();
        let mut pinned_deps: Vec<(usize, String, String, String, String, Option<usize>)> =
            Vec::new();
        let mut deps_to_check: Vec<(usize, String, ParsedDep, Option<usize>)> = Vec::new();

        for i in 0..array.len() {
            if let Some(item) = array.get(i)
                && let Some(s) = item.as_str()
                && let Some(parsed) = self.parse_dependency(s)
            {
                let line_num = manifest.line_index.line_for(section_path, &parsed.package);

                if options.is_package_filtered_out(&parsed.package) {
                    result.unchanged += 1;
                    continue;
                }

                // Check if package should be ignored
                if options.should_ignore(&parsed.package) {
                    ignored_deps.push((parsed.package, parsed.version, line_num));
                    continue;
                }

                // Check if package has a pinned version
                if let Some(pinned_version) = options.get_pinned_version(&parsed.package) {
                    if !parsed.raisable {
                        // The pin was configured and cannot be written, so the
                        // manifest does not say what the config says it should.
                        // That is a failed instruction, not a note.
                        result.errors.push(unpinnable_error(
                            &parsed.package,
                            pinned_version,
                            &parsed.full_constraint,
                        ));
                        continue;
                    }
                    pinned_deps.push((
                        i,
                        s.to_string(),
                        parsed.package,
                        parsed.version,
                        pinned_version.to_string(),
                        line_num,
                    ));
                    continue;
                }

                deps_to_check.push((i, s.to_string(), parsed, line_num));
            }
        }

        // Record ignored packages
        for (package, version, line_num) in ignored_deps {
            result.ignored.push((package, version, line_num));
        }

        // Process pinned packages (no registry fetch needed)
        let mut updates: Vec<(usize, String)> = Vec::new();
        for (i, dep_str, package, current_version, pinned_version, line_num) in pinned_deps {
            let matched_version = if options.full_precision {
                pinned_version.clone()
            } else {
                match_version_precision(&current_version, &pinned_version)
            };

            if matched_version != current_version {
                let updated = self.update_dependency(&dep_str, &matched_version);
                result
                    .pinned
                    .push((package, current_version, matched_version.clone(), line_num));
                updates.push((i, updated));
            } else {
                result.unchanged += 1;
            }
        }

        // Fetch versions for remaining deps in parallel
        let version_futures: Vec<_> = deps_to_check
            .iter()
            .map(|(_, _, parsed, _)| async {
                if !parsed.raisable {
                    // Nothing here can be rewritten, so the only question left
                    // is whether the newest release is one this specifier
                    // already admits. Asking for the newest release *matching*
                    // it would answer that with itself.
                    registry.get_latest_version(&parsed.package).await
                } else if !is_stable_pep440(&parsed.version) {
                    python_version_with_revalidation(
                        registry,
                        &parsed.package,
                        &parsed.version,
                        VersionQuery::IncludingPrereleases,
                    )
                    .await
                } else if Self::is_simple_constraint(&parsed.full_constraint) {
                    python_version_with_revalidation(
                        registry,
                        &parsed.package,
                        &parsed.version,
                        VersionQuery::Stable,
                    )
                    .await
                } else {
                    python_version_with_revalidation(
                        registry,
                        &parsed.package,
                        &parsed.version,
                        VersionQuery::Matching(&parsed.full_constraint),
                    )
                    .await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results and collect updates
        for ((i, dep_str, parsed, line_num), version_result) in
            deps_to_check.into_iter().zip(version_results)
        {
            let ParsedDep {
                package,
                version: current_version,
                full_constraint,
                raisable,
            } = parsed;

            // A specifier with no floor to raise (a ceiling like "<6", an
            // exclusive bound like ">2.0", an exclusion like "!=1.5") is still
            // one upd can read: saying whether it is current is the difference
            // between a specifier doing its job and one that has quietly frozen
            // a dependency.
            if !raisable {
                match version_result {
                    Ok(latest) => match pep440_admits(&full_constraint, &latest) {
                        Some(true) => result.unchanged += 1,
                        Some(false) => result.warnings.push(unrewritable_warning(
                            &package,
                            &latest,
                            &full_constraint,
                        )),
                        None => result
                            .errors
                            .push(unreadable_error(&package, &full_constraint)),
                    },
                    Err(e) => result.errors.push(format!("{}: {}", package, e)),
                }
                continue;
            }

            match version_result {
                Ok(latest_version) => {
                    // When the current version is a pre-release, we fetched the latest
                    // pre-release. If the registry returned a stable version instead
                    // (no newer pre-release exists), refuse silent promotion to stable.
                    let current_is_prerelease = is_prerelease_pep440(&current_version);
                    if current_is_prerelease && !is_prerelease_pep440(&latest_version) {
                        result.unchanged += 1;
                        continue;
                    }

                    let constraints_for_cooldown = if full_constraint.is_empty() {
                        None
                    } else {
                        Some(full_constraint.as_str())
                    };
                    let (outcome, note) = crate::updater::apply_cooldown(
                        registry,
                        &package,
                        &current_version,
                        &latest_version,
                        constraints_for_cooldown,
                        current_is_prerelease,
                        options,
                    )
                    .await;
                    if let Some(msg) = note {
                        options.note_cooldown_unavailable(&msg);
                    }
                    let (latest_version, held_back_record) = match outcome {
                        crate::updater::CooldownOutcome::Unchanged(v) => (v, None),
                        crate::updater::CooldownOutcome::HeldBack {
                            chosen,
                            skipped_version,
                            skipped_published_at,
                        } => (chosen, Some((skipped_version, skipped_published_at))),
                        crate::updater::CooldownOutcome::Skipped {
                            skipped_version,
                            skipped_published_at,
                        } => {
                            result.skipped_by_cooldown.push((
                                package,
                                current_version,
                                skipped_version,
                                skipped_published_at,
                            ));
                            continue;
                        }
                    };

                    // Match the precision of the original version (unless full precision requested)
                    let matched_version = if options.full_precision {
                        latest_version.clone()
                    } else {
                        match_version_precision(&current_version, &latest_version)
                    };
                    if matched_version != current_version {
                        // Refuse to write a downgrade.
                        if compare_versions(&matched_version, &current_version, Lang::Python)
                            != std::cmp::Ordering::Greater
                        {
                            result.warnings.push(downgrade_warning(
                                &package,
                                &matched_version,
                                &current_version,
                            ));
                            result.unchanged += 1;
                        } else if !options.allows_bump(&current_version, &matched_version) {
                            // Bump level exceeds the --only-bump/--max-bump ceiling.
                            result.record_capped(
                                &package,
                                &current_version,
                                &matched_version,
                                line_num,
                            );
                        } else {
                            let updated = self.update_dependency(&dep_str, &matched_version);
                            result.updated.push((
                                package.clone(),
                                current_version.clone(),
                                matched_version.clone(),
                                line_num,
                            ));
                            if let Some((skipped_version, skipped_published_at)) = held_back_record
                            {
                                result.held_back.push((
                                    package,
                                    current_version,
                                    matched_version,
                                    skipped_version,
                                    skipped_published_at,
                                ));
                            }
                            updates.push((i, updated));
                        }
                    } else {
                        result.unchanged += 1;
                    }
                }
                Err(e) => {
                    result.errors.push(format!("{}: {}", package, e));
                }
            }
        }

        // Apply updates, preserving decoration (comments, whitespace)
        for (i, updated) in updates {
            if let Some(item) = array.get_mut(i) {
                // Preserve the original decoration (prefix/suffix whitespace and comments)
                if let Value::String(formatted) = item {
                    let decor = formatted.decor().clone();
                    let mut new_formatted = Formatted::new(updated);
                    *new_formatted.decor_mut() = decor;
                    *formatted = new_formatted;
                } else {
                    *item = Value::from(updated);
                }
            }
        }
    }

    async fn normalize_array_deps(
        &self,
        array: &mut toml_edit::Array,
        registry: &dyn Registry,
        result: &mut UpdateResult,
        manifest: &ManifestContext<'_>,
        options: &UpdateOptions,
        section: NormalizeSection<'_>,
    ) {
        let operator = section.operator.as_str();
        let mut to_resolve: Vec<(usize, String, NormalizeTarget, Option<usize>)> = Vec::new();
        let mut rewrites: Vec<(usize, String)> = Vec::new();
        let mut occurrences: HashMap<String, usize> = HashMap::new();

        for i in 0..array.len() {
            let Some(dep) = array.get(i).and_then(Value::as_str) else {
                continue;
            };
            let spans_lines = matches!(
                array.get(i),
                Some(Value::String(formatted))
                    if formatted
                        .as_repr()
                        .and_then(|repr| repr.as_raw().as_str())
                        .is_some_and(|raw| raw.contains('\n'))
            );
            let occurrence = if spans_lines {
                None
            } else {
                Some(
                    *occurrences
                        .entry(dep.to_owned())
                        .and_modify(|seen| *seen += 1)
                        .or_insert(0),
                )
            };
            let Some(target) = self.parse_normalizable(dep) else {
                continue;
            };
            result.normalize_recognized.push(target.package.clone());
            if manifest
                .non_registry
                .contains(&pep503_normalize(&target.package))
            {
                continue;
            }
            let line_num = occurrence.and_then(|occurrence| {
                manifest
                    .line_index
                    .line_for_entry(section.path, dep, occurrence)
            });

            if options.is_package_filtered_out(&target.package) {
                if target.version.is_some() {
                    result.unchanged += 1;
                }
                continue;
            }
            if options.should_ignore(&target.package) {
                if let Some(version) = target.version {
                    result.ignored.push((target.package, version, line_num));
                }
                continue;
            }

            if let Some(pin) = options.get_pinned_version(&target.package) {
                if !is_pep440_version(pin) {
                    result.errors.push(format!(
                        "cannot normalize '{}': pin '{}' is not a version, and [normalize.pyproject] writes {}<version>",
                        target.package, pin, operator
                    ));
                    continue;
                }
                if section.operator != SpecifierOperator::Exact && has_local_label(pin) {
                    result.errors.push(format!(
                        "cannot normalize '{}': pin '{}' carries a local label, which {} does not take",
                        target.package, pin, operator
                    ));
                    continue;
                }
                let spec = format!("{operator}{pin}");
                match classify_rewrite(&target, operator, pin) {
                    Rewrite::Unchanged => result.unchanged += 1,
                    Rewrite::SameShape => {
                        let (_, existing) = target.single_clause.clone().unwrap();
                        result.pinned.push((
                            target.package.clone(),
                            existing,
                            pin.to_string(),
                            line_num,
                        ));
                        rewrites.push((i, with_specifier(dep, &target, &spec)));
                    }
                    Rewrite::NewShape => {
                        result.normalized.push(NormalizedSpec {
                            package: target.package.clone(),
                            section: section.path.to_string(),
                            previous_spec: target.previous_spec.clone(),
                            new_spec: spec.clone(),
                            version: pin.to_string(),
                            previous_version: target.interactive_version.clone(),
                            pinned: true,
                            held_back_from: None,
                            line_number: line_num,
                        });
                        rewrites.push((i, with_specifier(dep, &target, &spec)));
                    }
                }
                continue;
            }

            to_resolve.push((i, dep.to_string(), target, line_num));
        }

        let lookups: Vec<_> = to_resolve
            .iter()
            .map(|(_, _, target, _)| async {
                match target.anchor.as_deref() {
                    Some(anchor) if !is_stable_pep440(anchor) => {
                        python_version_with_revalidation(
                            registry,
                            &target.package,
                            anchor,
                            VersionQuery::IncludingPrereleases,
                        )
                        .await
                    }
                    Some(anchor) => {
                        python_version_with_revalidation(
                            registry,
                            &target.package,
                            anchor,
                            VersionQuery::Stable,
                        )
                        .await
                    }
                    _ => registry.get_latest_version(&target.package).await,
                }
            })
            .collect();
        let resolved = join_all(lookups).await;

        for ((i, dep, target, line_num), lookup) in to_resolve.into_iter().zip(resolved) {
            let latest = match lookup {
                Ok(latest) => latest,
                Err(error) => {
                    result.errors.push(format!("{}: {}", target.package, error));
                    continue;
                }
            };

            if section.operator != SpecifierOperator::Exact && has_local_label(&latest) {
                result.warnings.push(format!(
                    "'{}': the newest release, {}, carries a local label, which {} does not take; left as written",
                    target.package, latest, operator
                ));
                result.unchanged += 1;
                continue;
            }

            let (outcome, note) = match target.anchor.as_deref() {
                Some(anchor) => {
                    crate::updater::apply_cooldown(
                        registry,
                        &target.package,
                        anchor,
                        &latest,
                        None,
                        is_prerelease_pep440(&latest),
                        options,
                    )
                    .await
                }
                None => {
                    apply_cooldown_without_floor(
                        registry,
                        &target.package,
                        &latest,
                        is_prerelease_pep440(&latest),
                        options,
                    )
                    .await
                }
            };
            if let Some(note) = note {
                options.note_cooldown_unavailable(&note);
            }
            let (chosen, held_back) = match outcome {
                crate::updater::CooldownOutcome::Unchanged(version) => (version, None),
                crate::updater::CooldownOutcome::HeldBack {
                    chosen,
                    skipped_version,
                    skipped_published_at,
                } => (chosen, Some((skipped_version, skipped_published_at))),
                crate::updater::CooldownOutcome::Skipped {
                    skipped_version,
                    skipped_published_at,
                } => {
                    if let Some(anchor) = target.anchor {
                        result.skipped_by_cooldown.push((
                            target.package,
                            anchor,
                            skipped_version,
                            skipped_published_at,
                        ));
                    } else {
                        result.warnings.push(format!(
                            "'{}' names no release to keep ({}) and the newest one, {}, is inside the cooldown window; left as written",
                            target.package,
                            target.previous_spec.as_deref().unwrap_or("no specifier"),
                            skipped_version
                        ));
                    }
                    continue;
                }
            };

            if section.operator != SpecifierOperator::Exact && has_local_label(&chosen) {
                result.warnings.push(format!(
                    "'{}': cooldown selects {}, which carries a local label that {} does not take; left as written",
                    target.package, chosen, operator
                ));
                result.unchanged += 1;
                continue;
            }

            if let Some(anchor) = &target.anchor {
                if compare_versions(&chosen, anchor, Lang::Python) == std::cmp::Ordering::Less {
                    result
                        .warnings
                        .push(downgrade_warning(&target.package, &chosen, anchor));
                    result.unchanged += 1;
                    continue;
                }
                if chosen != *anchor && !options.allows_bump(anchor, &chosen) {
                    result.record_capped(&target.package, anchor, &chosen, line_num);
                    continue;
                }
            }

            let spec = format!("{operator}{chosen}");
            match classify_rewrite(&target, operator, &chosen) {
                Rewrite::Unchanged => {
                    if let Some((skipped_version, _)) = &held_back {
                        result.warnings.push(format!(
                            "'{}' is left as written ({}): the newest release, {}, is inside the cooldown window",
                            target.package,
                            target.previous_spec.as_deref().unwrap_or("no specifier"),
                            skipped_version
                        ));
                    }
                    result.unchanged += 1;
                }
                Rewrite::SameShape => {
                    let (_, existing) = target.single_clause.clone().unwrap();
                    result.updated.push((
                        target.package.clone(),
                        existing.clone(),
                        chosen.clone(),
                        line_num,
                    ));
                    if let Some((skipped_version, skipped_published_at)) = held_back {
                        result.held_back.push((
                            target.package.clone(),
                            existing,
                            chosen,
                            skipped_version,
                            skipped_published_at,
                        ));
                    }
                    rewrites.push((i, with_specifier(&dep, &target, &spec)));
                }
                Rewrite::NewShape => {
                    result.normalized.push(NormalizedSpec {
                        package: target.package.clone(),
                        section: section.path.to_string(),
                        previous_spec: target.previous_spec.clone(),
                        new_spec: spec.clone(),
                        version: chosen,
                        previous_version: target.interactive_version.clone(),
                        pinned: false,
                        held_back_from: held_back,
                        line_number: line_num,
                    });
                    rewrites.push((i, with_specifier(&dep, &target, &spec)));
                }
            }
        }

        for (i, rewritten) in rewrites {
            if let Some(Value::String(formatted)) = array.get_mut(i) {
                replace_string_value(formatted, rewritten);
            }
        }
    }

    async fn update_poetry_deps(
        &self,
        deps_table: &mut toml_edit::Table,
        registry: &dyn Registry,
        result: &mut UpdateResult,
        line_index: &PyProjectLineIndex,
        section_path: &str,
        options: &UpdateOptions,
    ) {
        // First pass: collect dependencies and separate by config status
        let mut ignored_deps: Vec<(String, String, Option<usize>)> = Vec::new();
        let mut pinned_deps: Vec<(String, String, String, String, Option<usize>)> = Vec::new();
        let mut deps_to_check: Vec<(String, String, String, Option<usize>)> = Vec::new();

        for (key, item) in deps_table.iter() {
            if key == "python" {
                continue;
            }

            if let Item::Value(Value::String(s)) = item {
                let version_str = s.value().to_string();
                let (prefix, version) =
                    if version_str.starts_with('^') || version_str.starts_with('~') {
                        (version_str[..1].to_string(), version_str[1..].to_string())
                    } else {
                        (String::new(), version_str.clone())
                    };

                let package = key.to_string();
                let line_num = line_index.line_for(section_path, &package);

                if options.is_package_filtered_out(&package) {
                    result.unchanged += 1;
                    continue;
                }

                // Check if package should be ignored
                if options.should_ignore(&package) {
                    ignored_deps.push((package, version, line_num));
                    continue;
                }

                // Check if package has a pinned version
                if let Some(pinned_version) = options.get_pinned_version(&package) {
                    pinned_deps.push((
                        package,
                        prefix,
                        version,
                        pinned_version.to_string(),
                        line_num,
                    ));
                    continue;
                }

                deps_to_check.push((package, prefix, version, line_num));
            }
        }

        // Record ignored packages
        for (package, version, line_num) in ignored_deps {
            result.ignored.push((package, version, line_num));
        }

        // Process pinned packages (no registry fetch needed)
        for (key, prefix, version, pinned_version, line_num) in pinned_deps {
            let matched_version = if options.full_precision {
                pinned_version.clone()
            } else {
                match_version_precision(&version, &pinned_version)
            };

            if matched_version != version {
                let new_val = format!("{}{}", prefix, matched_version);
                result
                    .pinned
                    .push((key.clone(), version, matched_version.clone(), line_num));

                // Preserve decoration when updating
                if let Some(Item::Value(Value::String(formatted))) = deps_table.get_mut(&key) {
                    let decor = formatted.decor().clone();
                    let mut new_formatted = Formatted::new(new_val);
                    *new_formatted.decor_mut() = decor;
                    *formatted = new_formatted;
                }
            } else {
                result.unchanged += 1;
            }
        }

        // Fetch versions for remaining deps in parallel
        let version_futures: Vec<_> = deps_to_check
            .iter()
            .map(|(key, _, version, _)| async {
                if is_stable_pep440(version) {
                    python_version_with_revalidation(registry, key, version, VersionQuery::Stable)
                        .await
                } else {
                    python_version_with_revalidation(
                        registry,
                        key,
                        version,
                        VersionQuery::IncludingPrereleases,
                    )
                    .await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results
        for ((key, prefix, version, line_num), version_result) in
            deps_to_check.into_iter().zip(version_results)
        {
            match version_result {
                Ok(latest_version) => {
                    // When the current version is a pre-release, we fetched the latest
                    // pre-release. If the registry returned a stable version instead
                    // (no newer pre-release exists), refuse silent promotion to stable.
                    let current_is_prerelease = is_prerelease_pep440(&version);
                    if current_is_prerelease && !is_prerelease_pep440(&latest_version) {
                        result.unchanged += 1;
                        continue;
                    }

                    let full_constraint = format!("{}{}", prefix, version);
                    let constraints_for_cooldown = if full_constraint.is_empty() {
                        None
                    } else {
                        Some(full_constraint.as_str())
                    };
                    let (outcome, note) = crate::updater::apply_cooldown(
                        registry,
                        &key,
                        &version,
                        &latest_version,
                        constraints_for_cooldown,
                        current_is_prerelease,
                        options,
                    )
                    .await;
                    if let Some(msg) = note {
                        options.note_cooldown_unavailable(&msg);
                    }
                    let (latest_version, held_back_record) = match outcome {
                        crate::updater::CooldownOutcome::Unchanged(v) => (v, None),
                        crate::updater::CooldownOutcome::HeldBack {
                            chosen,
                            skipped_version,
                            skipped_published_at,
                        } => (chosen, Some((skipped_version, skipped_published_at))),
                        crate::updater::CooldownOutcome::Skipped {
                            skipped_version,
                            skipped_published_at,
                        } => {
                            result.skipped_by_cooldown.push((
                                key,
                                version,
                                skipped_version,
                                skipped_published_at,
                            ));
                            continue;
                        }
                    };

                    // Match the precision of the original version (unless full precision requested)
                    let matched_version = if options.full_precision {
                        latest_version.clone()
                    } else {
                        match_version_precision(&version, &latest_version)
                    };
                    if matched_version != version {
                        // Refuse to write a downgrade.
                        if compare_versions(&matched_version, &version, Lang::Python)
                            != std::cmp::Ordering::Greater
                        {
                            result.warnings.push(downgrade_warning(
                                &key,
                                &matched_version,
                                &version,
                            ));
                            result.unchanged += 1;
                        } else if !options.allows_bump(&version, &matched_version) {
                            // Bump level exceeds the --only-bump/--max-bump ceiling.
                            result.record_capped(&key, &version, &matched_version, line_num);
                        } else {
                            let new_val = format!("{}{}", prefix, matched_version);
                            result.updated.push((
                                key.clone(),
                                version.clone(),
                                matched_version.clone(),
                                line_num,
                            ));
                            if let Some((skipped_version, skipped_published_at)) = held_back_record
                            {
                                result.held_back.push((
                                    key.clone(),
                                    version,
                                    matched_version.clone(),
                                    skipped_version,
                                    skipped_published_at,
                                ));
                            }

                            // Preserve decoration when updating
                            if let Some(Item::Value(Value::String(formatted))) =
                                deps_table.get_mut(&key)
                            {
                                let decor = formatted.decor().clone();
                                let mut new_formatted = Formatted::new(new_val);
                                *new_formatted.decor_mut() = decor;
                                *formatted = new_formatted;
                            }
                        }
                    } else {
                        result.unchanged += 1;
                    }
                }
                Err(e) => {
                    result.errors.push(format!("{}: {}", key, e));
                }
            }
        }
    }
}

impl PyProjectLineIndex {
    fn record_dependency_literals(
        index: &mut PyProjectLineIndex,
        section_path: &str,
        line: &str,
        table_depth: &mut usize,
        literal_re: &Regex,
        updater: &PyProjectUpdater,
        line_num: usize,
    ) {
        let code = Self::code_before_comment(line);
        let mut scanned_to = 0;
        for found in literal_re.find_iter(code) {
            Self::count_table_braces(&code[scanned_to..found.start()], table_depth);
            scanned_to = found.end();
            if *table_depth > 0 {
                continue;
            }
            let Some(dep) = found
                .as_str()
                .parse::<Value>()
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
            else {
                continue;
            };
            index
                .lines_by_entry
                .entry(section_path.to_string())
                .or_default()
                .entry(dep.clone())
                .or_default()
                .push(line_num);
            if let Some(parsed) = updater.parse_dependency(&dep) {
                index
                    .lines_by_section
                    .entry(section_path.to_string())
                    .or_default()
                    .entry(parsed.package)
                    .or_insert(line_num);
            }
        }
        Self::count_table_braces(&code[scanned_to..], table_depth);
    }

    fn count_table_braces(code: &str, depth: &mut usize) {
        for ch in code.chars() {
            match ch {
                '{' => *depth += 1,
                '}' => *depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }

    fn code_before_comment(line: &str) -> &str {
        let mut in_basic = false;
        let mut in_literal = false;
        let mut chars = line.char_indices();
        while let Some((index, ch)) = chars.next() {
            match ch {
                '#' if !in_basic && !in_literal => return &line[..index],
                '"' if !in_literal => in_basic = !in_basic,
                '\'' if !in_basic => in_literal = !in_literal,
                '\\' if in_basic => {
                    let _ = chars.next();
                }
                _ => {}
            }
        }
        line
    }

    fn count_structural_array_brackets(line: &str) -> ArrayBracketCounts {
        #[derive(Clone, Copy)]
        enum ScanState {
            Normal,
            BasicString,
            LiteralString,
        }

        let mut counts = ArrayBracketCounts::default();
        let mut state = ScanState::Normal;
        let mut chars = line.chars();

        while let Some(ch) = chars.next() {
            match state {
                ScanState::Normal => match ch {
                    '#' => break,
                    '"' => state = ScanState::BasicString,
                    '\'' => state = ScanState::LiteralString,
                    '[' => counts.opening += 1,
                    ']' => counts.closing += 1,
                    _ => {}
                },
                ScanState::BasicString => match ch {
                    '\\' => {
                        let _ = chars.next();
                    }
                    '"' => state = ScanState::Normal,
                    _ => {}
                },
                ScanState::LiteralString => {
                    if ch == '\'' {
                        state = ScanState::Normal;
                    }
                }
            }
        }

        counts
    }

    fn mask_multi_line_strings(
        line: &str,
        mut state: MultiLineString,
    ) -> (String, MultiLineString) {
        let mut masked = String::with_capacity(line.len());
        let mut rest = line;
        loop {
            if state != MultiLineString::Outside {
                let Some(end) = Self::multi_line_closer_at(rest, state) else {
                    masked.extend(rest.chars().map(|_| ' '));
                    return (masked, state);
                };
                let through = end + 3;
                masked.extend(rest[..through].chars().map(|_| ' '));
                rest = &rest[through..];
                state = MultiLineString::Outside;
                continue;
            }

            let Some((index, ch)) = rest
                .char_indices()
                .find(|&(_, ch)| matches!(ch, '#' | '"' | '\''))
            else {
                masked.push_str(rest);
                return (masked, state);
            };
            let after = &rest[index..];
            let opened = if after.starts_with("\"\"\"") {
                Some(MultiLineString::Basic)
            } else if after.starts_with("'''") {
                Some(MultiLineString::Literal)
            } else {
                None
            };
            if let Some(opened) = opened {
                if let Some(end) = Self::multi_line_closer_at(&after[3..], opened) {
                    let through = index + 3 + end + 3;
                    masked.push_str(&rest[..index]);
                    Self::push_as_single_line_string(&mut masked, &rest[index..through], opened);
                    rest = &rest[through..];
                } else {
                    masked.push_str(&rest[..index]);
                    masked.extend(after.chars().map(|_| ' '));
                    return (masked, opened);
                }
            } else if ch == '#' {
                masked.push_str(rest);
                return (masked, state);
            } else {
                let remainder = Self::after_single_line_string(&after[1..], ch);
                let through = rest.len() - remainder.len();
                masked.push_str(&rest[..through]);
                rest = remainder;
            }
        }
    }

    fn multi_line_closer_at(rest: &str, state: MultiLineString) -> Option<usize> {
        if state == MultiLineString::Literal {
            let index = rest.find("'''")?;
            return Some(Self::last_three_of_quote_run(rest, index, '\''));
        }
        let mut chars = rest.char_indices();
        while let Some((index, ch)) = chars.next() {
            match ch {
                '\\' => {
                    let _ = chars.next();
                }
                '"' if rest[index..].starts_with("\"\"\"") => {
                    return Some(Self::last_three_of_quote_run(rest, index, '"'));
                }
                _ => {}
            }
        }
        None
    }

    fn last_three_of_quote_run(rest: &str, index: usize, quote: char) -> usize {
        let run = rest[index..].chars().take_while(|&ch| ch == quote).count();
        index + run.min(5) - 3
    }

    fn after_single_line_string(body: &str, quote: char) -> &str {
        let mut chars = body.char_indices();
        while let Some((index, ch)) = chars.next() {
            if ch == quote {
                return &body[index + 1..];
            }
            if ch == '\\' && quote == '"' {
                let _ = chars.next();
            }
        }
        ""
    }

    fn push_as_single_line_string(masked: &mut String, source: &str, kind: MultiLineString) {
        let content = &source[3..source.len() - 3];
        let mut rendered = String::with_capacity(source.len());
        rendered.push('"');
        if kind == MultiLineString::Literal {
            for ch in content.chars() {
                if matches!(ch, '\\' | '"') {
                    rendered.push('\\');
                }
                rendered.push(ch);
            }
        } else {
            let mut chars = content.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' => {
                        rendered.push(ch);
                        if let Some(escaped) = chars.next() {
                            rendered.push(escaped);
                        }
                    }
                    '"' => rendered.push_str("\\\""),
                    _ => rendered.push(ch),
                }
            }
        }
        rendered.push('"');
        masked.push_str(&rendered);
        for _ in rendered.chars().count()..source.chars().count() {
            masked.push(' ');
        }
    }

    fn from_content(content: &str, updater: &PyProjectUpdater) -> Self {
        let section_re =
            Regex::new(r#"^\s*\[([^\]]+)\]\s*(?:#.*)?$"#).expect("Invalid pyproject section regex");
        let literal_re =
            Regex::new(r#""(?:[^"\\]|\\.)+"|'[^']+'"#).expect("Invalid dependency literal regex");
        let mut index = Self::default();
        let mut current_section: Option<String> = None;
        let mut current_array_section: Option<ArraySectionState> = None;
        let mut multi_line = MultiLineString::Outside;

        for (line_idx, line) in content.lines().enumerate() {
            let (masked, next_state) = Self::mask_multi_line_strings(line, multi_line);
            multi_line = next_state;
            let line = masked.as_str();

            if let Some(caps) = section_re.captures(line) {
                current_section = Some(caps.get(1).unwrap().as_str().to_string());
                current_array_section = None;
                continue;
            }

            if let Some(array_state) = current_array_section.as_mut() {
                Self::record_dependency_literals(
                    &mut index,
                    &array_state.section_path,
                    line,
                    &mut array_state.table_depth,
                    &literal_re,
                    updater,
                    line_idx + 1,
                );

                let brackets = Self::count_structural_array_brackets(line);
                let next_depth = array_state.depth as isize + brackets.opening as isize
                    - brackets.closing as isize;
                if next_depth <= 0 {
                    current_array_section = None;
                } else {
                    array_state.depth = next_depth as usize;
                }

                continue;
            }

            let Some(section) = current_section.as_deref() else {
                continue;
            };

            match section {
                "project" => {
                    if let Some((key, value)) = PyProjectUpdater::assignment_parts(line)
                        && key == "dependencies"
                    {
                        let brackets = Self::count_structural_array_brackets(value);
                        if brackets.opening == 0 {
                            continue;
                        }

                        let section_path = "project.dependencies".to_string();
                        let mut table_depth = 0;
                        Self::record_dependency_literals(
                            &mut index,
                            &section_path,
                            value,
                            &mut table_depth,
                            &literal_re,
                            updater,
                            line_idx + 1,
                        );

                        let depth = brackets.opening.saturating_sub(brackets.closing);
                        if depth > 0 {
                            current_array_section = Some(ArraySectionState {
                                section_path,
                                depth,
                                table_depth,
                            });
                        }
                    }
                }
                "project.optional-dependencies" | "dependency-groups" => {
                    if let Some((group, value)) = PyProjectUpdater::assignment_parts(line) {
                        let brackets = Self::count_structural_array_brackets(value);
                        if brackets.opening == 0 {
                            continue;
                        }

                        let section_path = format!("{}.{}", section, group);
                        let mut table_depth = 0;
                        Self::record_dependency_literals(
                            &mut index,
                            &section_path,
                            value,
                            &mut table_depth,
                            &literal_re,
                            updater,
                            line_idx + 1,
                        );

                        let depth = brackets.opening.saturating_sub(brackets.closing);
                        if depth > 0 {
                            current_array_section = Some(ArraySectionState {
                                section_path,
                                depth,
                                table_depth,
                            });
                        }
                    }
                }
                "tool.poetry.dependencies" | "tool.poetry.dev-dependencies" => {
                    if let Some((key, value)) = PyProjectUpdater::assignment_parts(line)
                        && key != "python"
                        && (value.starts_with('"') || value.starts_with('\''))
                    {
                        index
                            .lines_by_section
                            .entry(section.to_string())
                            .or_default()
                            .entry(key)
                            .or_insert(line_idx + 1);
                    }
                }
                _ => {}
            }
        }

        index
    }

    fn line_for(&self, section_path: &str, package: &str) -> Option<usize> {
        self.lines_by_section
            .get(section_path)
            .and_then(|section_lines| section_lines.get(package).copied())
    }

    fn line_for_entry(&self, section_path: &str, entry: &str, occurrence: usize) -> Option<usize> {
        self.lines_by_entry
            .get(section_path)?
            .get(entry)?
            .get(occurrence)
            .copied()
    }
}

impl Default for PyProjectUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for PyProjectUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut doc: DocumentMut = content.parse().map_err(|e: toml_edit::TomlError| {
            anyhow!(
                "Failed to parse {}:\n  {}",
                crate::path_display::display_path(path),
                e.to_string().replace('\n', "\n  ")
            )
        })?;

        let mut result = UpdateResult::default();
        let line_index = PyProjectLineIndex::from_content(&content, self);
        let manifest = ManifestContext {
            line_index: &line_index,
            non_registry: non_registry_sources(&doc),
        };

        // Indexes the manifest declares (uv/Poetry/PDM) are layered over the
        // registry we were handed; only the tool's own replace-the-default rule
        // takes PyPI out of the chain.
        let declared = Self::declared_indexes(&doc);
        let chain = IndexChain::new(declared.chain, &declared.pins, registry);
        let effective_registry: &dyn Registry = match &chain {
            Some(chain) => chain,
            None => registry,
        };

        // Update project dependencies in section or inline-table spelling.
        if let Some(deps) = array_at(&mut doc, &["project", "dependencies"]) {
            self.update_array_deps(
                deps,
                effective_registry,
                &mut result,
                &manifest,
                "project.dependencies",
                &options,
            )
            .await;
        }

        for key in keys_at(&doc, &["project", "optional-dependencies"]) {
            if let Some(deps) = array_at(
                &mut doc,
                &["project", "optional-dependencies", key.as_str()],
            ) {
                let section_path = format!("project.optional-dependencies.{key}");
                self.update_array_deps(
                    deps,
                    effective_registry,
                    &mut result,
                    &manifest,
                    &section_path,
                    &options,
                )
                .await;
            }
        }

        for key in keys_at(&doc, &["dependency-groups"]) {
            if let Some(deps) = array_at(&mut doc, &["dependency-groups", key.as_str()]) {
                let section_path = format!("dependency-groups.{key}");
                self.update_array_deps(
                    deps,
                    effective_registry,
                    &mut result,
                    &manifest,
                    &section_path,
                    &options,
                )
                .await;
            }
        }

        // Update [tool.poetry.dependencies] and [tool.poetry.dev-dependencies]
        if let Some(Item::Table(tool)) = doc.get_mut("tool")
            && let Some(Item::Table(poetry)) = tool.get_mut("poetry")
        {
            if let Some(Item::Table(deps)) = poetry.get_mut("dependencies") {
                self.update_poetry_deps(
                    deps,
                    effective_registry,
                    &mut result,
                    &line_index,
                    "tool.poetry.dependencies",
                    &options,
                )
                .await;
            }

            if let Some(Item::Table(deps)) = poetry.get_mut("dev-dependencies") {
                self.update_poetry_deps(
                    deps,
                    effective_registry,
                    &mut result,
                    &line_index,
                    "tool.poetry.dev-dependencies",
                    &options,
                )
                .await;
            }
        }

        if (!result.updated.is_empty()
            || !result.pinned.is_empty()
            || !result.normalized.is_empty())
            && !options.dry_run
        {
            write_file_atomic(path, &doc.to_string())?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::PyProject
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let doc: DocumentMut = content.parse().map_err(|e: toml_edit::TomlError| {
            anyhow!(
                "Failed to parse {}:\n  {}",
                crate::path_display::display_path(path),
                e.to_string().replace('\n', "\n  ")
            )
        })?;

        let mut deps = Vec::new();
        let line_index = PyProjectLineIndex::from_content(&content, self);

        // Parse [project.dependencies]
        if let Some(Item::Table(project)) = doc.get("project") {
            if let Some(Item::Value(Value::Array(arr))) = project.get("dependencies") {
                for item in arr.iter() {
                    if let Some(s) = item.as_str()
                        && let Some(parsed) = self.parse_dependency(s)
                    {
                        let has_upper_bound = !Self::is_simple_constraint(&parsed.full_constraint);
                        let line_num = line_index.line_for("project.dependencies", &parsed.package);
                        deps.push(ParsedDependency {
                            name: parsed.package,
                            version: parsed.version,
                            line_number: line_num,
                            has_upper_bound,
                            // Alignment writes the highest version it found over
                            // every other occurrence, so a specifier with no floor
                            // to raise is no more alignable than it is updatable.
                            is_bumpable: parsed.raisable,
                        });
                    }
                }
            }

            // Parse [project.optional-dependencies.*]
            if let Some(Item::Table(opt_deps)) = project.get("optional-dependencies") {
                for (group_name, group_deps) in opt_deps.iter() {
                    if let Some(arr) = group_deps.as_array() {
                        for item in arr.iter() {
                            if let Some(s) = item.as_str()
                                && let Some(parsed) = self.parse_dependency(s)
                            {
                                let has_upper_bound =
                                    !Self::is_simple_constraint(&parsed.full_constraint);
                                let line_num = line_index.line_for(
                                    &format!("project.optional-dependencies.{}", group_name),
                                    &parsed.package,
                                );
                                deps.push(ParsedDependency {
                                    name: parsed.package,
                                    version: parsed.version,
                                    line_number: line_num,
                                    has_upper_bound,
                                    is_bumpable: parsed.raisable,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Parse [tool.poetry.dependencies] and [tool.poetry.dev-dependencies]
        if let Some(Item::Table(tool)) = doc.get("tool")
            && let Some(Item::Table(poetry)) = tool.get("poetry")
        {
            for section in ["dependencies", "dev-dependencies"] {
                if let Some(Item::Table(section_deps)) = poetry.get(section) {
                    for (key, item) in section_deps.iter() {
                        if key == "python" {
                            continue;
                        }
                        if let Item::Value(Value::String(s)) = item {
                            let version_str = s.value().to_string();
                            let version =
                                if version_str.starts_with('^') || version_str.starts_with('~') {
                                    version_str[1..].to_string()
                                } else {
                                    version_str
                                };
                            let line_num =
                                line_index.line_for(&format!("tool.poetry.{}", section), key);
                            // Poetry's own wildcard (`pkg = "*"`) and a
                            // multi-clause requirement both arrive here as the
                            // whole string, and neither names a release
                            // alignment could carry to another file.
                            let is_bumpable = is_pep440_version(&version);
                            deps.push(ParsedDependency {
                                name: key.to_string(),
                                version,
                                line_number: line_num,
                                has_upper_bound: false,
                                is_bumpable,
                            });
                        }
                    }
                }
            }
        }

        Ok(deps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use std::io::Write;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_dependency() {
        let updater = PyProjectUpdater::new();

        let dep = updater.parse_dependency("requests>=2.28.0").unwrap();
        assert_eq!(dep.package, "requests");
        assert_eq!(dep.version, "2.28.0");
        assert_eq!(dep.full_constraint, ">=2.28.0");
        assert!(dep.raisable);

        let dep = updater
            .parse_dependency("uvicorn[standard]>=0.20.0")
            .unwrap();
        assert_eq!(dep.package, "uvicorn");
        assert_eq!(dep.version, "0.20.0");
        assert_eq!(dep.full_constraint, ">=0.20.0");
        assert!(dep.raisable);

        // Test constraint with upper bound
        let dep = updater.parse_dependency("flask>=2.0.0,<3.0.0").unwrap();
        assert_eq!(dep.package, "flask");
        assert_eq!(dep.version, "2.0.0");
        assert_eq!(dep.full_constraint, ">=2.0.0,<3.0.0");
        assert!(dep.raisable);
    }

    #[test]
    fn test_is_simple_constraint() {
        // Simple constraints - no upper bound, no exclusions
        assert!(PyProjectUpdater::is_simple_constraint("==1.0.0"));
        assert!(PyProjectUpdater::is_simple_constraint(">=1.0.0"));
        assert!(PyProjectUpdater::is_simple_constraint(">1.0.0"));

        // Multiple constraints with comma
        assert!(!PyProjectUpdater::is_simple_constraint(">=1.0.0,<2.0.0"));
        assert!(!PyProjectUpdater::is_simple_constraint(">=2.8.0,<9"));

        // Upper-bound operators (need constraint-aware lookup)
        assert!(!PyProjectUpdater::is_simple_constraint("<4.2"));
        assert!(!PyProjectUpdater::is_simple_constraint("<=2.0"));
        assert!(!PyProjectUpdater::is_simple_constraint("~=1.4"));

        // Exclusion operator
        assert!(!PyProjectUpdater::is_simple_constraint("!=1.5.0"));
    }

    /// Which specifiers offer a floor an update may carry forward. A ceiling
    /// never had one; `>1.0.0` and `!=1.5.0` hold a version each, and each is
    /// one the specifier rules out, so raising either writes a specifier that
    /// excludes the release it was raised to.
    #[test]
    fn a_dependency_knows_whether_its_version_is_a_floor_to_raise() {
        let updater = PyProjectUpdater::new();
        let raisable = |dep: &str| updater.parse_dependency(dep).unwrap().raisable;

        for dep in [
            "pkg>=1.0.0",
            "pkg==1.0.0",
            "pkg~=1.4",
            "pkg>=1.0.0,<2.0.0",
            "pkg<2.0.0,>=1.0.0",
            "pkg>=2.8.0,<9",
        ] {
            assert!(raisable(dep), "{dep}");
        }
        for dep in [
            "pkg<6",
            "pkg<4.2",
            "pkg<=5.0",
            "pkg<=2.0.0",
            "pkg>1.0.0",
            "pkg>1.0.0,<2.0.0",
            "pkg!=1.5.0",
            "pkg<6,!=5.0",
            // A prefix match holds a digit-led operand that is not a version:
            // `6.*` names the whole 6 series, not a release to raise.
            "pkg==6.*",
            "pkg==6.0.*",
            "pkg>=1.2.*",
        ] {
            assert!(!raisable(dep), "{dep}");
        }
    }

    #[test]
    fn test_update_dependency() {
        let updater = PyProjectUpdater::new();

        assert_eq!(
            updater.update_dependency("requests>=2.28.0", "2.31.0"),
            "requests>=2.31.0"
        );

        assert_eq!(
            updater.update_dependency("uvicorn[standard]>=0.20.0", "0.24.0"),
            "uvicorn[standard]>=0.24.0"
        );

        // Constraint preservation - upper bound should be kept
        assert_eq!(
            updater.update_dependency("django>=4.0,<6", "5.2"),
            "django>=5.2,<6"
        );

        assert_eq!(
            updater.update_dependency("pytest>=2.8.0,<9", "8.0.0"),
            "pytest>=8.0.0,<9"
        );

        // Multiple constraints should all be preserved
        assert_eq!(
            updater.update_dependency("foo>=1.0.0,!=1.5.0,<2.0.0", "1.8.0"),
            "foo>=1.8.0,!=1.5.0,<2.0.0"
        );
    }

    #[test]
    fn test_count_structural_array_brackets_ignores_strings_and_comments() {
        assert_eq!(
            PyProjectLineIndex::count_structural_array_brackets(
                r#"[ "requests[socks]>=2.28.0", 'flask[async]>=2.0.0' ] # ]"#,
            ),
            ArrayBracketCounts {
                opening: 1,
                closing: 1,
            }
        );
        assert_eq!(
            PyProjectLineIndex::count_structural_array_brackets(
                r#"  "requests[socks]>=2.28.0", # ] inside a comment"#,
            ),
            ArrayBracketCounts::default()
        );
    }

    #[test]
    fn test_line_index_tracks_entries_after_extras_in_multiline_arrays() {
        let updater = PyProjectUpdater::new();
        let content = r#"[project]
name = "demo"
dependencies = [
  "requests[socks]>=2.28.0", # ] inside a comment should be ignored
  "flask>=2.0.0",
]

[project.optional-dependencies]
dev = [
  "pytest[testing]>=7.0.0",
  "black>=23.0.0", # [comment]
]
"#;

        let line_index = PyProjectLineIndex::from_content(content, &updater);

        assert_eq!(
            line_index.line_for("project.dependencies", "requests"),
            Some(4)
        );
        assert_eq!(
            line_index.line_for("project.dependencies", "flask"),
            Some(5)
        );
        assert_eq!(
            line_index.line_for("project.optional-dependencies.dev", "pytest"),
            Some(10)
        );
        assert_eq!(
            line_index.line_for("project.optional-dependencies.dev", "black"),
            Some(11)
        );
    }

    // Integration tests using MockRegistry and temp files

    #[tokio::test]
    async fn test_update_pyproject_pep621() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
version = "1.0.0"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=3.0.0"));
    }

    #[tokio::test]
    async fn stale_cached_latest_below_pyproject_current_is_revalidated() {
        use crate::cache::{Cache, CachedRegistry};
        use std::sync::Mutex;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["private-package==1.1.1"]
"#
        )
        .unwrap();

        let cache = Arc::new(Mutex::new(Cache::default()));
        cache
            .lock()
            .unwrap()
            .set("pypi", "private-package", "1.1.0".to_string());
        let registry = CachedRegistry::new(
            MockRegistry::new("pypi").with_version("private-package", "1.1.2"),
            Arc::clone(&cache),
            true,
        );

        let result = PyProjectUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(true, false))
            .await
            .unwrap();

        assert!(
            result.warnings.is_empty(),
            "warnings: {:?}",
            result.warnings
        );
        assert_eq!(result.updated[0].0, "private-package");
        assert_eq!(result.updated[0].1, "1.1.1");
        assert_eq!(result.updated[0].2, "1.1.2");
    }

    #[tokio::test]
    async fn test_update_pyproject_poetry() {
        // Poetry uses table format: key = "version"
        // The version can be ^2.28.0, >=2.0.0, 2.0.0, etc.
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.9"
requests = "2.28.0"
flask = "2.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Poetry table-style dependencies with bare versions should be updated
        assert_eq!(result.updated.len(), 2);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("[tool.poetry.dependencies]"));
        // Both should be updated to new versions
        assert!(contents.contains("requests = \"2.31.0\""));
        assert!(contents.contains("flask = \"3.0.0\""));
    }

    #[tokio::test]
    async fn test_update_pyproject_dry_run() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.28.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified in dry-run mode
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.28.0"));
        assert!(!contents.contains("2.31.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_preserves_formatting() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"  # Project name
version = "1.0.0"

# Main dependencies
dependencies = [
    "requests>=2.28.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let contents = std::fs::read_to_string(file.path()).unwrap();
        // toml_edit should preserve comments
        assert!(contents.contains("# Project name") || contents.contains("# Main dependencies"));
    }

    #[tokio::test]
    async fn test_update_pyproject_optional_dependencies() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.28.0"]

[project.optional-dependencies]
dev = ["pytest>=7.0.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("pytest", "8.0.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("pytest>=8.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_unchanged_packages() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.31.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 1);
    }

    /// A ceiling holds no floor to raise, but upd can still say the dependency
    /// has been frozen below the newest release. Reading a specifier it will not
    /// rewrite is the difference between a ceiling doing its job and one that
    /// has quietly stopped a package receiving updates.
    #[tokio::test]
    async fn a_ceiling_the_newest_release_is_outside_is_reported_not_rewritten() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["django<6", "flask<=3.0"]
"#
        )
        .unwrap();

        // The constrained answer is what a lookup *matching* the specifier would
        // return, and it is inside the ceiling by construction: asking that
        // question of a specifier upd cannot rewrite answers it with itself and
        // reports a frozen dependency as current.
        let registry = MockRegistry::new("PyPI")
            .with_version("django", "6.1.0")
            .with_constrained("django", "<6", "5.2.0")
            .with_version("flask", "2.3.0");

        let result = PyProjectUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 1, "flask's 2.3.0 is inside '<=3.0'");
        assert_eq!(
            result.warnings,
            vec!["django: 6.1.0 is available, but '<6' is a range upd does not rewrite"]
        );
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#""django<6""#));
        assert!(contents.contains(r#""flask<=3.0""#));
    }

    /// `urllib3>2.0` names the one release the author refuses. Raising it wrote
    /// `urllib3>2.7` with 2.7 the newest release, and the next run could not
    /// resolve the manifest upd had just written.
    #[tokio::test]
    async fn an_exclusive_lower_bound_is_never_raised() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["urllib3>2.0", "chardet>4.0,<6.0", "pkg!=1.5.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("urllib3", "2.7.0")
            .with_version("chardet", "5.2.0")
            .with_version("pkg", "2.0.0");

        let result = PyProjectUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 3, "each specifier admits its newest");
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#""urllib3>2.0""#));
        assert!(contents.contains(r#""chardet>4.0,<6.0""#));
        assert!(contents.contains(r#""pkg!=1.5.0""#));
    }

    /// `pyyaml==6.*` admits every 6.x release, so it names no one release to
    /// carry forward. Rewriting it as though `6.*` were a version wrote `==6.0`,
    /// the replacement being truncated to the operand's own component count: a
    /// project resolving 6.0.3 was moved back to 6.0 by a run that reported an
    /// update and exited 0.
    #[tokio::test]
    async fn a_prefix_match_is_reported_not_rewritten() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["pyyaml==6.*", "click==8.1.*"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("pyyaml", "6.0.3")
            .with_version("click", "8.3.0");

        let result = PyProjectUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 1, "6.0.3 is one '==6.*' already admits");
        assert_eq!(
            result.warnings,
            vec!["click: 8.3.0 is available, but '==8.1.*' is a range upd does not rewrite"]
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#""pyyaml==6.*""#), "{contents}");
        assert!(contents.contains(r#""click==8.1.*""#), "{contents}");
    }

    /// A pin names one release, and a prefix match has nowhere to put it: what
    /// the rewrite wrote was `==6.0`, which is neither the pinned version nor
    /// the range that was there before.
    #[tokio::test]
    async fn a_pin_a_prefix_match_cannot_hold_is_an_error() {
        use crate::config::UpdConfig;
        use std::collections::HashMap;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["pyyaml==6.*"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("pyyaml", "6.0.3");

        let mut pin = HashMap::new();
        pin.insert("pyyaml".to_string(), "6.0.1".to_string());
        let config = UpdConfig {
            pin,
            ..Default::default()
        };

        let result = PyProjectUpdater::new()
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, false).with_config(Arc::new(config)),
            )
            .await
            .unwrap();

        assert!(result.pinned.is_empty());
        assert_eq!(
            result.errors,
            vec!["cannot pin 'pyyaml' to '6.0.1': '==6.*' has no lower bound that version fits"]
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#""pyyaml==6.*""#), "{contents}");
    }

    /// PEP 508 lets whitespace separate an operator from its version, and
    /// `django <= 5.0` is a ceiling like any other. Reading the operator without
    /// the version behind it turned every spaced bound into a specifier upd
    /// could not read, and a valid manifest into exit 2.
    #[tokio::test]
    async fn a_spaced_bound_is_read_as_the_bound_it_is() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["django <= 5.0", "flask < 3.0", "pkg != 1.5.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("django", "6.1.0")
            .with_version("flask", "2.3.0")
            .with_version("pkg", "2.0.0");

        let result = PyProjectUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 2, "flask and pkg admit their newest");
        assert_eq!(
            result.warnings,
            vec!["django: 6.1.0 is available, but '<= 5.0' is a range upd does not rewrite"]
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#""django <= 5.0""#));
        assert!(contents.contains(r#""flask < 3.0""#));
        assert!(contents.contains(r#""pkg != 1.5.0""#));
    }

    /// A spaced range is still a range. Reading `requests >= 2.0, < 2.20` as a
    /// bare `>=` dropped the ceiling from the lookup, and the run wrote
    /// `requests >= 2.34, < 2.20`: a floor above its own ceiling, which nothing
    /// can install.
    #[tokio::test]
    async fn a_spaced_range_keeps_its_ceiling() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests >= 2.0, < 2.20"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.34.0")
            .with_constrained("requests", ">= 2.0, < 2.20", "2.19.1");

        let result = PyProjectUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "requests");
        assert_eq!(result.updated[0].1, "2.0");
        assert_eq!(
            result.updated[0].2, "2.19",
            "the newest release the range admits"
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains(r#""requests >= 2.19, < 2.20""#),
            "{contents}"
        );
    }

    /// A configured pin is an instruction. When the specifier has no floor to
    /// write it into, the manifest does not say what the config says it should,
    /// so the run reports a failure rather than passing silently.
    #[tokio::test]
    async fn a_pin_a_specifier_cannot_hold_is_an_error() {
        use crate::config::UpdConfig;
        use std::collections::HashMap;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["django<6"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("django", "5.2.0");

        let mut pin = HashMap::new();
        pin.insert("django".to_string(), "5.1.0".to_string());
        let config = UpdConfig {
            pin,
            ..Default::default()
        };

        let result = PyProjectUpdater::new()
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, false).with_config(Arc::new(config)),
            )
            .await
            .unwrap();

        assert!(result.pinned.is_empty());
        assert_eq!(
            result.errors,
            vec!["cannot pin 'django' to '5.1.0': '<6' has no lower bound that version fits"]
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#""django<6""#));
    }

    // Error path tests

    #[tokio::test]
    async fn test_update_pyproject_invalid_toml() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project
name = "invalid toml - missing bracket"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater.update(file.path(), &registry, options).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to parse"));
    }

    #[tokio::test]
    async fn test_update_pyproject_file_not_found() {
        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(
                Path::new("/nonexistent/path/pyproject.toml"),
                &registry,
                options,
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update_pyproject_registry_error_for_package() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "nonexistent-pkg>=1.0.0",
]
"#
        )
        .unwrap();

        // Registry only has requests - nonexistent-pkg will cause an error
        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // One package updated successfully
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "requests");

        // One error for the nonexistent package
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent-pkg"));
    }

    #[tokio::test]
    async fn test_update_pyproject_empty_dependencies() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = []
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn test_update_pyproject_no_dependencies_section() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
version = "1.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
    }

    // Tests for declared package indexes. Each tool's rules for how a declared
    // index relates to the default index are asserted here in isolation; the
    // end-to-end behaviour against two live mock indexes lives in
    // tests/pyproject_indexes.rs.

    fn declared(content: &str) -> DeclaredIndexes {
        let doc: DocumentMut = content.parse().unwrap();
        PyProjectUpdater::declared_indexes(&doc)
    }

    fn url(name: &str, url: &str) -> DeclaredIndex {
        DeclaredIndex::url(Some(name), url)
    }

    #[test]
    fn uv_index_without_default_is_added_ahead_of_pypi() {
        // The reported case: a private-only index declared without
        // `default = true` must not replace PyPI, or every public dependency
        // in the file 404s.
        let d = declared(
            r#"
[[tool.uv.index]]
name = "nexus"
url = "https://nexus.example.com/repository/private/simple/"
publish-url = "https://nexus.example.com/repository/private/"
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                url(
                    "nexus",
                    "https://nexus.example.com/repository/private/simple/"
                ),
                DeclaredIndex::default_registry(),
            ]
        );
        assert!(d.pins.is_empty());
    }

    #[test]
    fn inline_tool_and_index_tables_have_the_same_registry_semantics() {
        let d = declared(
            r#"tool = { uv = { index = [
  { name = "private", url = "https://private.example/simple" },
  { name = "mirror", url = "https://mirror.example/simple", default = true },
], sources = { internal = { index = "private" } } } }
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                url("private", "https://private.example/simple"),
                url("mirror", "https://mirror.example/simple"),
            ]
        );
        assert_eq!(d.pins.get("internal").map(String::as_str), Some("private"));
    }

    #[test]
    fn uv_indexes_keep_declaration_order_with_default_last() {
        let d = declared(
            r#"
[tool.uv]

[[tool.uv.index]]
name = "pytorch"
url = "https://download.pytorch.org/whl/cpu"

[[tool.uv.index]]
name = "private"
url = "https://private.pypi.com/simple"
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                url("pytorch", "https://download.pytorch.org/whl/cpu"),
                url("private", "https://private.pypi.com/simple"),
                DeclaredIndex::default_registry(),
            ]
        );
    }

    #[test]
    fn uv_default_true_replaces_pypi_and_stays_last() {
        let d = declared(
            r#"
[[tool.uv.index]]
name = "mirror"
url = "https://mirror.example.com/simple"
default = true

[[tool.uv.index]]
name = "private"
url = "https://private.pypi.com/simple"
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                url("private", "https://private.pypi.com/simple"),
                url("mirror", "https://mirror.example.com/simple"),
            ]
        );
    }

    #[test]
    fn uv_explicit_index_is_only_reachable_through_sources_pins() {
        let d = declared(
            r#"
[[tool.uv.index]]
name = "pytorch"
url = "https://download.pytorch.org/whl/cpu"
explicit = true

[tool.uv.sources]
torch = { index = "pytorch" }
torchvision = [{ index = "pytorch", marker = "sys_platform == 'linux'" }]
mylib = { git = "https://github.com/example/mylib" }
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                DeclaredIndex::default_registry(),
                url("pytorch", "https://download.pytorch.org/whl/cpu").explicit(),
            ]
        );
        assert_eq!(d.pins.len(), 2);
        assert_eq!(d.pins["torch"], "pytorch");
        assert_eq!(d.pins["torchvision"], "pytorch");
    }

    #[test]
    fn uv_default_and_explicit_index_removes_pypi_without_joining_the_chain() {
        let d = declared(
            r#"
[[tool.uv.index]]
name = "locked"
url = "https://locked.example.com/simple"
default = true
explicit = true
"#,
        );
        assert_eq!(
            d.chain,
            vec![url("locked", "https://locked.example.com/simple").explicit()]
        );
    }

    #[test]
    fn uv_legacy_index_keys_map_to_default_and_extra_roles() {
        let d = declared(
            r#"
[tool.uv]
index-url = "https://mirror.example.com/simple"
extra-index-url = ["https://extra.example.com/simple"]
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                DeclaredIndex::url(None, "https://extra.example.com/simple"),
                DeclaredIndex::url(None, "https://mirror.example.com/simple"),
            ]
        );
    }

    #[test]
    fn uv_index_without_url_is_ignored() {
        let d = declared(
            r#"
[[tool.uv.index]]
name = "broken"
"#,
        );
        assert!(d.chain.is_empty());
    }

    #[test]
    fn poetry_primary_sources_replace_pypi() {
        // Sources without a priority are primary, and a primary source
        // disables the implicit PyPI.
        let d = declared(
            r#"
[tool.poetry]
name = "myproject"

[[tool.poetry.source]]
name = "private"
url = "https://private.pypi.com/simple"

[[tool.poetry.source]]
name = "extra"
url = "https://extra.pypi.com/simple"
priority = "primary"
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                url("private", "https://private.pypi.com/simple"),
                url("extra", "https://extra.pypi.com/simple"),
            ]
        );
    }

    #[test]
    fn poetry_supplemental_source_follows_pypi() {
        let d = declared(
            r#"
[[tool.poetry.source]]
name = "private"
url = "https://private.pypi.com/simple"
priority = "supplemental"
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                DeclaredIndex::default_registry(),
                url("private", "https://private.pypi.com/simple"),
            ]
        );
    }

    #[test]
    fn poetry_named_pypi_source_keeps_the_default_in_its_position() {
        let d = declared(
            r#"
[[tool.poetry.source]]
name = "private"
url = "https://private.pypi.com/simple"
priority = "primary"

[[tool.poetry.source]]
name = "PyPI"
priority = "primary"

[[tool.poetry.source]]
name = "explicit-only"
url = "https://explicit.pypi.com/simple"
priority = "explicit"
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                url("private", "https://private.pypi.com/simple"),
                DeclaredIndex {
                    name: Some("PyPI".to_string()),
                    ..DeclaredIndex::default_registry()
                },
                url("explicit-only", "https://explicit.pypi.com/simple").explicit(),
            ]
        );
    }

    #[test]
    fn poetry_legacy_secondary_flag_is_supplemental() {
        let d = declared(
            r#"
[[tool.poetry.source]]
name = "private"
url = "https://private.pypi.com/simple"
secondary = true
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                DeclaredIndex::default_registry(),
                url("private", "https://private.pypi.com/simple"),
            ]
        );
    }

    #[test]
    fn pdm_sources_follow_pypi_unless_one_is_named_pypi() {
        let d = declared(
            r#"
[tool.pdm]
name = "myproject"

[[tool.pdm.source]]
name = "private"
url = "https://private.pypi.com/simple"

[[tool.pdm.source]]
name = "wheels"
url = "https://wheels.example.com/"
type = "find_links"
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                DeclaredIndex::default_registry(),
                url("private", "https://private.pypi.com/simple"),
            ]
        );

        let d = declared(
            r#"
[[tool.pdm.source]]
name = "private"
url = "https://private.pypi.com/simple"

[[tool.pdm.source]]
name = "pypi"
url = "https://mirror.example.com/simple"
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                url("private", "https://private.pypi.com/simple"),
                url("pypi", "https://mirror.example.com/simple"),
            ]
        );
    }

    #[test]
    fn pdm_source_package_filters_are_carried_onto_the_index() {
        let d = declared(
            r#"
[[tool.pdm.source]]
name = "private"
url = "https://private.pypi.com/simple"
include_packages = ["foo", "foo-*"]
exclude_packages = ["bar"]
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                DeclaredIndex::default_registry(),
                url("private", "https://private.pypi.com/simple").with_package_filters(
                    vec!["foo".to_string(), "foo-*".to_string()],
                    vec!["bar".to_string()],
                ),
            ]
        );
    }

    #[test]
    fn no_declared_indexes_means_no_chain() {
        let d = declared(
            r#"
[project]
name = "myproject"
dependencies = ["requests>=2.0.0"]
"#,
        );
        assert!(d.chain.is_empty());
        assert!(d.pins.is_empty());
    }

    #[test]
    fn the_first_tool_that_declares_indexes_wins() {
        // uv resolves a project that still carries a Poetry table.
        let d = declared(
            r#"
[[tool.uv.index]]
name = "uv-private"
url = "https://uv.pypi.com/simple"

[[tool.poetry.source]]
name = "poetry-private"
url = "https://poetry.pypi.com/simple"

[[tool.pdm.source]]
name = "pdm-private"
url = "https://pdm.pypi.com/simple"
"#,
        );
        assert_eq!(
            d.chain,
            vec![
                url("uv-private", "https://uv.pypi.com/simple"),
                DeclaredIndex::default_registry(),
            ]
        );
    }

    /// The updater resolves through the declared chain: a package the private
    /// index does not carry is answered by the default registry instead of
    /// failing, and a package the private index does carry comes from there.
    #[tokio::test]
    async fn update_layers_declared_uv_index_over_the_default_registry() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let private = MockServer::start().await;
        for p in ["/simple/requests/", "/pypi/requests/json"] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(404))
                .mount(&private)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/simple/private-package/"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&private)
            .await;
        Mock::given(method("GET"))
            .and(path("/pypi/private-package/json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"releases": {"1.0.909": [{"yanked": false}]}}"#),
            )
            .mount(&private)
            .await;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "demo"
dependencies = [
    "requests>=2.28.0",
    "private-package>=1.0.908",
]

[[tool.uv.index]]
name = "nexus"
url = "{}/simple/"
"#,
            private.uri()
        )
        .unwrap();

        let default = MockRegistry::new("pypi").with_version("requests", "2.32.0");
        let updater = PyProjectUpdater::new();
        let result = updater
            .update(file.path(), &default, UpdateOptions::new(true, false))
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        let mut updated: Vec<(&str, &str)> = result
            .updated
            .iter()
            .map(|(p, _, new, _)| (p.as_str(), new.as_str()))
            .collect();
        updated.sort();
        assert_eq!(
            updated,
            vec![("private-package", "1.0.909"), ("requests", "2.32.0")]
        );
    }

    // Tests for config-based ignore/pin functionality

    #[tokio::test]
    async fn test_update_pyproject_pep621_with_config_ignore() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
    "django>=4.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "5.0.0");

        // Create config that ignores flask
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["flask".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
            ..Default::default()
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 packages updated (requests, django), 1 ignored (flask)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "flask");
        assert_eq!(result.ignored[0].1, "2.0.0");

        // Verify file was updated only for non-ignored packages
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=2.0.0")); // unchanged
        assert!(contents.contains("django>=5.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_pep621_with_config_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        // Create config that pins flask to 2.3.0
        let mut pin = std::collections::HashMap::new();
        pin.insert("flask".to_string(), "2.3.0".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: Vec::new(),
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 package updated from registry, 1 pinned
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "requests");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "flask");
        assert_eq!(result.pinned[0].1, "2.0.0"); // old
        assert_eq!(result.pinned[0].2, "2.3.0"); // new (pinned)

        // Verify file was updated with pinned version
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=2.3.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_poetry_with_config_ignore() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.28.0"
flask = "^2.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        // Create config that ignores requests
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["requests".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
            ..Default::default()
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 package updated (flask), 1 ignored (requests)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "flask");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "requests");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests = \"^2.28.0\"")); // unchanged
        assert!(contents.contains("flask = \"^3.0.0\""));
    }

    #[tokio::test]
    async fn test_update_pyproject_poetry_with_config_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.28.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        // Create config that pins requests to 2.29.0
        let mut pin = std::collections::HashMap::new();
        pin.insert("requests".to_string(), "2.29.0".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: Vec::new(),
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 pinned
        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "requests");
        assert_eq!(result.pinned[0].2, "2.29.0");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests = \"^2.29.0\""));
    }

    #[tokio::test]
    async fn test_update_pyproject_duplicate_dependency_names_keep_occurrence_line_numbers() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "demo"
version = "0.1.0"
dependencies = [
  "requests>=2.28.0",
]

[project.optional-dependencies]
dev = [
  "requests>=2.27.0",
]
"#
        )
        .unwrap();

        let mut pin = std::collections::HashMap::new();
        pin.insert("requests".to_string(), "2.29.0".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: Vec::new(),
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &MockRegistry::new("PyPI"), options)
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.pinned.len(), 2);

        let mut line_numbers: Vec<_> = result
            .pinned
            .iter()
            .map(|(_, _, _, line_num)| line_num.unwrap())
            .collect();
        line_numbers.sort_unstable();
        assert_eq!(line_numbers, vec![5, 10]);
    }

    #[tokio::test]
    async fn test_update_pyproject_multiline_arrays_with_extras_keep_following_line_numbers() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "demo"
dependencies = [
  "requests[socks]>=2.28.0", # ] inside a comment should be ignored
  "flask>=2.0.0",
]

[project.optional-dependencies]
dev = [
  "pytest[testing]>=7.0.0",
  "black>=23.0.0", # [comment]
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("pytest", "8.0.0")
            .with_version("black", "24.0.0");

        let updater = PyProjectUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 4);

        let line_for = |package: &str| {
            result
                .updated
                .iter()
                .find(|(name, _, _, _)| name == package)
                .and_then(|(_, _, _, line_num)| *line_num)
        };

        assert_eq!(line_for("requests"), Some(4));
        assert_eq!(line_for("flask"), Some(5));
        assert_eq!(line_for("pytest"), Some(10));
        assert_eq!(line_for("black"), Some(11));

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests[socks]>=2.31.0"));
        assert!(contents.contains("flask>=3.0.0"));
        assert!(contents.contains("pytest[testing]>=8.0.0"));
        assert!(contents.contains("black>=24.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_with_config_ignore_and_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
    "django>=4.0.0",
    "pytest>=7.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "5.0.0")
            .with_version("pytest", "8.0.0");

        // Config: ignore flask, pin django to 4.2.0
        let mut pin = std::collections::HashMap::new();
        pin.insert("django".to_string(), "4.2.0".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["flask".to_string()],
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 updated from registry (requests, pytest), 1 ignored (flask), 1 pinned (django)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.pinned.len(), 1);

        // Verify ignored
        assert_eq!(result.ignored[0].0, "flask");

        // Verify pinned
        assert_eq!(result.pinned[0].0, "django");
        assert_eq!(result.pinned[0].2, "4.2.0");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=2.0.0")); // unchanged (ignored)
        assert!(contents.contains("django>=4.2.0")); // pinned version
        assert!(contents.contains("pytest>=8.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_optional_deps_with_config() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.28.0"]

[project.optional-dependencies]
dev = ["pytest>=7.0.0", "black>=23.0.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("pytest", "8.0.0")
            .with_version("black", "24.0.0");

        // Config: ignore pytest
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["pytest".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
            ..Default::default()
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 updated (requests, black), 1 ignored (pytest)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "pytest");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("pytest>=7.0.0")); // unchanged
        assert!(contents.contains("black>=24.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_pin_preserves_prefix() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.28.0"
flask = "~2.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        // Pin both with different prefixes
        let mut pin = std::collections::HashMap::new();
        pin.insert("requests".to_string(), "2.30.0".to_string());
        pin.insert("flask".to_string(), "2.5.0".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: Vec::new(),
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.pinned.len(), 2);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        // Prefixes should be preserved
        assert!(contents.contains("requests = \"^2.30.0\""));
        assert!(contents.contains("flask = \"~2.5.0\""));
    }

    fn normalization_config(
        dependencies: Option<SpecifierOperator>,
        optional_dependencies: Option<SpecifierOperator>,
        dependency_groups: Option<SpecifierOperator>,
    ) -> crate::config::UpdConfig {
        crate::config::UpdConfig {
            normalize: Some(crate::config::NormalizeConfig {
                pyproject: Some(crate::config::PyprojectNormalize {
                    dependencies,
                    optional_dependencies,
                    dependency_groups,
                }),
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn normalization_is_opt_in_and_section_specific() {
        let content = r#"[project]
name = "demo"
dependencies = [
  'requests[socks] ; python_version >= "3.11"', # keep me
  "click>=7,<9",
]

[project.optional-dependencies]
dev = ["pytest>=8.0"]

[dependency-groups]
lint = ["ruff==0.12.0"]
"#;
        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.34.0")
            .with_version("click", "8.2.1")
            .with_version("pytest", "8.0")
            .with_version("ruff", "0.12.0");

        let mut unconfigured = NamedTempFile::with_suffix(".toml").unwrap();
        write!(unconfigured, "{content}").unwrap();
        let updater = PyProjectUpdater::new();
        let result = updater
            .update(
                unconfigured.path(),
                &registry,
                UpdateOptions::new(false, true),
            )
            .await
            .unwrap();
        assert!(result.normalized.is_empty());
        let shape_preserved = std::fs::read_to_string(unconfigured.path()).unwrap();
        assert!(shape_preserved.contains(r#""click>=8.2.1,<9""#));
        assert!(!shape_preserved.contains("click=="));

        let mut configured = NamedTempFile::with_suffix(".toml").unwrap();
        write!(configured, "{content}").unwrap();
        let options = UpdateOptions::new(false, true).with_config(Arc::new(normalization_config(
            Some(SpecifierOperator::Exact),
            None,
            None,
        )));
        let result = updater
            .update(configured.path(), &registry, options)
            .await
            .unwrap();
        let rewritten = std::fs::read_to_string(configured.path()).unwrap();
        assert!(
            rewritten
                .contains(r#"'requests[socks]==2.34.0 ; python_version >= "3.11"', # keep me"#)
        );
        assert!(rewritten.contains(r#""click==8.2.1""#));
        assert!(rewritten.contains(r#"dev = ["pytest>=8.0"]"#));
        assert!(rewritten.contains(r#"lint = ["ruff==0.12.0"]"#));
        assert_eq!(result.normalized.len(), 2);
        assert_eq!(result.normalized[0].line_number, Some(4));
    }

    #[tokio::test]
    async fn normalization_skips_direct_and_declared_non_registry_sources() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        let content = r#"[project]
name = "demo"
dependencies = [
  "local-package",
  "conditional-package",
  "marked-index-package",
  "archive @ https://example.invalid/archive.whl",
  "requests",
]

[tool.uv.sources]
local-package = { path = "../local-package" }
conditional-package = [
  { index = "linux", marker = "sys_platform == 'linux'" },
  { index = "other", marker = "sys_platform != 'linux'" },
]
marked-index-package = { index = "private", marker = "python_version < '3.12'" }
"#;
        write!(file, "{content}").unwrap();
        let registry = MockRegistry::new("PyPI").with_version("requests", "2.34.0");
        let options = UpdateOptions::new(false, true).with_config(Arc::new(normalization_config(
            Some(SpecifierOperator::AtLeast),
            None,
            None,
        )));
        let result = PyProjectUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();
        let rewritten = std::fs::read_to_string(file.path()).unwrap();
        assert!(rewritten.contains(r#""local-package""#));
        assert!(rewritten.contains(r#""conditional-package""#));
        assert!(rewritten.contains(r#""marked-index-package""#));
        assert!(rewritten.contains(r#""archive @ https://example.invalid/archive.whl""#));
        assert!(rewritten.contains(r#""requests>=2.34.0""#));
        assert_eq!(result.normalized.len(), 1);
        assert_eq!(
            result.normalize_recognized,
            vec![
                "local-package",
                "conditional-package",
                "marked-index-package",
                "requests"
            ]
        );
    }

    #[tokio::test]
    async fn normalization_updates_inline_spelled_project_sections() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        file.write_all(
            r#"project = { name = "demo", dependencies = ["click"], optional-dependencies = { dev = ["pytest>=7"] } }
dependency-groups = { lint = ["ruff~=0.11"] }
"#
            .as_bytes(),
        )
        .unwrap();
        let registry = MockRegistry::new("PyPI")
            .with_version("click", "8.2.1")
            .with_version("pytest", "8.4.0")
            .with_version("ruff", "0.12.10");
        let options = UpdateOptions::new(false, true).with_config(Arc::new(normalization_config(
            Some(SpecifierOperator::AtLeast),
            Some(SpecifierOperator::AtLeast),
            Some(SpecifierOperator::Exact),
        )));
        let result = PyProjectUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();
        let rewritten = std::fs::read_to_string(file.path()).unwrap();
        assert!(rewritten.contains(r#"dependencies = ["click>=8.2.1"]"#));
        assert!(rewritten.contains(r#"dev = ["pytest>=8.4.0"]"#));
        assert!(rewritten.contains(r#"lint = ["ruff==0.12.10"]"#));
        assert_eq!(result.normalized.len(), 2);
        assert_eq!(result.updated.len(), 1);
    }

    #[test]
    fn interactive_normalization_replay_preserves_toml_spelling() {
        let content = r#"[project]
dependencies = ['click', "requests>=2,<3"]
"#;
        let specs = vec![
            NormalizedSpec {
                package: "click".to_string(),
                section: "project.dependencies".to_string(),
                previous_spec: None,
                new_spec: ">=8.2.1".to_string(),
                version: "8.2.1".to_string(),
                previous_version: None,
                pinned: false,
                held_back_from: None,
                line_number: Some(2),
            },
            NormalizedSpec {
                package: "requests".to_string(),
                section: "project.dependencies".to_string(),
                previous_spec: Some(">=2,<3".to_string()),
                new_spec: "==2.34.0".to_string(),
                version: "2.34.0".to_string(),
                previous_version: Some("2".to_string()),
                pinned: false,
                held_back_from: None,
                line_number: Some(2),
            },
        ];
        let rewritten = apply_normalized_specs(content, &specs).unwrap();
        assert_eq!(
            rewritten,
            r#"[project]
dependencies = ['click>=8.2.1', "requests==2.34.0"]
"#
        );
    }

    #[test]
    fn interactive_normalization_replay_is_scoped_to_the_configured_section() {
        let content = r#"[project]
dependencies = ["click"]

[dependency-groups]
dev = ["click"]
"#;
        let specs = vec![NormalizedSpec {
            package: "click".to_string(),
            section: "dependency-groups.dev".to_string(),
            previous_spec: None,
            new_spec: "==8.2.1".to_string(),
            version: "8.2.1".to_string(),
            previous_version: None,
            pinned: false,
            held_back_from: None,
            line_number: Some(5),
        }];
        let rewritten = apply_normalized_specs(content, &specs).unwrap();
        assert!(rewritten.contains(r#"dependencies = ["click"]"#));
        assert!(rewritten.contains(r#"dev = ["click==8.2.1"]"#));
    }

    #[test]
    fn normalization_line_index_ignores_tables_and_multiline_string_content() {
        let content = r#"[project]
description = """
[dependency-groups]
fake = ["not-a-dependency"]
"""
dependencies = [
  { include-group = "lint" },
  "click; python_version == \"3.12\"",
]

[dependency-groups] # policy lives here
dev = ["ruff"]
"#;
        let updater = PyProjectUpdater::new();
        let index = PyProjectLineIndex::from_content(content, &updater);
        assert_eq!(
            index.line_for_entry(
                "project.dependencies",
                r#"click; python_version == "3.12""#,
                0
            ),
            Some(8)
        );
        assert_eq!(
            index.line_for_entry("project.dependencies", "lint", 0),
            None
        );
        assert_eq!(
            index.line_for_entry("dependency-groups.dev", "ruff", 0),
            Some(12)
        );
        assert_eq!(
            index.line_for_entry("dependency-groups.fake", "not-a-dependency", 0),
            None
        );
    }

    #[tokio::test]
    async fn dry_run_reports_normalization_without_writing() {
        let content = "[project]\ndependencies = [\"click\"]\n";
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(file, "{content}").unwrap();
        let options = UpdateOptions::new(true, false).with_config(Arc::new(normalization_config(
            Some(SpecifierOperator::AtMost),
            None,
            None,
        )));
        let result = PyProjectUpdater::new()
            .update(
                file.path(),
                &MockRegistry::new("PyPI").with_version("click", "8.2.1"),
                options,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), content);
        assert_eq!(result.normalized[0].new_spec, "<=8.2.1");
    }

    #[tokio::test]
    async fn cooldown_leaves_an_unanchored_name_unchanged_when_every_release_is_fresh() {
        let now = chrono::Utc::now();
        let content = "[project]\ndependencies = [\"click\"]\n";
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(file, "{content}").unwrap();
        let registry = MockRegistry::new("PyPI")
            .with_version("click", "8.2.1")
            .with_version_meta(
                "click",
                "8.2.1",
                Some(now - chrono::Duration::days(1)),
                false,
                false,
            );
        let policy = crate::cooldown::CooldownPolicy {
            default: chrono::Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };
        let options = UpdateOptions::new(false, false)
            .with_config(Arc::new(normalization_config(
                Some(SpecifierOperator::Exact),
                None,
                None,
            )))
            .with_cooldown_policy(policy, now);
        let result = PyProjectUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), content);
        assert!(result.normalized.is_empty());
        assert!(result.skipped_by_cooldown.is_empty());
        assert_eq!(result.warnings.len(), 1);
    }

    #[tokio::test]
    async fn normalization_honors_pins_ignores_and_package_filters() {
        let content = r#"[project]
dependencies = ["requests>=2.0", "click", "rich"]
"#;
        let mut config = normalization_config(Some(SpecifierOperator::Exact), None, None);
        config
            .pin
            .insert("requests".to_string(), "2.28.0".to_string());
        config.ignore.push("click".to_string());
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(file, "{content}").unwrap();
        let options = UpdateOptions::new(false, true)
            .with_config(Arc::new(config))
            .with_packages(vec!["requests".to_string(), "click".to_string()]);
        let result = PyProjectUpdater::new()
            .update(
                file.path(),
                &MockRegistry::new("PyPI").with_version("rich", "14.0.0"),
                options,
            )
            .await
            .unwrap();

        let rewritten = std::fs::read_to_string(file.path()).unwrap();
        assert!(rewritten.contains(r#""requests==2.28.0""#));
        assert!(rewritten.contains(r#""click""#));
        assert!(rewritten.contains(r#""rich""#));
        assert_eq!(result.normalized.len(), 1);
        assert!(result.normalized[0].pinned);
        assert!(
            result.ignored.is_empty(),
            "bare ignored names carry no version"
        );
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn normalized_manifest_is_idempotent() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            "[project]\ndependencies = [\"click\", \"requests>=2,<3\"]\n"
        )
        .unwrap();
        let config = Arc::new(normalization_config(
            Some(SpecifierOperator::AtLeast),
            None,
            None,
        ));
        let registry = MockRegistry::new("PyPI")
            .with_version("click", "8.2.1")
            .with_version("requests", "2.34.0");
        let updater = PyProjectUpdater::new();
        let first = updater
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, true).with_config(Arc::clone(&config)),
            )
            .await
            .unwrap();
        let once = std::fs::read_to_string(file.path()).unwrap();
        let second = updater
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, true).with_config(config),
            )
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), once);
        assert_eq!(first.normalized.len(), 2);
        assert!(second.normalized.is_empty());
        assert!(second.updated.is_empty());
        assert_eq!(second.unchanged, 2);
    }

    #[tokio::test]
    async fn normalization_honors_bump_ceiling_but_can_shape_a_bare_name() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            "[project]\ndependencies = [\"requests>=2.0\", \"click\"]\n"
        )
        .unwrap();
        let options = UpdateOptions::new(false, true)
            .with_config(Arc::new(normalization_config(
                Some(SpecifierOperator::Exact),
                None,
                None,
            )))
            .with_bump_filter(crate::updater::BumpFilter {
                major: false,
                minor: false,
                patch: true,
            });
        let result = PyProjectUpdater::new()
            .update(
                file.path(),
                &MockRegistry::new("PyPI")
                    .with_version("requests", "2.34.0")
                    .with_version("click", "8.2.1"),
                options,
            )
            .await
            .unwrap();

        let rewritten = std::fs::read_to_string(file.path()).unwrap();
        assert!(rewritten.contains(r#""requests>=2.0""#));
        assert!(rewritten.contains(r#""click==8.2.1""#));
        assert_eq!(result.capped.len(), 1);
        assert_eq!(result.normalized.len(), 1);
    }

    #[tokio::test]
    async fn normalization_tracks_prereleases_and_refuses_registry_downgrades() {
        let mut prerelease = NamedTempFile::with_suffix(".toml").unwrap();
        write!(prerelease, "[project]\ndependencies = [\"foo==1.0b1\"]\n").unwrap();
        let config = normalization_config(Some(SpecifierOperator::AtLeast), None, None);
        let result = PyProjectUpdater::new()
            .update(
                prerelease.path(),
                &MockRegistry::new("PyPI").with_prerelease("foo", "1.0.0", "1.1rc1"),
                UpdateOptions::new(false, true).with_config(Arc::new(config)),
            )
            .await
            .unwrap();
        assert_eq!(result.normalized[0].version, "1.1rc1");

        let original = "[project]\ndependencies = [\"foo>=9.0\"]\n";
        let mut behind = NamedTempFile::with_suffix(".toml").unwrap();
        write!(behind, "{original}").unwrap();
        let result = PyProjectUpdater::new()
            .update(
                behind.path(),
                &MockRegistry::new("PyPI").with_version("foo", "8.0.0"),
                UpdateOptions::new(false, true).with_config(Arc::new(normalization_config(
                    Some(SpecifierOperator::Exact),
                    None,
                    None,
                ))),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(behind.path()).unwrap(), original);
        assert!(result.normalized.is_empty());
        assert_eq!(result.warnings.len(), 1);
    }
}
