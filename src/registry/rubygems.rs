use super::{Registry, VersionMeta, get_with_retry, http_error_message};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct RubyGemsRegistry {
    client: Client,
    api_url: String,
}

#[derive(Debug, Deserialize)]
struct GemInfo {
    version: String,
}

#[derive(Debug, Deserialize)]
struct GemVersion {
    number: String,
    prerelease: bool,
    /// RubyGems marks yanked releases with this flag. Older API responses
    /// omit the field; `serde(default)` treats that as "not yanked".
    #[serde(default)]
    yanked: bool,
    #[serde(default)]
    created_at: Option<String>,
}

impl RubyGemsRegistry {
    pub fn new() -> Self {
        Self::with_api_url("https://rubygems.org".to_string())
    }

    pub fn with_api_url(api_url: String) -> Self {
        let client = crate::http::apply(
            Client::builder()
                .gzip(true)
                .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10)),
        )
        .build()
        .expect("Failed to create HTTP client. This usually indicates a TLS/SSL configuration issue on your system.");

        Self { client, api_url }
    }
}

impl Default for RubyGemsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for RubyGemsRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let url = format!("{}/api/v1/gems/{}.json", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Gem",
                package,
                None
            )));
        }

        let gem_info: GemInfo = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse RubyGems response for '{}': {}", package, e))?;

        Ok(gem_info.version)
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let url = format!("{}/api/v1/versions/{}.json", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Gem",
                package,
                None
            )));
        }

        let versions: Vec<GemVersion> = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse RubyGems versions for '{}': {}", package, e))?;

        // Versions are returned newest first by RubyGems API
        versions
            .iter()
            .find(|v| !v.yanked)
            .map(|v| v.number.clone())
            .ok_or_else(|| anyhow!("Gem '{}' has no versions", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let url = format!("{}/api/v1/versions/{}.json", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Gem",
                package,
                None
            )));
        }

        let versions: Vec<GemVersion> = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse RubyGems versions for '{}': {}", package, e))?;

        // Parse the constraint (e.g., "~> 7.1", ">= 4.9.0")
        // For now, return latest stable version that satisfies semver constraints
        for version in &versions {
            if version.prerelease || version.yanked {
                continue;
            }

            if matches_ruby_constraint(&version.number, constraints) {
                return Ok(version.number.clone());
            }
        }

        Err(anyhow!(
            "No version of gem '{}' matches constraints '{}'",
            package,
            constraints
        ))
    }

    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        let url = format!("{}/api/v1/versions/{}.json", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(anyhow!(http_error_message(status, "Gem", package, None)));
        }

        let items: Vec<GemVersion> = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse RubyGems versions for '{}': {}", package, e))?;

        Ok(items
            .into_iter()
            .map(|v| {
                let published_at = v
                    .created_at
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc));
                VersionMeta {
                    version: v.number,
                    published_at,
                    yanked: v.yanked,
                    prerelease: v.prerelease,
                }
            })
            .collect())
    }

    /// Gems are not pinned to Git refs.
    async fn list_ref_names(&self, _package: &str) -> Result<Vec<String>> {
        super::no_ref_names()
    }

    /// Gems are not pinned to Git refs.
    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        Err(super::ref_resolution_unsupported(
            self.name(),
            package,
            reference,
        ))
    }

    async fn tags_at_commit(&self, _package: &str, _commit: &str) -> Result<super::TagsAtCommit> {
        super::tags_at_commit_unsupported()
    }

    fn name(&self) -> &'static str {
        "rubygems"
    }
}

/// Check if a version matches a Ruby version constraint.
///
/// Supports ~> (pessimistic), >=, <=, >, <, =, != operators. A Gemfile may state
/// several at once (`gem 'rails', '>= 6.0', '< 7.0'`) and RubyGems requires all
/// of them, so a comma-separated list is read as the conjunction it is.
pub(crate) fn matches_ruby_constraint(version: &str, constraint: &str) -> bool {
    constraint
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .all(|c| matches_single_ruby_constraint(version, c))
}

fn matches_single_ruby_constraint(version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();
    // The operator may be written against the version (`~>7.1`) or apart from
    // it (`~> 7.1`), so it is read as a prefix. Two-character operators come
    // first: `>` would otherwise claim the `>` of `>=`.
    let (op, required) = ["~>", ">=", "<=", "!=", "==", ">", "<", "="]
        .into_iter()
        .find_map(|op| constraint.strip_prefix(op).map(|rest| (op, rest.trim())))
        .unwrap_or(("=", constraint));

    let ver = canonical_segments(version);
    let req = canonical_segments(required);
    let ordering = compare_segments(&ver, &req);

    match op {
        ">=" => ordering.is_ge(),
        "<=" => ordering.is_le(),
        ">" => ordering.is_gt(),
        "<" => ordering.is_lt(),
        "=" | "==" => ordering.is_eq(),
        "!=" => ordering.is_ne(),
        // The pessimistic operator is `>= required` together with a ceiling
        // one component up: `~> 2.1` admits 2.9 but not 3.0, and `~> 2.1.0`
        // admits 2.1.9 but not 2.2. The ceiling is tested against the version
        // with its prerelease dropped, so 3.0.0.rc1 is out of `~> 2.1` even
        // though the prerelease itself sorts below 3.0.
        "~>" => match bumped_segments(required) {
            Some(ceiling) => {
                ordering.is_ge() && compare_segments(&release_segments(version), &ceiling).is_lt()
            }
            None => ordering.is_ge(),
        },
        _ => false,
    }
}

/// One component of a RubyGems version. `Gem::Version` reads a version as runs
/// of digits and runs of letters, so `8.1.0.rc1` is `8`, `1`, `0`, `rc`, `1`
/// and the digits in `rc10` are the number ten. A run of letters sorts below
/// any number in the same position, which is what puts a prerelease below the
/// release it precedes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Segment {
    Text(String),
    Number(u64),
}

/// Split a version the way `Gem::Version` does: runs of digits and runs of
/// letters, with everything else read as a separator.
fn segments_of(v: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut chars = v.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut run = String::new();
            while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                run.push(chars.next().unwrap());
            }
            // A run of digits longer than u64 holds is not a version anyone
            // published; saturate rather than drop the component and let a
            // shorter number take its place in the ordering.
            segments.push(Segment::Number(run.parse().unwrap_or(u64::MAX)));
        } else if c.is_ascii_alphabetic() {
            let mut run = String::new();
            while chars.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                run.push(chars.next().unwrap());
            }
            segments.push(Segment::Text(run));
        } else {
            chars.next();
        }
    }
    segments
}

/// The segments `Gem::Version` compares with. A trailing zero is no part of a
/// version, so `8.1.0` and `8.1` are one release; the numeric head and the
/// prerelease tail each drop their own trailing zeros.
fn canonical_segments(v: &str) -> Vec<Segment> {
    let segments = segments_of(v);
    let split_at = segments
        .iter()
        .position(|s| matches!(s, Segment::Text(_)))
        .unwrap_or(segments.len());
    let (head, tail) = segments.split_at(split_at);
    let mut canonical = without_trailing_zeros(head);
    canonical.extend(without_trailing_zeros(tail));
    canonical
}

/// The version with its prerelease dropped: everything from the first run of
/// letters onwards. `8.1.0.rc1` releases as `8.1.0`. The zeros stay, because
/// the pessimistic ceiling is built by raising the component before the last
/// one the requirement states and `~> 2.1.0` ceils a component lower than
/// `~> 2.1` does.
fn release_segments(v: &str) -> Vec<Segment> {
    let segments = segments_of(v);
    let split_at = segments
        .iter()
        .position(|s| matches!(s, Segment::Text(_)))
        .unwrap_or(segments.len());
    segments[..split_at].to_vec()
}

/// The exclusive ceiling the pessimistic operator names: drop the prerelease,
/// drop the last component when more than one remains, and raise what is left.
/// `~> 2.1.0` ceils at 2.2, `~> 2.1` at 3, `~> 2` at 3.
fn bumped_segments(required: &str) -> Option<Vec<Segment>> {
    let mut segments = release_segments(required);
    if segments.len() > 1 {
        segments.pop();
    }
    match segments.pop() {
        Some(Segment::Number(n)) => {
            segments.push(Segment::Number(n.saturating_add(1)));
            Some(segments)
        }
        // A requirement with no numeric component names no ceiling to bump.
        _ => None,
    }
}

fn without_trailing_zeros(segments: &[Segment]) -> Vec<Segment> {
    let end = segments
        .iter()
        .rposition(|s| !matches!(s, Segment::Number(0)))
        .map_or(0, |i| i + 1);
    segments[..end].to_vec()
}

/// Compare two canonical segment lists. A component the shorter version does
/// not state is the number zero, which is how `8.1` and `8.1.0.0` compare
/// equal while `8.1.0.rc1` stays below both.
fn compare_segments(left: &[Segment], right: &[Segment]) -> std::cmp::Ordering {
    let zero = Segment::Number(0);
    for i in 0..left.len().max(right.len()) {
        let l = left.get(i).unwrap_or(&zero);
        let r = right.get(i).unwrap_or(&zero);
        match l.cmp(r) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_get_latest_version() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/gems/rails.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"name":"rails","version":"7.2.1"}"#),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry.get_latest_version("rails").await.unwrap();
        assert_eq!(version, "7.2.1");
    }

    #[tokio::test]
    async fn test_package_not_found() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/gems/nonexistent-gem-xyz.json"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let result = registry.get_latest_version("nonexistent-gem-xyz").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_registry_name() {
        let registry = RubyGemsRegistry::new();
        assert_eq!(registry.name(), "rubygems");
    }

    #[tokio::test]
    async fn test_get_latest_including_prereleases() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"number":"8.0.0.beta1","prerelease":true},
                    {"number":"7.2.1","prerelease":false},
                    {"number":"7.2.0","prerelease":false}
                ]"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version_including_prereleases("rails")
            .await
            .unwrap();
        assert_eq!(version, "8.0.0.beta1");
    }

    #[test]
    fn test_matches_ruby_constraint_pessimistic() {
        // ~> 7.1 means >= 7.1 and < 8.0
        assert!(matches_ruby_constraint("7.1.0", "~> 7.1"));
        assert!(matches_ruby_constraint("7.2.3", "~> 7.1"));
        assert!(matches_ruby_constraint("7.99.0", "~> 7.1"));
        assert!(!matches_ruby_constraint("8.0.0", "~> 7.1"));
        assert!(!matches_ruby_constraint("6.0.0", "~> 7.1"));

        // ~> 7.1.0 means >= 7.1.0 and < 7.2.0
        assert!(matches_ruby_constraint("7.1.0", "~> 7.1.0"));
        assert!(matches_ruby_constraint("7.1.5", "~> 7.1.0"));
        assert!(!matches_ruby_constraint("7.2.0", "~> 7.1.0"));
        assert!(!matches_ruby_constraint("7.0.0", "~> 7.1.0"));
    }

    #[tokio::test]
    async fn test_get_latest_version_matching_pessimistic() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"number": "8.0.4", "prerelease": false},
                    {"number": "7.2.3", "prerelease": false},
                    {"number": "7.1.5", "prerelease": false},
                    {"number": "6.1.7", "prerelease": false}
                ]"#,
            ))
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        // ~> 7.1 should match >= 7.1, < 8.0
        let version = registry
            .get_latest_version_matching("rails", "~> 7.1")
            .await
            .unwrap();
        assert_eq!(version, "7.2.3");
    }

    #[tokio::test]
    async fn test_get_latest_version_matching_no_match() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/oldgem.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"[{"number": "2.0.0", "prerelease": false}]"#),
            )
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let result = registry
            .get_latest_version_matching("oldgem", "~> 1.0")
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_matches_ruby_constraint_comparison() {
        assert!(matches_ruby_constraint("5.0.0", ">= 4.9.0"));
        assert!(matches_ruby_constraint("4.9.0", ">= 4.9.0"));
        assert!(!matches_ruby_constraint("4.8.0", ">= 4.9.0"));

        assert!(matches_ruby_constraint("1.5.3", "< 2.0.0"));
        assert!(!matches_ruby_constraint("2.0.0", "< 2.0.0"));

        assert!(matches_ruby_constraint("1.5.4", "= 1.5.4"));
        assert!(!matches_ruby_constraint("1.5.5", "= 1.5.4"));
    }

    /// `Gem::Version` treats a trailing zero as no part of the version, so
    /// `8.1` and `8.1.0` are one release under every operator. Reading the
    /// segments as a plain list orders the shorter one first, which turns
    /// `!= 8.1` into a constraint the release it names satisfies.
    #[test]
    fn a_trailing_zero_names_the_same_release() {
        assert!(matches_ruby_constraint("8.1.0", "= 8.1"));
        assert!(matches_ruby_constraint("8.1", "= 8.1.0"));
        assert!(!matches_ruby_constraint("8.1.0", "!= 8.1"));
        assert!(!matches_ruby_constraint("8.1", "!= 8.1.0"));
        assert!(!matches_ruby_constraint("8.1.0", "> 8.1"));
        assert!(!matches_ruby_constraint("8.1.0", "< 8.1"));
        assert!(matches_ruby_constraint("8.1.0", ">= 8.1"));
        assert!(matches_ruby_constraint("8.1.0", "<= 8.1"));
        assert!(matches_ruby_constraint("8.1.0.0", "= 8.1"));

        // A zero that is not trailing still separates two releases.
        assert!(!matches_ruby_constraint("8.1.0", "= 8.0.1"));
        assert!(matches_ruby_constraint("8.1.1", "> 8.1"));

        // The numeric head drops its own trailing zeros even when a prerelease
        // follows, so `8.1.0.rc1` and `8.1.rc1` are one release. Keeping them
        // would leave a zero standing where the other version has its first
        // letter, and a number outranks any text in the same position.
        assert!(matches_ruby_constraint("8.1.0.rc1", "= 8.1.rc1"));
        assert!(matches_ruby_constraint("8.1.rc1", "= 8.1.0.rc1"));
        assert!(!matches_ruby_constraint("8.1.0.rc1", "> 8.1.rc1"));
        assert!(!matches_ruby_constraint("8.1.0.rc1", "!= 8.1.rc1"));
    }

    /// A prerelease sorts below the release it precedes: `8.1.0.rc1` is not
    /// `8.1.0`. Dropping the segments that do not parse as numbers makes the
    /// two compare equal, so a constraint that rules the release out admits
    /// its prerelease and one that requires it accepts the prerelease instead.
    #[test]
    fn a_prerelease_sorts_below_the_release_it_precedes() {
        assert!(matches_ruby_constraint("8.1.0.rc1", "< 8.1.0"));
        assert!(!matches_ruby_constraint("8.1.0.rc1", ">= 8.1.0"));
        assert!(!matches_ruby_constraint("8.1.0.rc1", "= 8.1.0"));
        assert!(matches_ruby_constraint("8.1.0.rc1", "!= 8.1.0"));
        assert!(matches_ruby_constraint("8.1.0.rc2", "> 8.1.0.rc1"));
        assert!(matches_ruby_constraint("8.1.0", "> 8.1.0.rc1"));

        // RubyGems splits a segment where the character class changes, so the
        // digits in `rc10` are the number ten rather than the text "10".
        assert!(matches_ruby_constraint("8.1.0.rc10", "> 8.1.0.rc2"));

        // The pessimistic ceiling is tested against the release a prerelease
        // qualifies, so 8.2.0.beta1 is inside `~> 8.1` and 9.0.0.beta1 is not,
        // even though the prerelease itself sorts below 9.
        assert!(matches_ruby_constraint("8.2.0.beta1", "~> 8.1"));
        assert!(!matches_ruby_constraint("9.0.0.beta1", "~> 8.1"));

        // A run of letters is below the zero a missing component stands for.
        assert!(matches_ruby_constraint("1.0.a", "< 1.0"));
        assert!(matches_ruby_constraint("1.0", "> 1.0.a"));
    }

    /// The pessimistic operator raises the component before the last one the
    /// requirement states, so how many it states is what sets the ceiling. A
    /// requirement of one component still has one: `~> 8` stops below 9.
    #[test]
    fn the_pessimistic_ceiling_follows_the_components_the_requirement_states() {
        assert!(matches_ruby_constraint("8.5.0", "~> 8"));
        assert!(!matches_ruby_constraint("9.0.0", "~> 8"));
        assert!(!matches_ruby_constraint("7.9.0", "~> 8"));

        // Written without a space between the operator and the version.
        assert!(matches_ruby_constraint("7.2.3", "~>7.1"));
        assert!(!matches_ruby_constraint("8.0.0", "~>7.1"));
        assert!(matches_ruby_constraint("5.0.0", ">=4.9.0"));
    }

    #[tokio::test]
    async fn test_get_latest_including_prereleases_skips_yanked() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"number":"8.0.1","prerelease":false,"yanked":true},
                    {"number":"8.0.0.beta1","prerelease":true,"yanked":false},
                    {"number":"7.2.1","prerelease":false,"yanked":false}
                ]"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version_including_prereleases("rails")
            .await
            .unwrap();
        assert_eq!(
            version, "8.0.0.beta1",
            "yanked 8.0.1 must be skipped, next newest returned"
        );
    }

    #[tokio::test]
    async fn test_get_latest_version_matching_skips_yanked() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"number":"7.2.3","prerelease":false,"yanked":true},
                    {"number":"7.1.5","prerelease":false,"yanked":false},
                    {"number":"7.1.4","prerelease":false,"yanked":false}
                ]"#,
            ))
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version_matching("rails", "~> 7.1")
            .await
            .unwrap();
        assert_eq!(
            version, "7.1.5",
            "yanked 7.2.3 must be skipped even when it matches the constraint"
        );
    }

    #[tokio::test]
    async fn test_get_latest_version_matching_accepts_missing_yanked_field() {
        // Older RubyGems responses may omit the `yanked` field entirely; we
        // must treat that as "not yanked" and still return the version.
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"[{"number":"7.1.5","prerelease":false}]"#),
            )
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version_matching("rails", "~> 7.1")
            .await
            .unwrap();
        assert_eq!(version, "7.1.5");
    }

    #[tokio::test]
    async fn test_rubygems_list_versions_returns_publish_dates() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/versions/rails.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
              {"number": "7.1.0", "created_at": "2023-10-05T10:00:00Z", "yanked": false, "prerelease": false},
              {"number": "7.0.8", "created_at": "2023-11-08T10:00:00Z", "yanked": true, "prerelease": false},
              {"number": "6.0.0.rc1", "created_at": "2019-04-24T10:00:00Z", "yanked": false, "prerelease": true}
            ]"#,
            ))
            .mount(&mock_server)
            .await;

        let registry = RubyGemsRegistry::with_api_url(mock_server.uri());
        let versions = registry.list_versions("rails").await.unwrap();

        assert_eq!(versions.len(), 3);
        let rc = versions.iter().find(|v| v.version == "6.0.0.rc1").unwrap();
        assert!(
            rc.prerelease,
            "6.0.0.rc1 should be recognised as pre-release"
        );
        let stable = versions.iter().find(|v| v.version == "7.1.0").unwrap();
        assert!(!stable.prerelease);
        assert!(stable.published_at.is_some());
        let yanked_entry = versions.iter().find(|v| v.version == "7.0.8").unwrap();
        assert!(
            yanked_entry.yanked,
            "yanked flag should round-trip from API"
        );
    }
}
