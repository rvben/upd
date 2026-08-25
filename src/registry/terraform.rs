use super::{Registry, VersionMeta, get_with_retry, http_error_message};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct TerraformRegistry {
    client: Client,
    api_url: String,
}

#[derive(Debug, Deserialize)]
struct ProviderVersionsResponse {
    versions: Vec<ProviderVersion>,
}

#[derive(Debug, Deserialize)]
struct ProviderVersion {
    version: String,
}

#[derive(Debug, Deserialize)]
struct ModuleVersionsResponse {
    modules: Vec<ModuleVersionList>,
}

#[derive(Debug, Deserialize)]
struct ModuleVersionList {
    versions: Vec<ModuleVersion>,
}

#[derive(Debug, Deserialize)]
struct ModuleVersion {
    version: String,
}

impl TerraformRegistry {
    pub fn new() -> Self {
        Self::with_api_url("https://registry.terraform.io".to_string())
    }

    #[cfg(test)]
    pub fn with_api_url(api_url: String) -> Self {
        let client = crate::http::apply(
            Client::builder()
                .gzip(true)
                .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10)),
        )
        .build()
        .expect("Failed to create HTTP client");

        Self { client, api_url }
    }

    #[cfg(not(test))]
    fn with_api_url(api_url: String) -> Self {
        let client = crate::http::apply(
            Client::builder()
                .gzip(true)
                .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10)),
        )
        .build()
        .expect("Failed to create HTTP client");

        Self { client, api_url }
    }

    /// Determine if a package identifier refers to a module (3 segments) or provider (2 segments)
    fn is_module(package: &str) -> bool {
        package.split('/').count() == 3
    }

    /// Fetch all versions for a provider (namespace/type)
    async fn get_provider_versions(&self, package: &str) -> Result<Vec<String>> {
        let url = format!("{}/v1/providers/{}/versions", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Provider",
                package,
                None
            )));
        }

        let data: ProviderVersionsResponse = response.json().await.map_err(|e| {
            anyhow!(
                "Failed to parse Terraform Registry response for '{}': {}",
                package,
                e
            )
        })?;

        Ok(data.versions.into_iter().map(|v| v.version).collect())
    }

    /// Fetch all versions for a module (namespace/name/provider)
    async fn get_module_versions(&self, package: &str) -> Result<Vec<String>> {
        let url = format!("{}/v1/modules/{}/versions", self.api_url, package);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "Module",
                package,
                None
            )));
        }

        let data: ModuleVersionsResponse = response.json().await.map_err(|e| {
            anyhow!(
                "Failed to parse Terraform Registry response for '{}': {}",
                package,
                e
            )
        })?;

        let versions = data
            .modules
            .into_iter()
            .flat_map(|m| m.versions.into_iter().map(|v| v.version))
            .collect();

        Ok(versions)
    }

    /// Get all versions (dispatches to provider or module endpoint)
    async fn get_all_versions(&self, package: &str) -> Result<Vec<String>> {
        if Self::is_module(package) {
            self.get_module_versions(package).await
        } else {
            self.get_provider_versions(package).await
        }
    }

    /// Find the latest stable version from a list of version strings
    fn find_latest_stable(versions: &[String]) -> Option<String> {
        versions
            .iter()
            .filter(|v| !v.contains('-')) // Skip prereleases (semver convention)
            .filter_map(|v| semver::Version::parse(v).ok().map(|sv| (v.clone(), sv)))
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(v, _)| v)
    }

    /// Find the latest version including prereleases
    fn find_latest_any(versions: &[String]) -> Option<String> {
        versions
            .iter()
            .filter_map(|v| semver::Version::parse(v).ok().map(|sv| (v.clone(), sv)))
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(v, _)| v)
    }
}

impl Default for TerraformRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for TerraformRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let versions = self.get_all_versions(package).await?;

        Self::find_latest_stable(&versions).ok_or_else(|| {
            anyhow!(
                "No stable versions found for '{}' in Terraform Registry",
                package
            )
        })
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let versions = self.get_all_versions(package).await?;

        Self::find_latest_any(&versions)
            .ok_or_else(|| anyhow!("No versions found for '{}' in Terraform Registry", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let versions = self.get_all_versions(package).await?;

        // Parse constraint and find matching versions
        let matching: Vec<_> = versions
            .iter()
            .filter(|v| !v.contains('-')) // Skip prereleases
            .filter(|v| matches_terraform_constraint(v, constraints))
            .filter_map(|v| semver::Version::parse(v).ok().map(|sv| (v.clone(), sv)))
            .collect();

        matching
            .into_iter()
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(v, _)| v)
            .ok_or_else(|| {
                anyhow!(
                    "No version of '{}' matches constraints '{}'",
                    package,
                    constraints
                )
            })
    }

    /// The registry API returns no publish dates, so cooldown cannot apply here.
    async fn list_versions(&self, _package: &str) -> Result<Vec<VersionMeta>> {
        super::no_version_metadata()
    }

    /// Modules are addressed by registry version, not by Git ref.
    async fn list_ref_names(&self, _package: &str) -> Result<Vec<String>> {
        super::no_ref_names()
    }

    /// Modules are addressed by registry version, not by Git ref.
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
        "terraform"
    }
}

/// Check if a version matches a Terraform version constraint string.
/// Supports ~> (pessimistic), >=, <=, >, <, = operators.
/// Multiple constraints can be comma-separated: ">= 5.0, < 6.0"
pub(crate) fn matches_terraform_constraint(version: &str, constraint: &str) -> bool {
    // Split on commas for multiple constraints
    for part in constraint.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !matches_single_constraint(version, part) {
            return false;
        }
    }
    true
}

fn matches_single_constraint(version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();

    // Extract operator and required version
    let (op, required) = if let Some(rest) = constraint.strip_prefix("~>") {
        ("~>", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix(">=") {
        (">=", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix("<=") {
        ("<=", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix("!=") {
        ("!=", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix('>') {
        (">", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix('<') {
        ("<", rest.trim())
    } else if let Some(rest) = constraint.strip_prefix('=') {
        ("=", rest.trim())
    } else {
        ("=", constraint)
    };

    let ver = TerraformVersion::parse(version);
    let req = TerraformVersion::parse(required);
    let ordering = ver.compare(&req);

    match op {
        // Equality reads the versions alone: a prerelease is simply not the
        // release it qualifies, so `= 6.61` rules out 6.61.0-rc1 and
        // `!= 6.61` admits it.
        "=" | "==" => ordering.is_eq(),
        "!=" => ordering.is_ne(),
        ">=" => ver.comparable_with(&req) && ordering.is_ge(),
        "<=" => ver.comparable_with(&req) && ordering.is_le(),
        ">" => ver.comparable_with(&req) && ordering.is_gt(),
        "<" => ver.comparable_with(&req) && ordering.is_lt(),
        "~>" => ver.satisfies_pessimistic(&req),
        _ => false,
    }
}

/// A version as Terraform reads it. Segments are padded to three, so `6.61`
/// and `6.61.0` are one release, and how many were written is remembered
/// because the pessimistic operator bounds on the ones the author stated.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerraformVersion {
    segments: Vec<u64>,
    /// How many segments the string stated, before padding.
    stated: usize,
    prerelease: String,
}

impl TerraformVersion {
    fn parse(v: &str) -> Self {
        let v = v.trim().trim_start_matches('v');
        // Build metadata takes no part in ordering.
        let core = v.split('+').next().unwrap_or(v);
        let (core, prerelease) = match core.split_once('-') {
            Some((core, pre)) => (core, pre.to_string()),
            None => (core, String::new()),
        };
        // A segment that is not a number is no version Terraform would accept;
        // reading it as zero keeps the comparison total instead of dropping the
        // component and shifting every later one into its place.
        let mut segments: Vec<u64> = core
            .split('.')
            .map(|s| s.trim().parse().unwrap_or(0))
            .collect();
        let stated = segments.len();
        while segments.len() < 3 {
            segments.push(0);
        }
        Self {
            segments,
            stated,
            prerelease,
        }
    }

    fn compare(&self, other: &Self) -> std::cmp::Ordering {
        for i in 0..self.segments.len().max(other.segments.len()) {
            let left = self.segments.get(i).copied().unwrap_or(0);
            let right = other.segments.get(i).copied().unwrap_or(0);
            match left.cmp(&right) {
                std::cmp::Ordering::Equal => continue,
                other => return other,
            }
        }
        compare_prereleases(&self.prerelease, &other.prerelease)
    }

    /// Whether an ordering comparison against `required` is meaningful at all.
    /// A requirement that names no prerelease is about released versions, so a
    /// prerelease never satisfies it; a requirement that does name one speaks
    /// only of prereleases of the very release it names.
    fn comparable_with(&self, required: &Self) -> bool {
        match (!self.prerelease.is_empty(), !required.prerelease.is_empty()) {
            (true, true) => self.segments == required.segments,
            (true, false) => false,
            _ => true,
        }
    }

    /// The pessimistic operator: at least `required`, with every segment before
    /// the last one stated held equal. `~> 5.1` keeps the major, `~> 5.1.0`
    /// keeps major and minor, and `~> 5` bounds nothing above.
    fn satisfies_pessimistic(&self, required: &Self) -> bool {
        if !self.comparable_with(required) {
            return false;
        }
        // A prerelease requirement admits only prereleases.
        if !required.prerelease.is_empty() && self.prerelease.is_empty() {
            return false;
        }
        if self.compare(required).is_lt() {
            return false;
        }
        if required.segments.len() > self.segments.len() {
            return false;
        }
        for i in 0..required.stated.saturating_sub(1) {
            if self.segments.get(i) != required.segments.get(i) {
                return false;
            }
        }
        let last = required.segments.len() - 1;
        required.segments[last] <= self.segments.get(last).copied().unwrap_or(0)
    }
}

/// Compare two prerelease strings. A release outranks any prerelease of it, and
/// the prereleases themselves compare identifier by identifier: one that is all
/// digits compares as the number it is and ranks below one that is not.
fn compare_prereleases(left: &str, right: &str) -> std::cmp::Ordering {
    if left == right {
        return std::cmp::Ordering::Equal;
    }
    match (left.is_empty(), right.is_empty()) {
        (true, false) => return std::cmp::Ordering::Greater,
        (false, true) => return std::cmp::Ordering::Less,
        _ => {}
    }
    let left: Vec<&str> = left.split('.').collect();
    let right: Vec<&str> = right.split('.').collect();
    for i in 0..left.len().max(right.len()) {
        let l = left.get(i).copied().unwrap_or("");
        let r = right.get(i).copied().unwrap_or("");
        match compare_prerelease_part(l, r) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

fn compare_prerelease_part(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if left == right {
        return Ordering::Equal;
    }
    let left_number = left.parse::<u64>().ok();
    let right_number = right.parse::<u64>().ok();
    // An identifier the other side does not have ranks below a number and
    // above anything else, which is what makes `rc` precede `rc.1`.
    if left.is_empty() {
        return if right_number.is_some() {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    if right.is_empty() {
        return if left_number.is_some() {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    match (left_number, right_number) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_get_latest_provider_version() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/providers/hashicorp/aws/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"versions": [{"version": "5.83.0"}, {"version": "5.82.0"}, {"version": "4.67.0"}]}"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = TerraformRegistry::with_api_url(mock_server.uri());
        let version = registry.get_latest_version("hashicorp/aws").await.unwrap();
        assert_eq!(version, "5.83.0");
    }

    #[tokio::test]
    async fn test_get_latest_module_version() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/modules/terraform-aws-modules/vpc/aws/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"modules": [{"versions": [{"version": "5.1.0"}, {"version": "5.0.0"}, {"version": "4.0.0"}]}]}"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = TerraformRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version("terraform-aws-modules/vpc/aws")
            .await
            .unwrap();
        assert_eq!(version, "5.1.0");
    }

    #[tokio::test]
    async fn test_provider_not_found() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/providers/nonexistent/provider/versions"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = TerraformRegistry::with_api_url(mock_server.uri());
        let result = registry.get_latest_version("nonexistent/provider").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_registry_name() {
        let registry = TerraformRegistry::new();
        assert_eq!(registry.name(), "terraform");
    }

    #[tokio::test]
    async fn test_skips_prereleases() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/providers/hashicorp/aws/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"versions": [{"version": "6.0.0-beta1"}, {"version": "5.83.0"}, {"version": "5.82.0"}]}"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = TerraformRegistry::with_api_url(mock_server.uri());
        let version = registry.get_latest_version("hashicorp/aws").await.unwrap();
        assert_eq!(version, "5.83.0");
    }

    #[test]
    fn test_matches_terraform_constraint_pessimistic() {
        // ~> 5.0 means >= 5.0 and < 6.0
        assert!(matches_terraform_constraint("5.0.0", "~> 5.0"));
        assert!(matches_terraform_constraint("5.83.0", "~> 5.0"));
        assert!(!matches_terraform_constraint("6.0.0", "~> 5.0"));
        assert!(!matches_terraform_constraint("4.0.0", "~> 5.0"));

        // ~> 5.1.0 means >= 5.1.0 and < 5.2.0
        assert!(matches_terraform_constraint("5.1.0", "~> 5.1.0"));
        assert!(matches_terraform_constraint("5.1.5", "~> 5.1.0"));
        assert!(!matches_terraform_constraint("5.2.0", "~> 5.1.0"));
        assert!(!matches_terraform_constraint("5.0.0", "~> 5.1.0"));
    }

    #[test]
    fn test_matches_terraform_constraint_comparison() {
        assert!(matches_terraform_constraint("5.0.0", ">= 4.0.0"));
        assert!(!matches_terraform_constraint("3.0.0", ">= 4.0.0"));
        assert!(matches_terraform_constraint("5.0.0", "< 6.0.0"));
        assert!(!matches_terraform_constraint("6.0.0", "< 6.0.0"));
    }

    /// Terraform pads a version to the length of the one it is compared with,
    /// so `6.61` and `6.61.0` are one release under every operator. Reading the
    /// segments as a plain list orders the shorter one first, which turns
    /// `!= 6.61` into a constraint the release it names satisfies.
    #[test]
    fn a_missing_segment_reads_as_zero() {
        assert!(matches_terraform_constraint("6.61.0", "= 6.61"));
        assert!(matches_terraform_constraint("6.61", "= 6.61.0"));
        assert!(!matches_terraform_constraint("6.61.0", "!= 6.61"));
        assert!(!matches_terraform_constraint("6.61", "!= 6.61.0"));
        assert!(!matches_terraform_constraint("6.61.0", "> 6.61"));
        assert!(!matches_terraform_constraint("6.61.0", "< 6.61"));
        assert!(matches_terraform_constraint("6.61.0", ">= 6.61"));
        assert!(matches_terraform_constraint("6.61.0", "<= 6.61"));

        // A zero that is not trailing still separates two releases.
        assert!(!matches_terraform_constraint("6.61.0", "= 6.0.61"));
        assert!(matches_terraform_constraint("6.61.1", "> 6.61"));

        // The pessimistic operator reads how many segments each side carries,
        // so the padding has to be in place before the lengths are compared.
        // `6.61` is `6.61.0`, and a constraint may not out-state a version that
        // names the same release.
        assert!(matches_terraform_constraint("6.61", "~> 6.61.0"));
    }

    /// A constraint that names no prerelease is about released versions, so a
    /// prerelease satisfies none of its ordering operators however the numbers
    /// compare. Dropping the segment that carries the prerelease hides that:
    /// `6.61.1-rc1` reads as `6.61` and passes for a release.
    #[test]
    fn a_prerelease_answers_only_to_a_constraint_that_names_one() {
        assert!(!matches_terraform_constraint("6.61.0-rc1", "< 6.61.0"));
        assert!(!matches_terraform_constraint("6.61.0-rc1", ">= 6.61.0"));
        assert!(!matches_terraform_constraint("6.61.1-rc1", "> 6.61.0"));
        assert!(!matches_terraform_constraint("6.61.0-rc1", "~> 6.61.0"));
        assert!(!matches_terraform_constraint("6.62.0-rc1", "> 6.61.0-rc1"));

        // Equality reads the versions alone: a prerelease is not the release it
        // qualifies, so the exclusion admits it and the pin rules it out.
        assert!(!matches_terraform_constraint("6.61.0-rc1", "= 6.61"));
        assert!(matches_terraform_constraint("6.61.0-rc1", "!= 6.61"));

        // Between prereleases of one release the identifiers decide, and one
        // that is all digits compares as the number it is.
        assert!(matches_terraform_constraint("6.61.0-rc2", "> 6.61.0-rc1"));
        assert!(matches_terraform_constraint(
            "6.61.0-rc.10",
            "> 6.61.0-rc.2"
        ));
        assert!(matches_terraform_constraint("6.61.0-rc.1", "> 6.61.0-rc"));
    }

    /// The pessimistic operator holds every segment before the last one the
    /// constraint states, so how many it states is what sets the ceiling. A
    /// constraint of one segment states no ceiling at all.
    #[test]
    fn the_pessimistic_ceiling_follows_the_segments_the_constraint_states() {
        assert!(matches_terraform_constraint("5.5.0", "~> 5"));
        assert!(matches_terraform_constraint("6.0.0", "~> 5"));
        assert!(!matches_terraform_constraint("4.9.0", "~> 5"));

        assert!(matches_terraform_constraint("5.2.0", "~> 5.1"));
        assert!(!matches_terraform_constraint("6.0.0", "~> 5.1"));
        assert!(matches_terraform_constraint("5.1.9", "~> 5.1.0"));
        assert!(!matches_terraform_constraint("5.2.0", "~> 5.1.0"));
    }

    #[test]
    fn test_matches_terraform_constraint_compound() {
        // >= 5.0, < 6.0
        assert!(matches_terraform_constraint("5.5.0", ">= 5.0, < 6.0"));
        assert!(!matches_terraform_constraint("6.0.0", ">= 5.0, < 6.0"));
        assert!(!matches_terraform_constraint("4.0.0", ">= 5.0, < 6.0"));
    }

    #[test]
    fn test_is_module() {
        assert!(!TerraformRegistry::is_module("hashicorp/aws"));
        assert!(TerraformRegistry::is_module(
            "terraform-aws-modules/vpc/aws"
        ));
    }
}
