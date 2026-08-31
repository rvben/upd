//! Per-manifest index resolution.
//!
//! A manifest can declare its own package indexes (`[[tool.uv.index]]`,
//! `[[tool.poetry.source]]`, `[[tool.pdm.source]]`, `--extra-index-url`).
//! Those declarations are additive: unless the tool's own rules say the
//! declared index replaces the default one, the process-wide default registry
//! (PyPI plus environment and pip.conf extras, with caching) must still be
//! consulted. Treating a declared index as the only index turns every public
//! dependency into a 404.
//!
//! The updaters translate a manifest's declarations into an ordered list of
//! [`DeclaredIndex`] entries, placing [`IndexSource::Default`] where the tool
//! consults its default index, and build an [`IndexChain`] over the registry
//! they were handed. The chain is queried first-match in that order, and
//! packages pinned to a named index only ever consult that index.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};

use super::{PyPiRegistry, Registry, VersionMeta, VersionQuery};
use crate::normalize::pep503_normalize;

/// Where one link of the chain sends its queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexSource {
    /// A concrete index URL declared by the manifest.
    Url(String),
    /// The process-wide default registry the updater was handed.
    Default,
}

/// One index declared by a manifest, already placed where the declaring
/// tool's rules put it in the query order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredIndex {
    /// The tool-level name (`name = "..."`) that per-package pins refer to.
    pub name: Option<String>,
    pub source: IndexSource,
    /// Consulted only for packages pinned to it, never as part of the chain.
    pub explicit: bool,
    /// Glob patterns (PDM `include_packages`) binding packages to this index:
    /// a package matching any index's include patterns is looked up only on
    /// the indexes that include it.
    pub include_packages: Vec<String>,
    /// Glob patterns (PDM `exclude_packages`) of packages never looked up on
    /// this index.
    pub exclude_packages: Vec<String>,
}

impl DeclaredIndex {
    pub fn url(name: Option<&str>, url: &str) -> Self {
        Self {
            name: name.map(str::to_string),
            source: IndexSource::Url(url.to_string()),
            explicit: false,
            include_packages: Vec::new(),
            exclude_packages: Vec::new(),
        }
    }

    pub fn default_registry() -> Self {
        Self {
            name: None,
            source: IndexSource::Default,
            explicit: false,
            include_packages: Vec::new(),
            exclude_packages: Vec::new(),
        }
    }

    pub fn explicit(mut self) -> Self {
        self.explicit = true;
        self
    }

    pub fn with_package_filters(mut self, include: Vec<String>, exclude: Vec<String>) -> Self {
        self.include_packages = include;
        self.exclude_packages = exclude;
        self
    }
}

/// Compile package-name globs. Patterns match the PEP 503 normalized name,
/// as PDM's `fnmatch` does. A pattern globset cannot parse is matched
/// literally instead of being dropped, so a typo narrows a filter rather
/// than silently removing it.
fn package_globs(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .or_else(|_| Glob::new(&globset::escape(pattern)))
            .expect("an escaped pattern is always a valid glob");
        builder.add(glob);
    }
    builder.build().ok()
}

/// A link of the chain together with the packages it is scoped to.
struct ScopedLink<'a> {
    link: Link<'a>,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl ScopedLink<'_> {
    fn includes(&self, normalized_package: &str) -> bool {
        self.include
            .as_ref()
            .is_some_and(|globs| globs.is_match(normalized_package))
    }

    fn excludes(&self, normalized_package: &str) -> bool {
        self.exclude
            .as_ref()
            .is_some_and(|globs| globs.is_match(normalized_package))
    }
}

#[derive(Clone)]
enum Link<'a> {
    Index(Arc<PyPiRegistry>),
    Default(&'a dyn Registry),
}

impl Link<'_> {
    fn registry(&self) -> &dyn Registry {
        match self {
            Link::Index(registry) => registry.as_ref(),
            Link::Default(registry) => *registry,
        }
    }
}

/// Which lookup to run against each link of the chain.
#[derive(Clone, Copy)]
enum Query<'q> {
    Latest,
    LatestIncludingPrereleases,
    Matching(&'q str),
}

impl Query<'_> {
    async fn run(self, registry: &dyn Registry, package: &str) -> Result<String> {
        match self {
            Query::Latest => registry.get_latest_version(package).await,
            Query::LatestIncludingPrereleases => {
                registry
                    .get_latest_version_including_prereleases(package)
                    .await
            }
            Query::Matching(constraints) => {
                registry
                    .get_latest_version_matching(package, constraints)
                    .await
            }
        }
    }
}

/// The indexes a manifest's dependencies are resolved against, in query order.
///
/// Queries run first-match: the first link that answers wins, and the last
/// error is reported when none does. This is the same rule
/// [`super::MultiPyPiRegistry`] applies to the default registry and, like uv's
/// `first-index` strategy, it keeps a private index that is listed first from
/// being shadowed by a same-named package on a public one. Tools that resolve
/// best-match across all indexes (pip, PDM) can install a higher version from
/// a later index than the one reported here; that is accepted deliberately,
/// since an updater that bumps a pinned private package to a version only a
/// public index carries would defeat the reason the private index is listed.
pub struct IndexChain<'a> {
    links: Vec<ScopedLink<'a>>,
    /// Normalized package name -> the only link consulted for that package.
    pins: HashMap<String, Link<'a>>,
}

impl<'a> IndexChain<'a> {
    /// Build a chain from a manifest's declarations.
    ///
    /// `pins` maps a package name to the `name` of the declared index it is
    /// pinned to; a pin naming an index the manifest does not declare is
    /// ignored and the package follows the chain. Returns `None` when nothing
    /// is declared, so the caller keeps using `default` unchanged.
    pub fn new(
        declared: Vec<DeclaredIndex>,
        pins: &HashMap<String, String>,
        default: &'a dyn Registry,
    ) -> Option<Self> {
        if declared.is_empty() {
            return None;
        }

        let mut links = Vec::new();
        let mut by_name: HashMap<String, Link<'a>> = HashMap::new();

        for index in declared {
            let link = match index.source {
                IndexSource::Url(url) => Link::Index(Arc::new(PyPiRegistry::from_url(&url))),
                IndexSource::Default => Link::Default(default),
            };
            if let Some(name) = index.name {
                by_name.entry(name).or_insert_with(|| link.clone());
            }
            if !index.explicit {
                links.push(ScopedLink {
                    link,
                    include: package_globs(&index.include_packages),
                    exclude: package_globs(&index.exclude_packages),
                });
            }
        }

        let pins = pins
            .iter()
            .filter_map(|(package, index_name)| {
                by_name
                    .get(index_name)
                    .map(|link| (pep503_normalize(package), link.clone()))
            })
            .collect();

        Some(Self { links, pins })
    }

    /// Number of links consulted for an unpinned package.
    pub fn len(&self) -> usize {
        self.links.len()
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// The links consulted for `package`, in chain order: its pinned index if
    /// it has one; otherwise the indexes that include it by pattern, if any
    /// does; otherwise every index that does not exclude it.
    fn links_for(&self, package: &str) -> Vec<&Link<'a>> {
        let normalized = pep503_normalize(package);
        if let Some(pinned) = self.pins.get(&normalized) {
            return vec![pinned];
        }
        let including: Vec<&Link<'a>> = self
            .links
            .iter()
            .filter(|scoped| scoped.includes(&normalized))
            .map(|scoped| &scoped.link)
            .collect();
        if !including.is_empty() {
            return including;
        }
        self.links
            .iter()
            .filter(|scoped| !scoped.excludes(&normalized))
            .map(|scoped| &scoped.link)
            .collect()
    }

    async fn first_match(&self, package: &str, query: Query<'_>) -> Result<String> {
        let links = self.links_for(package);
        if links.is_empty() {
            return Err(anyhow!(
                "No package index is configured for '{}': the manifest excludes the default index and declares no other",
                package
            ));
        }

        let mut last_error = None;
        for link in links {
            match query.run(link.registry(), package).await {
                Ok(version) => return Ok(version),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("No versions found for package '{}'", package)))
    }

    async fn first_match_revalidated(
        &self,
        package: &str,
        query: VersionQuery<'_>,
        stale_version: &str,
    ) -> Result<String> {
        let links = self.links_for(package);
        if links.is_empty() {
            return Err(anyhow!(
                "No package index is configured for '{}': the manifest excludes the default index and declares no other",
                package
            ));
        }

        let mut last_error = None;
        for link in links {
            match link
                .registry()
                .revalidate_version(package, query, stale_version)
                .await
            {
                Ok(version) => return Ok(version),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("No versions found for package '{}'", package)))
    }
}

#[async_trait]
impl Registry for IndexChain<'_> {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        self.first_match(package, Query::Latest).await
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        self.first_match(package, Query::LatestIncludingPrereleases)
            .await
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        self.first_match(package, Query::Matching(constraints))
            .await
    }

    async fn revalidate_version(
        &self,
        package: &str,
        query: VersionQuery<'_>,
        stale_version: &str,
    ) -> Result<String> {
        self.first_match_revalidated(package, query, stale_version)
            .await
    }

    /// First non-empty answer in chain order.
    ///
    /// A link that fails is not a link that answered "no publish dates". The
    /// cooldown layer distinguishes the two and tells the user which happened,
    /// so a failure anywhere in the chain is reported rather than flattened. It
    /// still never becomes a hard failure: that call degrades to the regular
    /// update path, which is the caller's decision to make and not this
    /// layer's to pre-empt by discarding the reason.
    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        let mut last_error = None;
        for link in self.links_for(package) {
            match link.registry().list_versions(package).await {
                Ok(versions) if !versions.is_empty() => return Ok(versions),
                Ok(_) => {}
                Err(error) => last_error = Some(error),
            }
        }
        match last_error {
            Some(error) => Err(error),
            None => Ok(Vec::new()),
        }
    }

    /// First non-empty answer in chain order. A chain can be built over any
    /// registry (`Link::Default` holds an arbitrary `&dyn Registry`), so this
    /// forwards rather than assuming the Python indexes it usually wraps.
    ///
    /// A link that fails is not a link that answered "no refs". Unlike
    /// `list_versions`, whose empty result the cooldown layer already reads as
    /// "unavailable", an empty ref list tells the caller the refs were checked,
    /// so a failure anywhere in the chain is reported rather than flattened.
    async fn list_ref_names(&self, package: &str) -> Result<Vec<String>> {
        let mut last_error = None;
        for link in self.links_for(package) {
            match link.registry().list_ref_names(package).await {
                Ok(refs) if !refs.is_empty() => return Ok(refs),
                Ok(_) => {}
                Err(error) => last_error = Some(error),
            }
        }
        match last_error {
            Some(error) => Err(error),
            None => Ok(Vec::new()),
        }
    }

    /// First link that resolves the ref wins; otherwise the last error, so the
    /// caller sees why the chain could not answer.
    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        let mut last_error = None;
        for link in self.links_for(package) {
            match link
                .registry()
                .resolve_ref_to_commit(package, reference)
                .await
            {
                Ok(commit) => return Ok(commit),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error
            .unwrap_or_else(|| super::ref_resolution_unsupported(self.name(), package, reference)))
    }

    /// First link that names the commit wins.
    ///
    /// A link that failed is reported rather than flattened into "no tag names
    /// this commit": the caller writes a version comment from this answer, and
    /// an outage that reads as a definitive absence would have it annotate, or
    /// refuse to annotate, on evidence nobody gathered. Only when every link
    /// answered, and none of them had a tag, is the empty answer real. A chain
    /// of registries that all lack tags stays `Unsupported`, which is a third
    /// fact again.
    async fn tags_at_commit(&self, package: &str, commit: &str) -> Result<super::TagsAtCommit> {
        let mut last_error = None;
        let mut any_answered = false;
        for link in self.links_for(package) {
            match link.registry().tags_at_commit(package, commit).await {
                Ok(super::TagsAtCommit::Known(tags)) if !tags.is_empty() => {
                    return Ok(super::TagsAtCommit::Known(tags));
                }
                Ok(super::TagsAtCommit::Known(_)) => any_answered = true,
                Ok(super::TagsAtCommit::Unsupported) => {}
                Err(error) => last_error = Some(error),
            }
        }
        match (last_error, any_answered) {
            (Some(error), _) => Err(error),
            (None, true) => Ok(super::TagsAtCommit::Known(Vec::new())),
            (None, false) => super::tags_at_commit_unsupported(),
        }
    }

    fn name(&self) -> &'static str {
        "pypi"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Serve `versions` for `package` on the legacy JSON API and 404 the Simple
    /// API, which is the path `PyPiRegistry` falls back to.
    async fn serve(mock: &MockServer, package: &str, versions: &[&str]) {
        let releases: Vec<String> = versions
            .iter()
            .map(|v| format!(r#""{v}": [{{"yanked": false}}]"#))
            .collect();
        Mock::given(method("GET"))
            .and(path(format!("/simple/{package}/")))
            .respond_with(ResponseTemplate::new(404))
            .mount(mock)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/pypi/{package}/json")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!(r#"{{"releases": {{{}}}}}"#, releases.join(","))),
            )
            .mount(mock)
            .await;
    }

    async fn missing(mock: &MockServer, package: &str) {
        for p in [
            format!("/simple/{package}/"),
            format!("/pypi/{package}/json"),
        ] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(404))
                .mount(mock)
                .await;
        }
    }

    /// The index must never be asked about `package`; the server verifies the
    /// expectation when it is dropped.
    async fn never_asked(mock: &MockServer, package: &str) {
        for p in [
            format!("/simple/{package}/"),
            format!("/pypi/{package}/json"),
        ] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(mock)
                .await;
        }
    }

    fn no_pins() -> HashMap<String, String> {
        HashMap::new()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn nothing_declared_means_no_chain() {
        let default = MockRegistry::new("pypi");
        assert!(IndexChain::new(Vec::new(), &no_pins(), &default).is_none());
    }

    #[tokio::test]
    async fn declared_index_is_consulted_before_the_default() {
        let private = MockServer::start().await;
        serve(&private, "internal", &["2.0.0"]).await;
        let default = MockRegistry::new("pypi").with_version("internal", "9.9.9");

        let chain = IndexChain::new(
            vec![
                DeclaredIndex::url(Some("private"), &private.uri()),
                DeclaredIndex::default_registry(),
            ],
            &no_pins(),
            &default,
        )
        .unwrap();

        assert_eq!(chain.get_latest_version("internal").await.unwrap(), "2.0.0");
    }

    #[tokio::test]
    async fn default_answers_when_declared_index_lacks_the_package() {
        let private = MockServer::start().await;
        missing(&private, "requests").await;
        let default = MockRegistry::new("pypi").with_version("requests", "2.32.0");

        let chain = IndexChain::new(
            vec![
                DeclaredIndex::url(Some("private"), &private.uri()),
                DeclaredIndex::default_registry(),
            ],
            &no_pins(),
            &default,
        )
        .unwrap();

        assert_eq!(
            chain.get_latest_version("requests").await.unwrap(),
            "2.32.0"
        );
        assert_eq!(
            chain
                .get_latest_version_including_prereleases("requests")
                .await
                .unwrap(),
            "2.32.0"
        );
        assert_eq!(
            chain
                .get_latest_version_matching("requests", ">=2")
                .await
                .unwrap(),
            "2.32.0"
        );
    }

    #[tokio::test]
    async fn without_a_default_link_the_default_registry_is_never_asked() {
        let private = MockServer::start().await;
        missing(&private, "requests").await;
        let default = MockRegistry::new("pypi").with_version("requests", "2.32.0");

        let chain = IndexChain::new(
            vec![DeclaredIndex::url(Some("private"), &private.uri())],
            &no_pins(),
            &default,
        )
        .unwrap();

        let err = chain.get_latest_version("requests").await.unwrap_err();
        assert!(
            err.to_string().contains("404"),
            "expected the private index's 404, got: {err}"
        );
    }

    #[tokio::test]
    async fn pinned_package_only_consults_its_index() {
        let private = MockServer::start().await;
        serve(&private, "torch", &["2.1.0"]).await;
        // The default registry would answer with a higher version; a pinned
        // package must not see it.
        let default = MockRegistry::new("pypi")
            .with_version("torch", "99.0.0")
            .with_version("numpy", "99.0.0");

        let mut pins = HashMap::new();
        pins.insert("Torch".to_string(), "pytorch".to_string());
        let chain = IndexChain::new(
            vec![
                DeclaredIndex::url(Some("pytorch"), &private.uri()).explicit(),
                DeclaredIndex::default_registry(),
            ],
            &pins,
            &default,
        )
        .unwrap();

        assert_eq!(chain.get_latest_version("torch").await.unwrap(), "2.1.0");
        // Unpinned packages skip the explicit index entirely.
        assert_eq!(chain.get_latest_version("numpy").await.unwrap(), "99.0.0");
        assert_eq!(chain.len(), 1);
    }

    #[tokio::test]
    async fn pin_to_an_undeclared_index_falls_back_to_the_chain() {
        let default = MockRegistry::new("pypi").with_version("torch", "1.2.3");
        let mut pins = HashMap::new();
        pins.insert("torch".to_string(), "nowhere".to_string());
        let chain =
            IndexChain::new(vec![DeclaredIndex::default_registry()], &pins, &default).unwrap();

        assert_eq!(chain.get_latest_version("torch").await.unwrap(), "1.2.3");
    }

    #[tokio::test]
    async fn included_package_is_looked_up_only_on_the_including_index() {
        let private = MockServer::start().await;
        serve(&private, "foo-bar", &["1.0.0"]).await;
        missing(&private, "requests").await;
        // The default registry carries a higher version of the included
        // package; the include pattern must keep it out of the lookup even
        // though the default link comes first in the chain.
        let default = MockRegistry::new("pypi")
            .with_version("foo-bar", "99.0.0")
            .with_version("Foo_Bar", "99.0.0")
            .with_version("requests", "2.32.0");

        let chain = IndexChain::new(
            vec![
                DeclaredIndex::default_registry(),
                DeclaredIndex::url(Some("private"), &private.uri())
                    .with_package_filters(strings(&["foo-*"]), Vec::new()),
            ],
            &no_pins(),
            &default,
        )
        .unwrap();

        assert_eq!(chain.get_latest_version("foo-bar").await.unwrap(), "1.0.0");
        // Patterns match the normalized name, so `Foo_Bar` is `foo-bar` too.
        assert_eq!(chain.get_latest_version("Foo_Bar").await.unwrap(), "1.0.0");
        // Packages no pattern includes still walk the whole chain in order.
        assert_eq!(
            chain.get_latest_version("requests").await.unwrap(),
            "2.32.0"
        );
    }

    #[tokio::test]
    async fn excluded_package_skips_the_index() {
        let private = MockServer::start().await;
        never_asked(&private, "requests").await;
        serve(&private, "internal", &["3.0.0"]).await;
        let default = MockRegistry::new("pypi")
            .with_version("requests", "2.32.0")
            .with_version("internal", "0.0.1");

        let chain = IndexChain::new(
            vec![
                DeclaredIndex::url(Some("private"), &private.uri())
                    .with_package_filters(Vec::new(), strings(&["requests", "urllib*"])),
                DeclaredIndex::default_registry(),
            ],
            &no_pins(),
            &default,
        )
        .unwrap();

        assert_eq!(
            chain.get_latest_version("requests").await.unwrap(),
            "2.32.0"
        );
        assert_eq!(chain.get_latest_version("internal").await.unwrap(), "3.0.0");
    }

    #[tokio::test]
    async fn a_pin_overrides_package_filters() {
        let pinned = MockServer::start().await;
        serve(&pinned, "foo", &["1.0.0"]).await;
        let other = MockServer::start().await;
        never_asked(&other, "foo").await;
        let default = MockRegistry::new("pypi").with_version("foo", "99.0.0");

        let mut pins = HashMap::new();
        pins.insert("foo".to_string(), "pinned".to_string());
        let chain = IndexChain::new(
            vec![
                DeclaredIndex::url(Some("pinned"), &pinned.uri()),
                DeclaredIndex::url(Some("other"), &other.uri())
                    .with_package_filters(strings(&["foo"]), Vec::new()),
                DeclaredIndex::default_registry(),
            ],
            &pins,
            &default,
        )
        .unwrap();

        assert_eq!(chain.get_latest_version("foo").await.unwrap(), "1.0.0");
    }

    #[test]
    fn an_unparsable_pattern_matches_literally_instead_of_vanishing() {
        let globs = package_globs(&strings(&["foo["])).unwrap();
        assert!(globs.is_match("foo["));
        assert!(!globs.is_match("foo"));
        assert!(!globs.is_match("foobar"));

        assert!(package_globs(&[]).is_none());
    }

    #[tokio::test]
    async fn explicit_only_chain_reports_no_index_configured() {
        let private = MockServer::start().await;
        let default = MockRegistry::new("pypi").with_version("requests", "1.0.0");
        let chain = IndexChain::new(
            vec![DeclaredIndex::url(Some("private"), &private.uri()).explicit()],
            &no_pins(),
            &default,
        )
        .unwrap();

        assert!(chain.is_empty());
        let err = chain.get_latest_version("requests").await.unwrap_err();
        assert!(err.to_string().contains("No package index is configured"));
    }

    /// A chain is a decorator: `Link::Default` holds an arbitrary registry, so
    /// ref lookups have to travel to it. Answering from the chain itself would
    /// report "no refs" for a registry that publishes them, which the caller
    /// cannot tell from a real answer.
    #[tokio::test]
    async fn chain_forwards_ref_lookups_to_the_link_that_can_answer() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let default = MockRegistry::new("github-releases")
            .with_ref_names("actions/checkout", &["v4.2.2", "v4"])
            .with_resolved_ref("actions/checkout", "v4", sha);

        let chain = IndexChain::new(
            vec![DeclaredIndex::default_registry()],
            &no_pins(),
            &default,
        )
        .unwrap();

        assert_eq!(
            chain.list_ref_names("actions/checkout").await.unwrap(),
            strings(&["v4.2.2", "v4"]),
            "the chain must return the link's refs, not its own empty answer"
        );
        assert_eq!(
            chain
                .resolve_ref_to_commit("actions/checkout", "v4")
                .await
                .unwrap(),
            sha
        );
    }

    /// A lookup that could not complete is not the same fact as a repository
    /// with no refs, and the chain is the last place that distinction can be
    /// lost: an empty list here tells the caller the refs were checked.
    #[tokio::test]
    async fn a_failed_ref_listing_stays_an_error_through_the_chain() {
        let default =
            MockRegistry::new("github-releases").with_unavailable_ref_names("actions/checkout");

        let chain = IndexChain::new(
            vec![DeclaredIndex::default_registry()],
            &no_pins(),
            &default,
        )
        .unwrap();

        let error = chain
            .list_ref_names("actions/checkout")
            .await
            .expect_err("a failed listing must not be reported as an empty ref list");
        assert!(
            error.to_string().contains("actions/checkout"),
            "the link's error must survive, got: {error}"
        );
    }

    /// A link that answers "no refs here" is a real answer, so the chain keeps
    /// reporting it as one.
    #[tokio::test]
    async fn a_link_with_no_ref_concept_is_still_an_empty_answer() {
        let default = MockRegistry::new("pypi");

        let chain = IndexChain::new(
            vec![DeclaredIndex::default_registry()],
            &no_pins(),
            &default,
        )
        .unwrap();

        assert!(
            chain.list_ref_names("requests").await.unwrap().is_empty(),
            "a registry without ref data answers empty, not an error"
        );
    }

    /// The cooldown layer reads an empty version list as "this registry holds
    /// no publish dates" and says so to the user. Flattening a failed lookup
    /// into that answer here reports a transient outage as a permanent
    /// registry limitation.
    #[tokio::test]
    async fn a_failed_version_listing_stays_an_error_through_the_chain() {
        let default = MockRegistry::new("pypi").with_unavailable_versions("requests");

        let chain = IndexChain::new(
            vec![DeclaredIndex::default_registry()],
            &no_pins(),
            &default,
        )
        .unwrap();

        let error = chain
            .list_versions("requests")
            .await
            .expect_err("a failed listing must not be reported as an empty version list");
        assert!(
            error.to_string().contains("requests"),
            "the link's error must survive, got: {error}"
        );
    }

    /// A link answers empty when it does not hold the package at all, which says
    /// nothing about publish dates. Letting that suppress a later link's failure
    /// would report an outage on the link that does hold the package as a
    /// registry unable to support cooldown, which is the confusion this chain
    /// exists to avoid.
    #[tokio::test]
    async fn a_link_without_the_package_does_not_mask_a_later_failure() {
        let private = MockServer::start().await;
        missing(&private, "requests").await;
        let default = MockRegistry::new("pypi").with_unavailable_versions("requests");

        let chain = IndexChain::new(
            vec![
                DeclaredIndex::url(Some("private"), &private.uri()),
                DeclaredIndex::default_registry(),
            ],
            &no_pins(),
            &default,
        )
        .unwrap();

        chain
            .list_versions("requests")
            .await
            .expect_err("an earlier link's 404 is not an answer about publish dates");
    }

    /// A registry that answers "I hold no publish dates" is answering, so the
    /// chain keeps reporting that as the empty list it is.
    #[tokio::test]
    async fn a_link_without_publish_dates_is_still_an_empty_answer() {
        let default = MockRegistry::new("nuget").with_version("requests", "2.31.0");

        let chain = IndexChain::new(
            vec![DeclaredIndex::default_registry()],
            &no_pins(),
            &default,
        )
        .unwrap();

        assert!(
            chain.list_versions("requests").await.unwrap().is_empty(),
            "a registry without publish dates answers empty, not an error"
        );
    }
}
