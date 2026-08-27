//! The updater for files `upd` has no parser for, driven by trailing comment
//! annotations. This module owns the registry dispatch and the warning mode;
//! the grammar and the text surgery live in `crate::annotation`.

use super::{
    CooldownOutcome, FileType, OwnsLines, ParsedDependency, PendingVersion, UpdateOptions,
    UpdateResult, Updater, apply_cooldown, downgrade_warning, read_file_safe, write_file_atomic,
};
use crate::align::compare_versions;
use crate::annotation::{
    AnnotationSource, ParseOutcome, UNSUPPORTED_SOURCE_PREFIX, distinct_values,
    is_prerelease_token, is_version_token, parse_line, reapply_v_prefix, rewrite_spans,
    version_spans,
};
use crate::cache::CachedRegistry;
use crate::registry::{
    CratesIoRegistry, GitHubReleasesRegistry, GoProxyRegistry, MultiPyPiRegistry, NpmRegistry,
    NuGetRegistry, Registry, RubyGemsRegistry,
};
use crate::updater::{GoModUpdater, Lang};
use crate::version::match_version_precision;
use anyhow::{Result, anyhow};
use futures::future::join_all;
use pep440_rs::Version as Pep440Version;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

/// One registry per annotation source.
///
/// Empty for a parse-only instance. Never partially filled: `resolving`
/// populates all seven or the constructor does not exist.
pub struct RegistrySet {
    entries: HashMap<AnnotationSource, Arc<dyn Registry>>,
}

impl RegistrySet {
    /// All seven v1 sources, from the `CachedRegistry`-wrapped registries the
    /// binary already builds.
    ///
    /// Concretely typed rather than taking `Arc<dyn Registry>` values: passing a
    /// freshly built or uncached registry is then a compile error instead of a
    /// silent misconfiguration that costs a request per lookup, and the PyPI
    /// parameter cannot be satisfied by a single-index `PyPiRegistry`.
    pub fn resolving(
        pypi: &Arc<CachedRegistry<MultiPyPiRegistry>>,
        npm: &Arc<CachedRegistry<NpmRegistry>>,
        crates_io: &Arc<CachedRegistry<CratesIoRegistry>>,
        go_proxy: &Arc<CachedRegistry<GoProxyRegistry>>,
        rubygems: &Arc<CachedRegistry<RubyGemsRegistry>>,
        nuget: &Arc<CachedRegistry<NuGetRegistry>>,
        github_releases: &Arc<CachedRegistry<GitHubReleasesRegistry>>,
    ) -> Self {
        let entries: HashMap<AnnotationSource, Arc<dyn Registry>> = HashMap::from([
            (
                AnnotationSource::PyPi,
                Arc::clone(pypi) as Arc<dyn Registry>,
            ),
            (AnnotationSource::Npm, Arc::clone(npm) as Arc<dyn Registry>),
            (
                AnnotationSource::Crates,
                Arc::clone(crates_io) as Arc<dyn Registry>,
            ),
            (
                AnnotationSource::Go,
                Arc::clone(go_proxy) as Arc<dyn Registry>,
            ),
            (
                AnnotationSource::RubyGems,
                Arc::clone(rubygems) as Arc<dyn Registry>,
            ),
            (
                AnnotationSource::NuGet,
                Arc::clone(nuget) as Arc<dyn Registry>,
            ),
            (
                AnnotationSource::GitHubReleases,
                Arc::clone(github_releases) as Arc<dyn Registry>,
            ),
        ]);
        Self { entries }
    }

    /// No registries. `parse_dependencies` never resolves, so this is
    /// sufficient for `align::get_updater`.
    pub fn parse_only() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// One source, backed by whatever registry the caller supplies.
    ///
    /// `entries` is private to this module, so tests in other modules cannot
    /// build the struct literally. Test-only on purpose: a partially filled set
    /// in production is the silent misconfiguration `resolving` exists to make
    /// impossible.
    #[cfg(test)]
    pub(crate) fn with_single(source: AnnotationSource, registry: Arc<dyn Registry>) -> Self {
        Self {
            entries: HashMap::from([(source, registry)]),
        }
    }

    /// Fallible in both constructions, so there is one signature rather than
    /// two. On a parse-only set every lookup is `Err`; on a resolving set every
    /// v1 source is `Ok`. Never a silent `None`: an `Option` here would make a
    /// misconstructed updater look like a file with nothing to update.
    pub fn for_source(&self, source: AnnotationSource) -> Result<&dyn Registry> {
        self.entries
            .get(&source)
            .map(|registry| registry.as_ref())
            .ok_or_else(|| {
                anyhow!(
                    "no registry available for source '{}': this updater was built for parsing only",
                    source.token()
                )
            })
    }
}

/// Whether `parse_dependencies` prints its refusals to stderr.
///
/// An enum rather than a `bool` because `scan_packages` would otherwise take
/// two unlabelled trailing arguments. Lives here rather than in
/// `crate::annotation` because only this updater has a second warning channel
/// (`UpdateResult.warnings`) for the same refusals to conflict with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseWarnings {
    Print,
    Suppress,
}

/// One annotated line that survived parsing, with everything the update pass
/// needs so no line is parsed twice.
struct AnnotatedLine {
    line_idx: usize,
    source: AnnotationSource,
    package: String,
    /// The single distinct version value found in the code portion.
    version: String,
    /// Every candidate span, ascending. A repeated value has several.
    spans: Vec<Range<usize>>,
}

/// The result of reading a file: the usable lines, and one refusal message per
/// line upd understood the intent of but declined to act on.
struct AnnotatedScan {
    lines: Vec<AnnotatedLine>,
    refusals: Vec<String>,
}

/// Apply `--lang` per line. An empty selection means everything;
/// `Lang::Annotated` selects every annotated line whatever its source; and a
/// source's own lang selects its lines individually.
fn lang_selected(langs: &[Lang], source: AnnotationSource) -> bool {
    langs.is_empty() || langs.contains(&Lang::Annotated) || langs.contains(&source.lang())
}

/// Whether a selection can reach any annotation at all.
///
/// This decides whether a file is worth opening; [`lang_selected`] then decides
/// each line inside it. Asked of the same predicate over every source so the two
/// cannot disagree: a selection that opens no file must be one that would have
/// admitted no line either, or a pin goes unreported with nothing said.
pub fn selection_reaches_annotations(langs: &[Lang]) -> bool {
    AnnotationSource::ALL
        .iter()
        .any(|source| lang_selected(langs, *source))
}

/// Match the current token's precision unless the caller asked for full
/// precision, then restore its `v` prefix.
fn choose_write_value(current: &str, resolved: &str, full_precision: bool) -> String {
    let matched = if full_precision {
        resolved.to_string()
    } else {
        match_version_precision(current, resolved)
    };
    reapply_v_prefix(current, &matched)
}

/// Read every annotated line in `content`. Refusals are collected rather than
/// returned as an error: one bad line must not blind upd to the rest of a file.
///
/// `owner` is present only when this file also has an updater of its own, and
/// reports the lines that updater rewrites. See [`OwnsLines`].
fn scan_annotated(content: &str, owner: Option<&dyn OwnsLines>) -> AnnotatedScan {
    let mut lines: Vec<AnnotatedLine> = Vec::new();
    let mut refusals: Vec<String> = Vec::new();
    let mut unsupported_sources: HashSet<String> = HashSet::new();

    for (line_idx, raw) in content.lines().enumerate() {
        let outcome = parse_line(raw);

        // Checked before the outcome is read, so a marker on an owned line is
        // reported as the collision it is rather than as whatever else might
        // be wrong with it. Refused rather than skipped: an annotation that
        // does nothing and says nothing is the exact failure this pass exists
        // to remove.
        if !matches!(outcome, ParseOutcome::None) && owner.is_some_and(|owner| owner.owns_line(raw))
        {
            refusals.push(format!(
                "line {}: annotation ignored, this line's version is already resolved by the file's own updater",
                line_idx + 1
            ));
            continue;
        }

        let annotation = match outcome {
            ParseOutcome::None => continue,
            ParseOutcome::Malformed(reason) => {
                if let Some(source) = reason
                    .strip_prefix(UNSUPPORTED_SOURCE_PREFIX)
                    .and_then(|rest| rest.split_once('\'').map(|(source, _)| source))
                    && !unsupported_sources.insert(source.to_string())
                {
                    continue;
                }
                refusals.push(format!("line {}: {}", line_idx + 1, reason));
                continue;
            }
            ParseOutcome::Found(annotation) => annotation,
        };

        let spans = version_spans(raw, annotation.comment_start);
        let distinct = distinct_values(raw, &spans);
        match distinct.len() {
            0 => {
                refusals.push(format!(
                    "line {}: no version token found on annotated line",
                    line_idx + 1
                ));
                continue;
            }
            1 => {}
            _ => {
                refusals.push(format!(
                    "line {}: ambiguous version token on annotated line: {}",
                    line_idx + 1,
                    distinct.join(", ")
                ));
                continue;
            }
        }

        lines.push(AnnotatedLine {
            line_idx,
            source: annotation.source,
            version: distinct[0].to_string(),
            package: annotation.package,
            spans,
        });
    }

    // One package name under two sources inside one file is a contradiction upd
    // cannot resolve, so every line involved is dropped and the file reports
    // exactly one warning naming all of them.
    let mut conflicts: Vec<(String, Vec<usize>)> = Vec::new();
    let mut examined: HashSet<&str> = HashSet::new();
    for line in &lines {
        if !examined.insert(line.package.as_str()) {
            continue;
        }
        let same: Vec<&AnnotatedLine> = lines
            .iter()
            .filter(|other| other.package == line.package)
            .collect();
        let sources: HashSet<AnnotationSource> = same.iter().map(|other| other.source).collect();
        if sources.len() > 1 {
            conflicts.push((
                line.package.clone(),
                same.iter().map(|other| other.line_idx).collect(),
            ));
        }
    }
    for (package, line_idxs) in &conflicts {
        let numbers: Vec<String> = line_idxs.iter().map(|idx| (idx + 1).to_string()).collect();
        refusals.push(format!(
            "lines {}: conflicting sources for annotated package '{}'",
            numbers.join(", "),
            package
        ));
    }
    let conflicted: HashSet<&str> = conflicts.iter().map(|(name, _)| name.as_str()).collect();
    lines.retain(|line| !conflicted.contains(line.package.as_str()));

    AnnotatedScan { lines, refusals }
}

/// Updates dependencies whose ecosystem is declared per line rather than by the
/// file's name.
pub struct AnnotatedUpdater {
    registries: RegistrySet,
    warnings: ParseWarnings,
}

impl AnnotatedUpdater {
    /// The updating constructor. Parse refusals travel back through
    /// `UpdateResult::warnings`, so this variant never prints them itself.
    pub fn new(registries: RegistrySet) -> Self {
        Self {
            registries,
            warnings: ParseWarnings::Suppress,
        }
    }

    /// The scan-only constructor used by `align::get_updater`. It cannot look
    /// anything up, and `warnings` decides whether its refusals reach stderr.
    pub fn new_parse_only(warnings: ParseWarnings) -> Self {
        Self {
            registries: RegistrySet::parse_only(),
            warnings,
        }
    }

    /// The annotation pass over a file that already has an updater of its own.
    ///
    /// `owner` reports which lines belong to that updater; annotations on them
    /// are refused rather than acted on, so the two passes never write the same
    /// line in one run. Composed by [`super::update_with_annotations`], which
    /// is the only intended caller.
    pub async fn update_alongside(
        &self,
        path: &Path,
        options: UpdateOptions,
        owner: &dyn OwnsLines,
    ) -> Result<UpdateResult> {
        self.run(path, options, Some(owner)).await
    }
}

#[async_trait::async_trait]
impl Updater for AnnotatedUpdater {
    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::Annotated
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let scan = scan_annotated(&content, None);
        if self.warnings == ParseWarnings::Print {
            for refusal in &scan.refusals {
                eprintln!("{}: Warning: {}", path.display(), refusal);
            }
        }
        Ok(scan
            .lines
            .into_iter()
            .map(|line| ParsedDependency {
                line_number: Some(line.line_idx + 1),
                // An annotated pin is a single value, never a range.
                has_upper_bound: false,
                is_bumpable: !(line.source == AnnotationSource::Go
                    && GoModUpdater::is_pseudo_version(&line.version)),
                name: line.package,
                version: line.version,
            })
            .collect())
    }

    async fn update(
        &self,
        path: &Path,
        _registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        self.run(path, options, None).await
    }
}

impl AnnotatedUpdater {
    /// The body shared by both entry points.
    ///
    /// `owner` is `None` when this updater is the file's only one, and `Some`
    /// when the annotation pass is running beside a file type's own parser.
    async fn run(
        &self,
        path: &Path,
        options: UpdateOptions,
        owner: Option<&dyn OwnsLines>,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut result = UpdateResult::default();
        let scan = scan_annotated(&content, owner);

        // Parse-time refusals are recorded unconditionally, ahead of every gate.
        // `--package other` must not hide a malformed annotation.
        result.warnings.extend(scan.refusals.iter().cloned());

        let lines: Vec<&str> = content.lines().collect();
        let mut fetch: Vec<&AnnotatedLine> = Vec::new();
        let mut version_map: HashMap<usize, PendingVersion> = HashMap::new();

        for line in &scan.lines {
            let line_num = line.line_idx + 1;

            // Every scanned line contributes its source, so the report can
            // label entries upd declined to change as well as ones it wrote.
            result
                .entry_ecosystem
                .insert(line.package.clone(), line.source);

            // A commit-pinned Go version names a commit, not a release. Refuse
            // it before the gates, as GoModUpdater does.
            if line.source == AnnotationSource::Go && GoModUpdater::is_pseudo_version(&line.version)
            {
                result.warnings.push(format!(
                    "line {line_num}: commit-pinned Go version, not updatable"
                ));
                continue;
            }

            if options.is_package_filtered_out(&line.package) {
                result.unchanged += 1;
                continue;
            }

            if options.should_ignore(&line.package) {
                result
                    .ignored
                    .push((line.package.clone(), line.version.clone(), Some(line_num)));
                continue;
            }
            if !lang_selected(&options.langs, line.source) {
                result.unchanged += 1;
                continue;
            }

            // A pin short-circuits the registry entirely.
            if let Some(pinned) = options.get_pinned_version(&line.package) {
                version_map.insert(line.line_idx, PendingVersion::Pinned(pinned.to_string()));
                continue;
            }

            if line.source.lang() == Lang::Python && line.version.parse::<Pep440Version>().is_err()
            {
                result.warnings.push(format!(
                    "line {line_num}: current version \"{}\" is not a valid PEP 440 version",
                    line.version
                ));
                continue;
            }

            fetch.push(line);
        }

        // One lookup per line, all in flight together.
        let lookups: Vec<_> = fetch
            .iter()
            .map(|line| async {
                let registry = self.registries.for_source(line.source)?;
                if is_prerelease_token(&line.version, line.source.lang()) {
                    registry
                        .get_latest_version_including_prereleases(&line.package)
                        .await
                } else {
                    registry.get_latest_version(&line.package).await
                }
            })
            .collect();
        for (line, resolved) in fetch.iter().zip(join_all(lookups).await) {
            version_map.insert(line.line_idx, PendingVersion::Registry(resolved));
        }

        let mut new_lines: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        let mut modified = false;

        for line in &scan.lines {
            let Some(pending) = version_map.remove(&line.line_idx) else {
                continue;
            };
            let line_num = line.line_idx + 1;
            let lang = line.source.lang();

            match pending {
                PendingVersion::Pinned(pinned) => {
                    let target = choose_write_value(&line.version, &pinned, options.full_precision);
                    if target == line.version {
                        result.unchanged += 1;
                        continue;
                    }
                    result.pinned.push((
                        line.package.clone(),
                        line.version.clone(),
                        target.clone(),
                        Some(line_num),
                    ));
                    new_lines[line.line_idx] =
                        rewrite_spans(lines[line.line_idx], &line.spans, &target);
                    modified = true;
                }
                PendingVersion::Registry(Err(e)) => {
                    result.errors.push(format!("{}: {}", line.package, e));
                }
                PendingVersion::Registry(Ok(resolved)) => {
                    // upd is about to write this value into a line it does not
                    // otherwise understand, so it must be a version.
                    if !is_version_token(&resolved) {
                        result.warnings.push(format!(
                            "line {line_num}: unusable version from {} for {}: {resolved}",
                            line.source.token(),
                            line.package
                        ));
                        result.unchanged += 1;
                        continue;
                    }
                    let current_is_prerelease = is_prerelease_token(&line.version, lang);
                    if current_is_prerelease && !is_prerelease_token(&resolved, lang) {
                        // No newer prerelease exists. Promoting the user to a
                        // stable release is not what they asked for, and it is
                        // silent in RequirementsUpdater too.
                        result.unchanged += 1;
                        continue;
                    }

                    // An annotated line carries no constraint, so the cooldown
                    // selector never has one to respect.
                    let registry = match self.registries.for_source(line.source) {
                        Ok(registry) => registry,
                        Err(e) => {
                            result.errors.push(format!("{}: {}", line.package, e));
                            continue;
                        }
                    };
                    let (outcome, note) = apply_cooldown(
                        registry,
                        &line.package,
                        &line.version,
                        &resolved,
                        None,
                        current_is_prerelease,
                        &options,
                    )
                    .await;
                    if let Some(msg) = note {
                        options.note_cooldown_unavailable(&msg);
                    }
                    let (resolved, held_back_record) = match outcome {
                        CooldownOutcome::Unchanged(version) => (version, None),
                        CooldownOutcome::HeldBack {
                            chosen,
                            skipped_version,
                            skipped_published_at,
                        } => (chosen, Some((skipped_version, skipped_published_at))),
                        CooldownOutcome::Skipped {
                            skipped_version,
                            skipped_published_at,
                        } => {
                            result.skipped_by_cooldown.push((
                                line.package.clone(),
                                line.version.clone(),
                                skipped_version,
                                skipped_published_at,
                            ));
                            continue;
                        }
                    };

                    let target =
                        choose_write_value(&line.version, &resolved, options.full_precision);
                    if target == line.version {
                        result.unchanged += 1;
                        continue;
                    }

                    // `compare_versions` strips a leading `v` from both operands
                    // itself (see `align::compare_semver`), so the raw tokens are
                    // passed straight through.
                    if compare_versions(&target, &line.version, lang) != Ordering::Greater {
                        result.warnings.push(downgrade_warning(
                            &line.package,
                            &target,
                            &line.version,
                        ));
                        result.unchanged += 1;
                        continue;
                    }
                    if !options.allows_bump(&line.version, &target) {
                        result.record_capped(&line.package, &line.version, &target, Some(line_num));
                        continue;
                    }

                    result.updated.push((
                        line.package.clone(),
                        line.version.clone(),
                        target.clone(),
                        Some(line_num),
                    ));
                    if let Some((skipped_version, skipped_published_at)) = held_back_record {
                        result.held_back.push((
                            line.package.clone(),
                            line.version.clone(),
                            target.clone(),
                            skipped_version,
                            skipped_published_at,
                        ));
                    }
                    new_lines[line.line_idx] =
                        rewrite_spans(lines[line.line_idx], &line.spans, &target);
                    modified = true;
                }
            }
        }

        if modified && !options.dry_run {
            let mut new_content = String::with_capacity(content.len());
            for (line_idx, segment) in content.split_inclusive('\n').enumerate() {
                let terminator = if segment.ends_with("\r\n") {
                    "\r\n"
                } else if segment.ends_with('\n') {
                    "\n"
                } else {
                    ""
                };
                new_content.push_str(&new_lines[line_idx]);
                new_content.push_str(terminator);
            }
            // A write failure must not discard what this run already learned.
            // Append the error, keep the warnings, and return Ok so the file's
            // diagnostics still reach the report.
            if let Err(e) = write_file_atomic(path, &new_content) {
                result.errors.push(e.to_string());
                return Ok(UpdateResult {
                    errors: result.errors,
                    warnings: result.warnings,
                    ..Default::default()
                });
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::registry::PyPiRegistry;
    use std::sync::Mutex;

    /// The outer gate must agree with the inner one: a selection that opens no
    /// file has to be one that would have admitted no line either. Asserted
    /// against `lang_selected` itself over every source, so a new source cannot
    /// satisfy one and not the other.
    #[test]
    fn a_selection_reaches_annotations_exactly_when_some_source_does() {
        for langs in [
            vec![],
            vec![Lang::Annotated],
            vec![Lang::Python],
            vec![Lang::GithubReleases],
            vec![Lang::Actions],
            vec![Lang::Terraform],
            vec![Lang::Actions, Lang::Annotated],
        ] {
            let any_line = AnnotationSource::ALL
                .iter()
                .any(|source| lang_selected(&langs, *source));
            assert_eq!(
                selection_reaches_annotations(&langs),
                any_line,
                "disagreement on {langs:?}"
            );
        }

        // The two outcomes, named, so the loop above is not comparing one
        // constant against another.
        assert!(selection_reaches_annotations(&[Lang::Annotated]));
        assert!(selection_reaches_annotations(&[Lang::GithubReleases]));
        assert!(!selection_reaches_annotations(&[Lang::Actions]));
        assert!(!selection_reaches_annotations(&[Lang::Terraform]));
    }

    /// A `RegistrySet::resolving` built from real registries. No network call
    /// happens: constructing a registry only builds an HTTP client, and the
    /// assertions below read `name()`, which is a constant.
    fn real_resolving_set() -> RegistrySet {
        let cache = Arc::new(Mutex::new(Cache::default()));
        let pypi = Arc::new(CachedRegistry::new(
            MultiPyPiRegistry::from_primary_and_extras(PyPiRegistry::new(), Vec::new()),
            Arc::clone(&cache),
            false,
        ));
        let npm = Arc::new(CachedRegistry::new(
            NpmRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let crates_io = Arc::new(CachedRegistry::new(
            CratesIoRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let go_proxy = Arc::new(CachedRegistry::new(
            GoProxyRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let rubygems = Arc::new(CachedRegistry::new(
            RubyGemsRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let nuget = Arc::new(CachedRegistry::new(
            NuGetRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let github_releases = Arc::new(CachedRegistry::new(
            GitHubReleasesRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        RegistrySet::resolving(
            &pypi,
            &npm,
            &crates_io,
            &go_proxy,
            &rubygems,
            &nuget,
            &github_releases,
        )
    }

    const ALL_SOURCES: [AnnotationSource; 7] = [
        AnnotationSource::PyPi,
        AnnotationSource::Npm,
        AnnotationSource::Crates,
        AnnotationSource::Go,
        AnnotationSource::RubyGems,
        AnnotationSource::NuGet,
        AnnotationSource::GitHubReleases,
    ];

    /// The two vocabularies must agree, or a `[cooldown.ecosystem]` override
    /// silently applies to nothing. This is the reason `registry_name()` exists
    /// as a second method rather than `token()` being reused.
    #[test]
    fn registry_name_matches_the_resolved_registrys_own_name() {
        let set = real_resolving_set();
        for source in ALL_SOURCES {
            let registry = set
                .for_source(source)
                .unwrap_or_else(|e| panic!("{source:?} must resolve: {e}"));
            assert_eq!(
                source.registry_name(),
                registry.name(),
                "{source:?} names its registry differently from the registry itself"
            );
        }
    }

    /// The PyPI entry must be the multi-index registry, or a user with a
    /// private index resolves against the public one without being told.
    /// `MultiPyPiRegistry::registries()` is the discriminating property: a bare
    /// `PyPiRegistry` has no such accessor, so a `resolving` that accepted one
    /// would fail to compile here rather than fail silently in production.
    #[test]
    fn resolving_takes_the_multi_index_pypi_registry() {
        let cache = Arc::new(Mutex::new(Cache::default()));
        let multi = MultiPyPiRegistry::from_primary_and_extras(
            PyPiRegistry::with_index_url("https://example.invalid/simple".to_string()),
            vec!["https://example.invalid/extra".to_string()],
        );
        assert_eq!(multi.registries().len(), 2);
        let pypi = Arc::new(CachedRegistry::new(multi, Arc::clone(&cache), false));
        let set = RegistrySet::resolving(
            &pypi,
            &Arc::new(CachedRegistry::new(
                NpmRegistry::new(),
                Arc::clone(&cache),
                false,
            )),
            &Arc::new(CachedRegistry::new(
                CratesIoRegistry::new(),
                Arc::clone(&cache),
                false,
            )),
            &Arc::new(CachedRegistry::new(
                GoProxyRegistry::new(),
                Arc::clone(&cache),
                false,
            )),
            &Arc::new(CachedRegistry::new(
                RubyGemsRegistry::new(),
                Arc::clone(&cache),
                false,
            )),
            &Arc::new(CachedRegistry::new(
                NuGetRegistry::new(),
                Arc::clone(&cache),
                false,
            )),
            &Arc::new(CachedRegistry::new(
                GitHubReleasesRegistry::new(),
                Arc::clone(&cache),
                false,
            )),
        );
        assert_eq!(
            set.for_source(AnnotationSource::PyPi).unwrap().name(),
            "pypi"
        );
    }

    #[test]
    fn a_parse_only_set_refuses_every_source_by_name() {
        // Not `.expect_err(...)`: that requires the `Ok` type to implement
        // `Debug`, and `&dyn Registry` does not (`Registry` is `Send + Sync`
        // only). A manual match asserts the identical thing.
        let set = RegistrySet::parse_only();
        for source in ALL_SOURCES {
            let err = match set.for_source(source) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("a parse-only set has no registries"),
            };
            assert!(
                err.contains(source.token()) && err.contains("parsing only"),
                "{source:?} error must name the source and the cause: {err}"
            );
        }
    }

    #[test]
    fn a_resolving_set_holds_exactly_the_seven_v1_sources() {
        let set = real_resolving_set();
        assert_eq!(set.entries.len(), 7);
    }

    use std::io::Write;

    fn write_temp(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(content.as_bytes()).expect("write");
        file.flush().expect("flush");
        file
    }

    fn deps_of(content: &str) -> Vec<crate::updater::ParsedDependency> {
        let file = write_temp(content);
        AnnotatedUpdater::new_parse_only(ParseWarnings::Suppress)
            .parse_dependencies(file.path())
            .expect("parse")
    }

    #[test]
    fn scans_one_annotated_line_and_ignores_everything_else() {
        let scan = scan_annotated(
            "# a comment\nBAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\nPLAIN = 1.0.0\n",
            None,
        );
        assert!(scan.refusals.is_empty(), "{:?}", scan.refusals);
        assert_eq!(scan.lines.len(), 1);
        assert_eq!(scan.lines[0].line_idx, 1);
        assert_eq!(scan.lines[0].package, "openbao-cli");
        assert_eq!(scan.lines[0].source, AnnotationSource::PyPi);
        assert_eq!(scan.lines[0].version, "2.6.1");
    }

    #[test]
    fn a_line_with_no_version_is_refused_by_line_number() {
        let scan = scan_annotated("TOOL ?= latest  # upd: pypi ruff\n", None);
        assert!(scan.lines.is_empty());
        assert_eq!(
            scan.refusals,
            vec!["line 1: no version token found on annotated line".to_string()]
        );
    }

    #[test]
    fn a_line_with_two_distinct_versions_is_refused_with_both_values() {
        // `:` is not a version-field byte, so the code portion splits into
        // fields that yield two different version values.
        let scan = scan_annotated("IMG ?= app:1.2.3 helper:2.0.0  # upd: pypi x\n", None);
        assert!(scan.lines.is_empty());
        assert_eq!(
            scan.refusals,
            vec!["line 1: ambiguous version token on annotated line: 1.2.3, 2.0.0".to_string()]
        );
    }

    #[test]
    fn one_version_repeated_on_a_line_is_not_ambiguous() {
        // `-` is a field byte, so a hyphen-joined identifier like
        // `app-1.2.3` is not a candidate; delimit each occurrence with `/`
        // instead, as in `FOO := 1.2.3` plus
        // `FOO_URL := .../1.2.3/...`.
        let scan = scan_annotated(
            "VERSION := 1.2.3 URL := .../1.2.3/tarball  # upd: pypi app\n",
            None,
        );
        assert!(scan.refusals.is_empty(), "{:?}", scan.refusals);
        assert_eq!(scan.lines.len(), 1);
        assert_eq!(scan.lines[0].spans.len(), 2);
        assert_eq!(scan.lines[0].version, "1.2.3");
    }

    #[test]
    fn conflicting_sources_drop_both_lines_with_one_warning() {
        let scan = scan_annotated(
            "A ?= 1.0.0  # upd: pypi widget\nB ?= 2.0.0  # upd: npm widget\nC ?= 3.0.0  # upd: pypi other\n",
            None,
        );
        assert_eq!(
            scan.refusals,
            vec!["lines 1, 2: conflicting sources for annotated package 'widget'".to_string()]
        );
        let names: Vec<&str> = scan.lines.iter().map(|l| l.package.as_str()).collect();
        assert_eq!(names, vec!["other"]);
    }

    #[test]
    fn unsupported_sources_are_deduplicated_after_normalization() {
        let scan = scan_annotated(
            "A ?= 1.0.0  # upd: Cargo first\nB ?= 2.0.0  # upd: cargo second\nC ?= 3.0.0  # upd: Helm third\n",
            None,
        );

        assert!(scan.lines.is_empty());
        assert_eq!(
            scan.refusals,
            vec![
                "line 1: unsupported source 'cargo'".to_string(),
                "line 3: unsupported source 'helm'".to_string(),
            ]
        );
    }

    #[test]
    fn two_lines_under_one_source_are_not_a_conflict() {
        let scan = scan_annotated(
            "A ?= 1.0.0  # upd: pypi widget\nB ?= 1.0.0  # upd: pypi widget\n",
            None,
        );
        assert!(scan.refusals.is_empty(), "{:?}", scan.refusals);
        assert_eq!(scan.lines.len(), 2);
    }

    #[test]
    fn parse_dependencies_reports_a_go_pseudo_version_as_unbumpable() {
        let deps = deps_of(
            "X ?= v0.0.0-20200115085410-6d4e4cb37c7d  # upd: go golang.org/x/net\nY ?= v1.2.3  # upd: go golang.org/x/text\n",
        );
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "golang.org/x/net");
        assert!(!deps[0].is_bumpable);
        assert_eq!(deps[0].line_number, Some(1));
        assert!(deps[1].is_bumpable);
        // An annotated pin carries no constraint, so there is never a ceiling.
        assert!(!deps[0].has_upper_bound);
        assert!(!deps[1].has_upper_bound);
    }

    #[test]
    fn parse_dependencies_keeps_the_package_name_byte_for_byte() {
        let deps = deps_of("PKG ?= 1.0.0  # upd: nuget Azure.Core\n");
        assert_eq!(deps[0].name, "Azure.Core");
    }

    use crate::registry::MockRegistry;

    fn set_with(source: AnnotationSource, registry: MockRegistry) -> RegistrySet {
        RegistrySet {
            entries: HashMap::from([(source, Arc::new(registry) as Arc<dyn Registry>)]),
        }
    }

    /// `AnnotatedUpdater` ignores the registry the trait hands it, so every test
    /// needs some `&dyn Registry` to pass and none of them care which.
    fn unused_registry() -> MockRegistry {
        MockRegistry::new("unused")
    }

    async fn run(
        content: &str,
        set: RegistrySet,
        options: UpdateOptions,
    ) -> (UpdateResult, String) {
        let file = write_temp(content);
        let updater = AnnotatedUpdater::new(set);
        let result = updater
            .update(file.path(), &unused_registry(), options)
            .await
            .expect("update");
        let written = std::fs::read_to_string(file.path()).expect("read back");
        (result, written)
    }

    #[tokio::test]
    async fn writes_the_resolved_version_in_place() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("openbao-cli", "2.7.0"),
        );
        let (result, written) = run(
            "BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;

        assert_eq!(
            result.updated,
            vec![(
                "openbao-cli".to_string(),
                "2.6.1".to_string(),
                "2.7.0".to_string(),
                Some(1)
            )]
        );
        assert_eq!(written, "BAO_VERSION ?= 2.7.0  # upd: pypi openbao-cli\n");
        assert_eq!(
            result.entry_ecosystem.get("openbao-cli"),
            Some(&AnnotationSource::PyPi)
        );
    }

    #[tokio::test]
    async fn preserves_mixed_line_endings_and_the_missing_final_newline() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("openbao-cli", "2.7.0"),
        );
        let original = "FIRST := unchanged\r\nBAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\nLAST := unchanged";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert_eq!(result.updated.len(), 1, "{result:?}");
        assert_eq!(
            written,
            "FIRST := unchanged\r\nBAO_VERSION ?= 2.7.0  # upd: pypi openbao-cli\nLAST := unchanged"
        );
    }

    #[tokio::test]
    async fn a_dry_run_reports_the_change_and_writes_nothing() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("ruff", "0.9.0"),
        );
        let original = "RUFF ?= 0.8.0  # upd: pypi ruff\n";
        let (result, written) = run(original, set, UpdateOptions::new(true, false)).await;

        assert_eq!(result.updated.len(), 1);
        assert_eq!(written, original);
    }

    #[tokio::test]
    async fn the_v_prefix_and_the_precision_of_the_line_are_preserved() {
        let set = set_with(
            AnnotationSource::GitHubReleases,
            MockRegistry::new("github-releases").with_version("cli/cli", "2.65.4"),
        );
        let (_, written) = run(
            "GH ?= v2.60  # upd: github-releases cli/cli\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;
        assert_eq!(written, "GH ?= v2.65  # upd: github-releases cli/cli\n");
    }

    #[tokio::test]
    async fn every_occurrence_of_the_version_on_the_line_is_rewritten() {
        // Same fixture shape as `one_version_repeated_on_a_line_is_not_ambiguous`:
        // `-` is a field byte, so the two occurrences are delimited with `/`
        // rather than joined onto an identifier with `-`.
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("app", "2.0.0"),
        );
        let (_, written) = run(
            "VERSION := 1.2.3 URL := .../1.2.3/tarball  # upd: pypi app\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;
        assert_eq!(
            written,
            "VERSION := 2.0.0 URL := .../2.0.0/tarball  # upd: pypi app\n"
        );
    }

    #[tokio::test]
    async fn an_unusable_registry_answer_is_refused_and_counted_unchanged() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "latest"),
        );
        let original = "THING ?= 1.0.0  # upd: pypi thing\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert_eq!(
            result.warnings,
            vec!["line 1: unusable version from pypi for thing: latest".to_string()]
        );
        assert_eq!(result.unchanged, 1);
        assert!(result.updated.is_empty());
        assert_eq!(written, original);
    }

    #[tokio::test]
    async fn a_downgrade_is_refused_with_a_warning() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "0.9.0"),
        );
        let original = "THING ?= 1.0.0  # upd: pypi thing\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].contains("0.9.0"),
            "{:?}",
            result.warnings
        );
        assert_eq!(result.unchanged, 1);
        assert_eq!(written, original);
    }

    #[tokio::test]
    async fn a_v_prefixed_upgrade_across_a_digit_width_change_is_not_a_downgrade() {
        // A lexical compare ranks "v10.0.0" below "v9.0.0" (`1` < `9`). The
        // downgrade check goes through `compare_versions`, which strips a
        // leading `v` before comparing, so this line must land as an upgrade.
        let set = set_with(
            AnnotationSource::Crates,
            MockRegistry::new("crates.io").with_version("thing", "10.0.0"),
        );
        let original = "THING ?= v9.0.0  # upd: crates thing\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(
            result.updated,
            vec![(
                "thing".to_string(),
                "v9.0.0".to_string(),
                "v10.0.0".to_string(),
                Some(1)
            )]
        );
        assert_eq!(written, "THING ?= v10.0.0  # upd: crates thing\n");
    }

    #[tokio::test]
    async fn a_stable_answer_to_a_prerelease_question_is_silent() {
        let set = set_with(
            AnnotationSource::Npm,
            MockRegistry::new("npm").with_prerelease("thing", "2.0.0", "2.0.0"),
        );
        let original = "THING ?= 2.0.0-rc.1  # upd: npm thing\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert!(result.updated.is_empty());
        assert_eq!(result.unchanged, 1);
        assert_eq!(written, original);
    }

    #[tokio::test]
    async fn a_registry_error_lands_in_errors_and_leaves_the_line_alone() {
        let set = set_with(AnnotationSource::PyPi, MockRegistry::new("pypi"));
        let original = "THING ?= 1.0.0  # upd: pypi thing\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].starts_with("thing: "),
            "{:?}",
            result.errors
        );
        assert_eq!(written, original);
    }

    #[tokio::test]
    async fn a_missing_registry_for_a_source_is_an_error_not_a_panic() {
        // A parse-only set answers for nothing, which is the shape a wiring bug
        // would produce in the real binary.
        let original = "THING ?= 1.0.0  # upd: pypi thing\n";
        let (result, written) = run(
            original,
            RegistrySet::parse_only(),
            UpdateOptions::new(false, false),
        )
        .await;

        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].contains("parsing only"),
            "{:?}",
            result.errors
        );
        assert_eq!(written, original);
    }

    #[tokio::test]
    async fn a_commit_pinned_go_line_is_refused_even_under_a_package_filter() {
        // The refusal runs before --package, so naming another package does not
        // silence it. GoModUpdater behaves the same way.
        let set = set_with(
            AnnotationSource::Go,
            MockRegistry::new("go-proxy").with_version("golang.org/x/net", "v0.30.0"),
        );
        let original = "NET ?= v0.0.0-20200115085410-6d4e4cb37c7d  # upd: go golang.org/x/net\n";
        let options = UpdateOptions::new(false, false).with_packages(vec!["other".to_string()]);
        let (result, written) = run(original, set, options).await;

        assert_eq!(
            result.warnings,
            vec!["line 1: commit-pinned Go version, not updatable".to_string()]
        );
        assert_eq!(written, original);
    }

    #[tokio::test]
    async fn a_malformed_annotation_is_reported_even_under_a_package_filter() {
        let original = "THING ?= 1.0.0  # upd: pypi\n";
        let options = UpdateOptions::new(false, false).with_packages(vec!["other".to_string()]);
        let (result, written) = run(original, RegistrySet::parse_only(), options).await;

        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].starts_with("line 1: malformed annotation:"),
            "{:?}",
            result.warnings
        );
        assert_eq!(written, original);
    }

    #[tokio::test]
    async fn an_unparseable_python_token_is_refused() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "2.7.0"),
        );
        // `1.0++` matches the version-field grammar, so the scanner accepts it as
        // this line's version, but PEP 440 rejects an empty local segment.
        // A token that failed the grammar would be refused earlier, by the
        // scanner, and would never reach the PEP 440 guard.
        let original = "THING ?= 1.0++  # upd: pypi thing\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;
        assert_eq!(
            result.warnings,
            vec!["line 1: current version \"1.0++\" is not a valid PEP 440 version".to_string()]
        );
        assert_eq!(written, original);
    }

    fn set_of(entries: Vec<(AnnotationSource, MockRegistry)>) -> RegistrySet {
        RegistrySet {
            entries: entries
                .into_iter()
                .map(|(source, registry)| (source, Arc::new(registry) as Arc<dyn Registry>))
                .collect(),
        }
    }

    /// Two sources, two registries, one mixed-source file.
    fn mixed_source_set() -> RegistrySet {
        set_of(vec![
            (
                AnnotationSource::PyPi,
                MockRegistry::new("pypi").with_version("ruff", "0.9.0"),
            ),
            (
                AnnotationSource::GitHubReleases,
                MockRegistry::new("github-releases").with_version("oven-sh/bun", "v1.2.5"),
            ),
        ])
    }

    const MIXED_SOURCE_FILE: &str = "RUFF ?= 0.8.0  # upd: pypi ruff\n\
                                     BUN ?= v1.1.0  # upd: github-releases oven-sh/bun\n";

    #[tokio::test]
    async fn two_sources_in_one_file_each_resolve_against_their_own_registry() {
        let (result, written) = run(
            MIXED_SOURCE_FILE,
            mixed_source_set(),
            UpdateOptions::new(false, false),
        )
        .await;

        assert_eq!(result.updated.len(), 2, "{result:?}");
        assert_eq!(
            written,
            "RUFF ?= 0.9.0  # upd: pypi ruff\n\
             BUN ?= v1.2.5  # upd: github-releases oven-sh/bun\n",
            "each line takes its own source's answer, not the other's"
        );
    }

    #[tokio::test]
    async fn a_lang_selection_that_excludes_the_source_leaves_the_line_alone() {
        let options = UpdateOptions::new(false, false).with_langs(vec![Lang::Node]);
        let (result, written) = run(MIXED_SOURCE_FILE, mixed_source_set(), options).await;

        assert!(result.updated.is_empty(), "{result:?}");
        assert_eq!(result.unchanged, 2);
        assert_eq!(written, MIXED_SOURCE_FILE);
    }

    #[tokio::test]
    async fn selecting_the_annotated_lang_selects_every_source() {
        let options = UpdateOptions::new(false, false).with_langs(vec![Lang::Annotated]);
        let (result, _) = run(MIXED_SOURCE_FILE, mixed_source_set(), options).await;
        assert_eq!(
            result.updated.len(),
            2,
            "`Lang::Annotated` selects every source, not only the one whose own \
             lang happens to be selected too: {result:?}"
        );
    }

    use crate::config::UpdConfig;
    use crate::registry::VersionMeta;
    use crate::updater::BumpFilter;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Wraps a `MockRegistry` and counts how many times it was asked anything
    /// about a version.
    ///
    /// `name()` is deliberately not counted, which narrows "every `Registry`
    /// method" to version queries. `apply_cooldown` calls `name()` for every resolved
    /// line before its no-policy early return (`src/updater/mod.rs:632-662`), so
    /// counting it would report 2 for a single lookup and the assertions below
    /// would stop meaning "the registry was asked for a version".
    struct CountingRegistry {
        inner: MockRegistry,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Registry for CountingRegistry {
        async fn get_latest_version(&self, package: &str) -> Result<String> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.inner.get_latest_version(package).await
        }

        async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.inner
                .get_latest_version_including_prereleases(package)
                .await
        }

        async fn get_latest_version_matching(
            &self,
            package: &str,
            constraints: &str,
        ) -> Result<String> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.inner
                .get_latest_version_matching(package, constraints)
                .await
        }

        async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.inner.list_versions(package).await
        }

        async fn list_ref_names(&self, package: &str) -> Result<Vec<String>> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.inner.list_ref_names(package).await
        }

        async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.inner.resolve_ref_to_commit(package, reference).await
        }

        async fn tags_at_commit(
            &self,
            package: &str,
            commit: &str,
        ) -> Result<crate::registry::TagsAtCommit> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.inner.tags_at_commit(package, commit).await
        }

        fn name(&self) -> &'static str {
            self.inner.name()
        }
    }

    fn counting_set(
        source: AnnotationSource,
        registry: MockRegistry,
    ) -> (RegistrySet, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counting = CountingRegistry {
            inner: registry,
            calls: Arc::clone(&calls),
        };
        (
            RegistrySet {
                entries: HashMap::from([(source, Arc::new(counting) as Arc<dyn Registry>)]),
            },
            calls,
        )
    }

    fn config_of(ignore: &[&str], pin: &[(&str, &str)]) -> Arc<UpdConfig> {
        Arc::new(UpdConfig {
            ignore: ignore.iter().map(|s| s.to_string()).collect(),
            pin: pin
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..UpdConfig::default()
        })
    }

    /// For a refused line, the diagnostic lands in `warnings` and nothing else
    /// moves.
    ///
    /// `entry_ecosystem` is deliberately not checked. It is recorded before
    /// every gate precisely so a refused line can still be labelled with its
    /// source in the report, so it is expected to be non-empty here.
    fn assert_records_nothing(result: &UpdateResult) {
        assert_eq!(
            result.unchanged, 0,
            "`unchanged` records that the registry was asked; a refusal never asked"
        );
        assert!(result.updated.is_empty(), "{:?}", result.updated);
        assert!(result.pinned.is_empty(), "{:?}", result.pinned);
        assert!(result.ignored.is_empty(), "{:?}", result.ignored);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.held_back.is_empty(), "{:?}", result.held_back);
        assert!(
            result.skipped_by_cooldown.is_empty(),
            "{:?}",
            result.skipped_by_cooldown
        );
    }

    #[tokio::test]
    async fn the_python_token_guard_refuses_before_any_lookup_and_counts_nothing() {
        let (set, calls) = counting_set(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "2.7.0"),
        );
        let original = "THING ?= 1.0++  # upd: pypi thing\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert_eq!(written, original);
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            0,
            "step 3a refuses the line before its lookup is queued"
        );
        assert_eq!(
            result.warnings,
            vec!["line 1: current version \"1.0++\" is not a valid PEP 440 version".to_string()]
        );
        assert_records_nothing(&result);
    }

    #[tokio::test]
    async fn a_package_filter_silences_the_python_token_guard() {
        let (set, calls) = counting_set(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi")
                .with_version("thing", "2.7.0")
                .with_version("other", "3.1.0"),
        );
        let options = UpdateOptions::new(false, false).with_packages(vec!["other".to_string()]);
        let (result, written) = run(
            "THING ?= 1.0++  # upd: pypi thing\nOTHER ?= 3.0.0  # upd: pypi other\n",
            set,
            options,
        )
        .await;

        assert!(
            result.warnings.is_empty(),
            "step 1 runs before step 3a, so `--package other` hides the guard: {:?}",
            result.warnings
        );
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            1,
            "only the selected package is looked up"
        );
        assert_eq!(
            result.unchanged, 1,
            "the filtered-out line counts unchanged"
        );
        assert_eq!(
            written,
            "THING ?= 1.0++  # upd: pypi thing\nOTHER ?= 3.1.0  # upd: pypi other\n"
        );
    }

    #[tokio::test]
    async fn a_configured_pin_repairs_an_unparseable_python_token_and_truncates_to_its_precision() {
        let (set, calls) = counting_set(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "9.9.9"),
        );
        let options =
            UpdateOptions::new(false, false).with_config(config_of(&[], &[("thing", "2.7.0")]));
        let (result, written) = run("THING ?= 1.0++  # upd: pypi thing\n", set, options).await;

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            0,
            "a pin short-circuits the registry"
        );
        assert_eq!(
            result.pinned,
            vec![(
                "thing".to_string(),
                "1.0++".to_string(),
                "2.7".to_string(),
                Some(1)
            )]
        );
        assert_eq!(written, "THING ?= 2.7  # upd: pypi thing\n");
    }

    #[tokio::test]
    async fn the_go_pseudo_version_guard_refuses_before_any_lookup_and_counts_nothing() {
        let (set, calls) = counting_set(
            AnnotationSource::Go,
            MockRegistry::new("go").with_version("example.com/m", "v0.38.0"),
        );
        let original = "MOD ?= v0.0.0-20241217172646-ca3f786aa774  # upd: go example.com/m\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert_eq!(written, original);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(
            result.warnings,
            vec!["line 1: commit-pinned Go version, not updatable".to_string()]
        );
        assert_records_nothing(&result);
    }

    #[tokio::test]
    async fn a_package_filter_does_not_silence_the_go_pseudo_version_guard() {
        let (set, calls) = counting_set(
            AnnotationSource::Go,
            MockRegistry::new("go")
                .with_version("example.com/m", "v0.38.0")
                .with_version("example.com/other", "v1.4.0"),
        );
        let options =
            UpdateOptions::new(false, false).with_packages(vec!["example.com/other".to_string()]);
        let original = "MOD ?= v0.0.0-20241217172646-ca3f786aa774  # upd: go example.com/m\nOTHER ?= v1.3.0  # upd: go example.com/other\n";
        let (result, written) = run(original, set, options).await;

        assert_eq!(
            result.warnings,
            vec!["line 1: commit-pinned Go version, not updatable".to_string()],
            "step 0 runs before step 1, so `--package` cannot hide the guard"
        );
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            1,
            "only the selected package is looked up"
        );
        assert_eq!(
            result.unchanged, 0,
            "the refused line counts nothing and the selected line was written"
        );
        assert_eq!(
            written,
            "MOD ?= v0.0.0-20241217172646-ca3f786aa774  # upd: go example.com/m\nOTHER ?= v1.4.0  # upd: go example.com/other\n"
        );
    }

    #[tokio::test]
    async fn a_configured_pin_does_not_repair_a_commit_pinned_go_line() {
        let (set, calls) = counting_set(
            AnnotationSource::Go,
            MockRegistry::new("go").with_version("example.com/m", "v0.38.0"),
        );
        let options = UpdateOptions::new(false, false)
            .with_config(config_of(&[], &[("example.com/m", "v0.38.0")]));
        let original = "MOD ?= v0.0.0-20241217172646-ca3f786aa774  # upd: go example.com/m\n";
        let (result, written) = run(original, set, options).await;

        assert_eq!(written, original, "step 0 is upstream of the pin branch");
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(
            result.warnings,
            vec!["line 1: commit-pinned Go version, not updatable".to_string()]
        );
        assert!(result.pinned.is_empty(), "{:?}", result.pinned);
    }

    #[tokio::test]
    async fn a_prerelease_python_line_takes_the_prerelease_answer() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_prerelease("thing", "1.1.0", "1.2.0rc2"),
        );
        let (result, written) = run(
            "THING ?= 1.2.0rc1  # upd: pypi thing\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;

        assert_eq!(result.updated.len(), 1, "{:?}", result);
        assert_eq!(written, "THING ?= 1.2.0rc2  # upd: pypi thing\n");
    }

    #[tokio::test]
    async fn a_stable_python_line_takes_the_stable_answer_even_when_a_newer_prerelease_exists() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_prerelease("thing", "1.2.1", "1.3.0rc1"),
        );
        let (result, written) = run(
            "THING ?= 1.2.0  # upd: pypi thing\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;

        assert_eq!(result.updated.len(), 1, "{:?}", result);
        assert_eq!(
            written, "THING ?= 1.2.1  # upd: pypi thing\n",
            "a stable current token must not be walked onto 1.3.0rc1"
        );
    }

    #[tokio::test]
    async fn a_prerelease_github_releases_line_takes_the_prerelease_answer() {
        let set = set_with(
            AnnotationSource::GitHubReleases,
            MockRegistry::new("github-releases").with_prerelease(
                "oven-sh/bun",
                "v1.3.0",
                "v1.2.0-rc.2",
            ),
        );
        let (result, written) = run(
            "BUN ?= v1.2.0-rc.1  # upd: github-releases oven-sh/bun\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;

        assert_eq!(result.updated.len(), 1, "{:?}", result);
        assert_eq!(
            written,
            "BUN ?= v1.2.0-rc.2  # upd: github-releases oven-sh/bun\n"
        );
    }

    #[tokio::test]
    async fn a_prerelease_ruby_line_takes_the_prerelease_answer() {
        let set = set_with(
            AnnotationSource::RubyGems,
            MockRegistry::new("rubygems").with_prerelease("thing", "9.0.0", "8.0.0.dev2"),
        );
        let (result, written) = run(
            "THING ?= 8.0.0.dev1  # upd: rubygems thing\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;

        assert_eq!(result.updated.len(), 1, "{:?}", result);
        assert_eq!(written, "THING ?= 8.0.0.dev2  # upd: rubygems thing\n");
    }

    #[tokio::test]
    async fn a_tag_shaped_registry_answer_is_refused_and_counted_unchanged() {
        let set = set_with(
            AnnotationSource::GitHubReleases,
            MockRegistry::new("github-releases").with_version("oven-sh/bun", "bun-v1.2.5"),
        );
        let original = "BUN ?= v1.1.0  # upd: github-releases oven-sh/bun\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert_eq!(written, original);
        assert_eq!(
            result.warnings,
            vec![
                "line 1: unusable version from github-releases for oven-sh/bun: bun-v1.2.5"
                    .to_string()
            ]
        );
        assert_eq!(
            result.unchanged, 1,
            "step 4 refuses after the lookup, so the registry was asked"
        );
        assert!(result.updated.is_empty(), "{:?}", result.updated);
    }

    #[tokio::test]
    async fn the_same_source_answering_a_version_writes_it() {
        let set = set_with(
            AnnotationSource::GitHubReleases,
            MockRegistry::new("github-releases").with_version("oven-sh/bun", "v1.2.5"),
        );
        let (result, written) = run(
            "BUN ?= v1.1.0  # upd: github-releases oven-sh/bun\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.unchanged, 0);
        assert_eq!(
            result.updated,
            vec![(
                "oven-sh/bun".to_string(),
                "v1.1.0".to_string(),
                "v1.2.5".to_string(),
                Some(1)
            )]
        );
        assert_eq!(
            written,
            "BUN ?= v1.2.5  # upd: github-releases oven-sh/bun\n"
        );
    }

    #[tokio::test]
    async fn a_two_segment_line_keeps_its_precision_and_reports_nothing() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "1.2.1"),
        );
        let original = "THING ?= 1.2  # upd: pypi thing\n";
        let (result, written) = run(original, set, UpdateOptions::new(false, false)).await;

        assert_eq!(written, original);
        assert!(
            result.updated.is_empty(),
            "1.2.1 truncated to the line's precision is 1.2, which is not a change: {:?}",
            result.updated
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.unchanged, 1);
    }

    #[tokio::test]
    async fn full_precision_writes_the_registrys_own_value() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "1.2.1"),
        );
        let (result, written) = run(
            "THING ?= 1.2  # upd: pypi thing\n",
            set,
            UpdateOptions::new(false, true),
        )
        .await;

        assert_eq!(result.updated.len(), 1, "{:?}", result);
        assert_eq!(written, "THING ?= 1.2.1  # upd: pypi thing\n");
    }

    #[tokio::test]
    async fn an_ignored_package_is_never_looked_up() {
        let (set, calls) = counting_set(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("ruff", "0.14.2"),
        );
        let options = UpdateOptions::new(false, false).with_config(config_of(&["ruff"], &[]));
        let original = "RUFF ?= 0.8.0  # upd: pypi ruff\n";
        let (result, written) = run(original, set, options).await;

        assert_eq!(written, original);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(
            result.ignored,
            vec![("ruff".to_string(), "0.8.0".to_string(), Some(1))]
        );
        assert_eq!(
            result.unchanged, 0,
            "an ignored package was never asked about"
        );
    }

    #[tokio::test]
    async fn a_configured_pin_that_is_a_downgrade_is_still_written() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("ruff", "0.14.2"),
        );
        let options =
            UpdateOptions::new(false, false).with_config(config_of(&[], &[("ruff", "0.13.0")]));
        let (result, written) = run("RUFF ?= 0.14.2  # upd: pypi ruff\n", set, options).await;

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(
            result.pinned,
            vec![(
                "ruff".to_string(),
                "0.14.2".to_string(),
                "0.13.0".to_string(),
                Some(1)
            )]
        );
        assert_eq!(written, "RUFF ?= 0.13.0  # upd: pypi ruff\n");
    }

    #[tokio::test]
    async fn a_configured_pin_outside_the_bump_ceiling_is_still_written() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "1.0.1"),
        );
        let options = UpdateOptions::new(false, false)
            .with_config(config_of(&[], &[("thing", "2.0.0")]))
            .with_bump_filter(BumpFilter {
                major: false,
                minor: false,
                patch: true,
            });
        let (_, written) = run("THING ?= 1.0.0  # upd: pypi thing\n", set, options).await;

        assert_eq!(written, "THING ?= 2.0.0  # upd: pypi thing\n");
    }

    #[tokio::test]
    async fn the_bump_ceiling_reports_a_capped_registry_answer() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "1.1.0"),
        );
        let options = UpdateOptions::new(false, false).with_bump_filter(BumpFilter {
            major: false,
            minor: false,
            patch: true,
        });
        let original = "THING ?= 1.0.0  # upd: pypi thing\n";
        let (result, written) = run(original, set, options).await;

        assert_eq!(written, original);
        assert!(result.updated.is_empty(), "{:?}", result.updated);
        // A capped bump is not a diagnostic, so it stays out of `warnings`, but
        // it is also not an up-to-date dependency: 1.1.0 is waiting.
        assert!(
            result.warnings.is_empty(),
            "a capped bump is not a warning: {:?}",
            result.warnings
        );
        assert_eq!(
            result.unchanged, 0,
            "a dependency with a newer release is not up to date"
        );
        assert_eq!(result.capped.len(), 1, "capped: {:?}", result.capped);
        assert_eq!(result.capped[0].package, "thing");
        assert_eq!(result.capped[0].current, "1.0.0");
        assert_eq!(result.capped[0].available, "1.1.0");
    }

    #[tokio::test]
    async fn a_dotted_python_suffix_is_replaced_whole() {
        let set = set_with(
            AnnotationSource::PyPi,
            MockRegistry::new("pypi").with_version("thing", "2.0.0"),
        );
        let (result, written) = run(
            "THING ?= 1.2.3.post1  # upd: pypi thing\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;

        assert_eq!(result.updated.len(), 1, "{:?}", result);
        assert_eq!(
            written, "THING ?= 2.0.0  # upd: pypi thing\n",
            "no `.post1` may survive the rewrite"
        );
    }

    #[tokio::test]
    async fn a_dotted_ruby_suffix_is_replaced_whole() {
        let set = set_with(
            AnnotationSource::RubyGems,
            MockRegistry::new("rubygems").with_prerelease("thing", "9.0.0", "9.0.0.beta2"),
        );
        let (result, written) = run(
            "THING ?= 8.0.0.beta1  # upd: rubygems thing\n",
            set,
            UpdateOptions::new(false, false),
        )
        .await;

        assert_eq!(result.updated.len(), 1, "{:?}", result);
        assert_eq!(written, "THING ?= 9.0.0.beta2  # upd: rubygems thing\n");
    }

    #[tokio::test]
    async fn a_parse_only_updater_parses_the_same_dependencies_as_a_resolving_one() {
        let content = "THING ?= 1.2.3  # upd: pypi thing\n\
                       BUN ?= v1.1.0  # upd: github-releases oven-sh/bun\n\
                       BAD ?= 1.0.0  # upd: pypi\n";
        let file = write_temp(content);

        let parse_only = AnnotatedUpdater::new_parse_only(ParseWarnings::Suppress)
            .parse_dependencies(file.path())
            .expect("parse-only");
        let resolving = AnnotatedUpdater::new(real_resolving_set())
            .parse_dependencies(file.path())
            .expect("resolving");

        let shape =
            |deps: &[ParsedDependency]| -> Vec<(String, String, Option<usize>, bool, bool)> {
                deps.iter()
                    .map(|d| {
                        (
                            d.name.clone(),
                            d.version.clone(),
                            d.line_number,
                            d.is_bumpable,
                            d.has_upper_bound,
                        )
                    })
                    .collect()
            };

        assert_eq!(shape(&parse_only), shape(&resolving));
        assert_eq!(
            shape(&parse_only),
            vec![
                (
                    "thing".to_string(),
                    "1.2.3".to_string(),
                    Some(1),
                    true,
                    false
                ),
                (
                    "oven-sh/bun".to_string(),
                    "v1.1.0".to_string(),
                    Some(2),
                    true,
                    false
                ),
            ],
            "the malformed third line is dropped from the dependency list by both"
        );
    }
}
