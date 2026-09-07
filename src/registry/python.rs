//! Interpreter compatibility is scoped to a manifest, never to the running Python.
use super::{Registry, TagsAtCommit, VersionMeta, VersionQuery};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use pep440_rs::{Version, VersionSpecifiers, release_specifiers_to_ranges};
use pep508_rs::{MarkerTree, MarkerTreeKind, MarkerValueVersion};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use version_ranges::Ranges;

/// A release's non-yanked files. Missing Requires-Python means unrestricted,
/// as on indexes that do not publish that optional metadata.
#[derive(Debug, Clone)]
pub struct PythonRelease {
    pub version: String,
    pub requires_python: Vec<Option<String>>,
}

type PythonReleaseCell = Arc<tokio::sync::OnceCell<Vec<PythonRelease>>>;

#[derive(Clone)]
pub(crate) struct PythonRegistry<'a> {
    enabled: bool,
    inner: &'a dyn Registry,
    requirement: String,
    supported: Ranges<Version>,
    releases: Arc<tokio::sync::Mutex<HashMap<String, PythonReleaseCell>>>,
    notes: Arc<Mutex<BTreeSet<String>>>,
}

impl<'a> PythonRegistry<'a> {
    pub fn new(inner: &'a dyn Registry, requirement: String, supported: Ranges<Version>) -> Self {
        Self {
            enabled: true,
            notes: Default::default(),
            inner,
            requirement,
            supported,
            releases: Default::default(),
        }
    }

    pub fn for_project(
        inner: &'a dyn Registry,
        requirement: Option<(String, Ranges<Version>)>,
    ) -> Self {
        match requirement {
            Some((label, supported)) => Self::new(inner, label, supported),
            None => {
                let mut registry = Self::new(inner, String::new(), Ranges::full());
                registry.enabled = false;
                registry
            }
        }
    }

    /// Each occurrence gets its own range; metadata and diagnostics are shared.
    pub fn for_dependency(&self, dependency: &str) -> Result<Self> {
        let mut scoped = self.clone();
        if !self.enabled {
            return Ok(scoped);
        }
        if let Some((_, marker)) = without_comment(dependency).split_once(';') {
            let marker = marker.trim();
            if marker.is_empty() {
                anyhow::bail!("Empty dependency marker");
            }
            let mut warnings = Vec::new();
            let tree = MarkerTree::parse_reporter(marker, &mut |_, warning| warnings.push(warning))
                .with_context(|| format!("Invalid dependency marker '{marker}'"))?;
            if !warnings.is_empty() {
                anyhow::bail!(
                    "Cannot interpret dependency marker '{marker}': {}",
                    warnings.join("; ")
                );
            }
            scoped.supported = scoped.supported.intersection(&python_marker_range(&tree));
            scoped.requirement = format!("{} (dependency marker: {marker})", self.requirement);
        }
        Ok(scoped)
    }

    pub fn is_applicable(&self) -> bool {
        !self.enabled || !self.supported.is_empty()
    }

    pub fn notes(&self) -> Vec<String> {
        self.notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    fn compatible(&self, release: &PythonRelease) -> bool {
        let supported =
            release
                .requires_python
                .iter()
                .fold(Ranges::empty(), |range, requirement| {
                    let file_range = match requirement.as_deref() {
                        None | Some("") => Ranges::full(),
                        Some(requirement) => match requirement.parse::<VersionSpecifiers>() {
                            Ok(specifiers) => release_specifiers_to_ranges(specifiers),
                            // Invalid metadata cannot establish compatibility for this file.
                            Err(_) => Ranges::empty(),
                        },
                    };
                    range.union(&file_range)
                });
        self.supported.subset_of(&supported)
    }

    async fn select(&self, package: &str, query: VersionQuery<'_>) -> Result<String> {
        let constraint = match query {
            VersionQuery::Matching(value) => Some(value.parse::<VersionSpecifiers>()?),
            _ => None,
        };
        if !self.enabled {
            return query.run(self.inner, package).await;
        }
        let releases = self.python_releases(package).await?;
        let mut candidates: Vec<_> = releases
            .iter()
            .filter_map(|release| {
                release
                    .version
                    .parse::<Version>()
                    .ok()
                    .map(|v| (v, release))
            })
            .filter(|(version, _)| {
                matches!(query, VersionQuery::IncludingPrereleases) || !version.any_prerelease()
            })
            .filter(|(version, _)| {
                constraint
                    .as_ref()
                    .is_none_or(|specifiers| specifiers.contains(version))
            })
            .collect();
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        let chosen = candidates.iter().find(|(_, release)| self.compatible(release))
            .ok_or_else(|| anyhow!("No release of '{package}' satisfies the package constraints and supports the project's Python requirement '{}' (invalid release metadata cannot establish compatibility)", self.requirement))?;
        if let Some(newest) = candidates.first().filter(|newest| newest.0 > chosen.0) {
            let requirements: BTreeSet<_> = newest
                .1
                .requires_python
                .iter()
                .filter_map(|value| value.as_deref())
                .collect();
            let requirements = requirements.into_iter().collect::<Vec<_>>().join(" or ");
            self.notes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(format!(
                "{package}: Python compatibility selects {} instead of {}; {} declares Requires-Python '{}'; project supports {}",
                chosen.1.version, newest.1.version, newest.1.version, requirements, self.requirement
            ));
        }
        Ok(chosen.1.version.clone())
    }
}

#[async_trait]
impl Registry for PythonRegistry<'_> {
    async fn python_releases(&self, package: &str) -> Result<Vec<PythonRelease>> {
        // Share one in-flight lookup across occurrences, including private indexes.
        let cell = self
            .releases
            .lock()
            .await
            .entry(package.to_string())
            .or_default()
            .clone();
        cell.get_or_try_init(|| self.inner.python_releases(package))
            .await
            .cloned()
    }
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        self.select(package, VersionQuery::Stable).await
    }
    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        self.select(package, VersionQuery::IncludingPrereleases)
            .await
    }
    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        self.select(package, VersionQuery::Matching(constraints))
            .await
    }
    async fn revalidate_version(
        &self,
        package: &str,
        query: VersionQuery<'_>,
        _stale: &str,
    ) -> Result<String> {
        if !self.enabled {
            return self.inner.revalidate_version(package, query, _stale).await;
        }
        // Metadata is fetched live once per run; a lower compatible answer is valid.
        self.select(package, query).await
    }
    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        if !self.enabled {
            return self.inner.list_versions(package).await;
        }
        let releases = self.python_releases(package).await?;
        Ok(self
            .inner
            .list_versions(package)
            .await?
            .into_iter()
            .filter(|meta| {
                releases
                    .iter()
                    .any(|release| release.version == meta.version && self.compatible(release))
            })
            .collect())
    }
    async fn list_ref_names(&self, package: &str) -> Result<Vec<String>> {
        self.inner.list_ref_names(package).await
    }
    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        self.inner.resolve_ref_to_commit(package, reference).await
    }
    async fn tags_at_commit(&self, package: &str, commit: &str) -> Result<TagsAtCommit> {
        self.inner.tags_at_commit(package, commit).await
    }
    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

/// Existentially project non-Python variables: retain Python versions for which
/// the dependency can apply on any platform or extra. The parser normalizes
/// python_version comparisons to full-version intervals (3.11 means 3.11.*).
fn python_marker_range(tree: &MarkerTree) -> Ranges<Version> {
    match tree.kind() {
        MarkerTreeKind::True => Ranges::full(),
        MarkerTreeKind::False => Ranges::empty(),
        MarkerTreeKind::Version(node) => {
            node.edges().fold(Ranges::empty(), |all, (range, child)| {
                let child = python_marker_range(&child);
                let branch = if matches!(
                    node.key(),
                    MarkerValueVersion::PythonFullVersion | MarkerValueVersion::PythonVersion
                ) {
                    range.intersection(&child)
                } else {
                    child
                };
                all.union(&branch)
            })
        }
        MarkerTreeKind::String(node) => node.children().fold(Ranges::empty(), |all, (_, child)| {
            all.union(&python_marker_range(&child))
        }),
        MarkerTreeKind::In(node) => node.children().fold(Ranges::empty(), |all, (_, child)| {
            all.union(&python_marker_range(&child))
        }),
        MarkerTreeKind::Contains(node) => {
            node.children().fold(Ranges::empty(), |all, (_, child)| {
                all.union(&python_marker_range(&child))
            })
        }
        MarkerTreeKind::Extra(node) => node.children().fold(Ranges::empty(), |all, (_, child)| {
            all.union(&python_marker_range(&child))
        }),
    }
}

fn without_comment(value: &str) -> &str {
    let mut quote = None;
    for (index, character) in value.char_indices() {
        match character {
            '\'' | '"' if quote == Some(character) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(character),
            '#' if quote.is_none() => return &value[..index],
            _ => {}
        }
    }
    value
}

fn item_at<'a>(doc: &'a toml_edit::DocumentMut, path: &[&str]) -> Option<&'a toml_edit::Item> {
    let mut item = doc.as_item();
    for key in path {
        item = item.as_table_like()?.get(key)?;
    }
    Some(item)
}

/// Combine standardized project metadata with Poetry's (possibly narrower)
/// resolver constraint. Never silently ignore malformed interpreter constraints.
pub(crate) fn project_requirement(
    doc: &toml_edit::DocumentMut,
) -> Result<Option<(String, Ranges<Version>)>> {
    let mut supported = Ranges::full();
    let mut labels = Vec::new();
    for (path, poetry) in [
        (&["project", "requires-python"][..], false),
        (&["tool", "poetry", "dependencies", "python"][..], true),
    ] {
        if let Some(item) = item_at(doc, path) {
            let raw = item.as_str().ok_or_else(|| {
                anyhow!(
                    "{} must be a Python version constraint string",
                    path.join(".")
                )
            })?;
            let range = if poetry {
                poetry_range(raw)
            } else {
                raw.parse::<VersionSpecifiers>()
                    .map(release_specifiers_to_ranges)
                    .map_err(anyhow::Error::from)
            }
            .with_context(|| format!("Invalid Python requirement '{raw}' in {}", path.join(".")))?;
            supported = supported.intersection(&range);
            labels.push(raw.to_string());
        }
    }
    if labels.is_empty() {
        return Ok(None);
    }
    if supported.is_empty() {
        anyhow::bail!(
            "Project Python requirements have no versions in common: {}",
            labels.join(" and ")
        );
    }
    Ok(Some((labels.join(" and "), supported)))
}

fn poetry_range(raw: &str) -> Result<Ranges<Version>> {
    let mut union = Ranges::empty();
    let operator_spacing = regex::Regex::new(r"([<>=!~^]+)\s+")?;
    for alternative in raw.split("||") {
        if alternative.trim().is_empty() {
            anyhow::bail!("Empty Python version constraint");
        }
        let mut clauses = Vec::new();
        // Poetry permits whitespace as well as commas between comparisons.
        let compact = operator_spacing.replace_all(alternative.trim(), "$1");
        for clause in compact
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|c| !c.is_empty())
        {
            if clause == "*" {
                continue;
            }
            if let Some(version) = clause.strip_prefix('^').or_else(|| {
                clause
                    .strip_prefix('~')
                    .filter(|_| !clause.starts_with("~="))
            }) {
                let parsed: Version = version.parse()?;
                let mut upper = parsed.release().to_vec();
                let index = if clause.starts_with('^') {
                    upper
                        .iter()
                        .position(|part| *part != 0)
                        .unwrap_or(upper.len() - 1)
                } else {
                    usize::from(upper.len() > 1)
                };
                upper[index] = upper[index]
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("Python version bound is too large"))?;
                upper.truncate(index + 1);
                clauses.push(format!(
                    ">={version},<{}",
                    upper
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(".")
                ));
            } else if clause.starts_with(|c: char| c.is_ascii_digit()) {
                clauses.push(format!("=={clause}"));
            } else {
                clauses.push(clause.to_string());
            }
        }
        union = union.union(&release_specifiers_to_ranges(
            clauses.join(",").parse::<VersionSpecifiers>()?,
        ));
    }
    Ok(union)
}

/// A requirements file inherits the nearest enclosing pyproject, stopping at a
/// repository boundary. A nearer project without a declaration is authoritative.
pub(crate) fn requirements_project(path: &Path) -> Result<Option<(String, Ranges<Version>)>> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    for dir in absolute.parent().into_iter().flat_map(Path::ancestors) {
        let project = dir.join("pyproject.toml");
        if project.try_exists()? {
            let content = crate::updater::read_file_safe(&project)?;
            return project_requirement(&content.parse::<toml_edit::DocumentMut>()?).with_context(
                || format!("Reading Python requirements from {}", project.display()),
            );
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;

    #[test]
    fn full_range_coverage_includes_patch_exclusions_and_file_unions() {
        let inner = MockRegistry::new("pypi");
        for (project, files, expected) in [
            (">=3.10", vec![Some(">=3.11")], false),
            (">=3.10,<4", vec![Some(">=3.9")], true),
            (">=3.10", vec![Some(">=3.10,<4")], false),
            (">=3.10,<3.12", vec![Some(">=3.10,!=3.10.5")], false),
            (">=3.10,!=3.10.5", vec![Some(">=3.10,!=3.10.5")], true),
            (">=3.10", vec![Some(">=3.10,<3.11"), Some(">=3.11")], true),
            (">=3.10", vec![None], true),
            (">=3.10", vec![Some("broken")], false),
            ("==3.10.*", vec![Some(">=3.10,<3.11")], true),
            ("~=3.10.2", vec![Some(">=3.10.2,<3.11")], true),
        ] {
            let registry = PythonRegistry::new(
                &inner,
                project.into(),
                release_specifiers_to_ranges(project.parse().unwrap()),
            );
            let release = PythonRelease {
                version: "1.0".into(),
                requires_python: files.into_iter().map(|f| f.map(str::to_string)).collect(),
            };
            assert_eq!(
                registry.compatible(&release),
                expected,
                "{project}: {release:?}"
            );
        }
    }

    #[test]
    fn markers_project_python_ranges_without_using_the_host_environment() {
        let inner = MockRegistry::new("pypi");
        let registry = PythonRegistry::new(
            &inner,
            ">=3.10".into(),
            release_specifiers_to_ranges(">=3.10".parse().unwrap()),
        );
        for (marker, included, excluded) in [
            ("python_version == '3.11'", "3.11.7", "3.12"),
            ("python_version > '3.10'", "3.11", "3.10.9"),
            ("python_version <= '3.10'", "3.10.9", "3.11"),
            ("python_full_version >= '3.11.2'", "3.11.2", "3.11.1"),
            ("'3.11' <= python_version", "3.11.7", "3.10.9"),
            ("python_version in '3.11 3.13'", "3.13.1", "3.12"),
            (
                "(python_version == '3.11' or python_version >= '3.13') and os_name == 'nt'",
                "3.13.2",
                "3.12",
            ),
            (
                "python_version >= '3.11' or sys_platform == 'win32'",
                "3.10",
                "3.9",
            ),
            (
                "python_version >= '3.11' and extra == 'test'",
                "3.11",
                "3.10",
            ),
        ] {
            let scoped = registry.for_dependency(&format!("demo; {marker}")).unwrap();
            assert!(
                scoped
                    .supported
                    .contains(&included.parse::<Version>().unwrap()),
                "{marker} should include {included}"
            );
            assert!(
                !scoped
                    .supported
                    .contains(&excluded.parse::<Version>().unwrap()),
                "{marker} should exclude {excluded}"
            );
        }
    }

    #[test]
    fn poetry_constraints_and_project_intersection() {
        for (raw, yes, no) in [
            ("^3.10", "3.12", "4.0"),
            ("~3.10", "3.10.5", "3.11"),
            ("3.10.* || >=3.12, <4", "3.12", "3.11"),
            (">= 3.10 < 4", "3.10", "4.0"),
        ] {
            let range = poetry_range(raw).unwrap();
            assert!(range.contains(&yes.parse::<Version>().unwrap()));
            assert!(!range.contains(&no.parse::<Version>().unwrap()));
        }
        let doc =
            "[project]\nrequires-python = '>=3.8'\n[tool.poetry.dependencies]\npython = '^3.10'"
                .parse()
                .unwrap();
        let (_, range) = project_requirement(&doc).unwrap().unwrap();
        assert!(range.contains(&"3.10".parse::<Version>().unwrap()));
        assert!(!range.contains(&"3.8".parse::<Version>().unwrap()));
        for text in [
            "[project]\nrequires-python = 'oops'",
            "[project]\nrequires-python = 3",
            "[project]\nrequires-python = '>=4,<3'",
        ] {
            assert!(project_requirement(&text.parse().unwrap()).is_err());
        }
    }
}
