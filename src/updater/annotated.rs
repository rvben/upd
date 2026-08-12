//! The updater for files `upd` has no parser for, driven by trailing comment
//! annotations. This module owns the registry dispatch and the warning mode;
//! the grammar and the text surgery live in `crate::annotation`.

use crate::annotation::AnnotationSource;
use crate::cache::CachedRegistry;
use crate::registry::{
    CratesIoRegistry, GitHubReleasesRegistry, GoProxyRegistry, MultiPyPiRegistry, NpmRegistry,
    NuGetRegistry, Registry, RubyGemsRegistry,
};
use anyhow::{Result, anyhow};
use std::collections::HashMap;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::registry::PyPiRegistry;
    use std::sync::Mutex;

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
}
