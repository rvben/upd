use super::docker_credentials::{Credential, Credentials};
use super::{
    Registry, TagsAtCommit, VersionMeta, get_with_retry, no_ref_names, ref_resolution_unsupported,
    tags_at_commit_unsupported,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use reqwest::header::{LINK, WWW_AUTHENTICATE};
use serde::Deserialize;
use std::time::Duration;

const LOOKUP_SEPARATOR: char = '\u{1f}';
const MAX_PAGES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageLocation {
    registry: String,
    repository: String,
    docker_hub: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TagPattern {
    prefix: String,
    segments: Vec<u64>,
    suffix: String,
}

impl TagPattern {
    fn parse(tag: &str) -> Option<Self> {
        let (prefix, rest) = tag
            .strip_prefix('v')
            .map_or_else(|| ("", tag), |rest| ("v", rest));
        let numeric_len = rest
            .char_indices()
            .take_while(|(_, ch)| ch.is_ascii_digit() || *ch == '.')
            .map(|(idx, ch)| idx + ch.len_utf8())
            .last()?;
        let numeric = &rest[..numeric_len];
        if numeric.ends_with('.') || numeric.starts_with('.') {
            return None;
        }
        let segments: Vec<u64> = numeric
            .split('.')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        if segments.is_empty() || segments.len() > 4 {
            return None;
        }
        Some(Self {
            prefix: prefix.to_string(),
            segments,
            suffix: rest[numeric_len..].to_string(),
        })
    }

    fn same_channel(&self, other: &Self) -> bool {
        self.prefix == other.prefix
            && self.suffix == other.suffix
            && self.segments.len() == other.segments.len()
    }
}

#[derive(Debug, Deserialize)]
struct DockerHubResponse {
    #[serde(default)]
    results: Vec<DockerHubTag>,
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerHubTag {
    name: String,
    #[serde(default)]
    last_updated: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct OciTagsResponse {
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

pub struct DockerRegistry {
    client: Client,
    docker_hub_api: String,
    oci_base_override: Option<String>,
    github_credentials: Option<(String, String)>,
    credentials: Credentials,
}

impl DockerRegistry {
    pub fn new() -> Self {
        Self::with_endpoints("https://hub.docker.com".to_string(), None)
    }

    #[cfg(test)]
    pub fn with_api_url(api_url: String) -> Self {
        let mut registry = Self::with_endpoints(api_url.clone(), Some(api_url));
        registry.credentials = Credentials::at(None);
        registry.github_credentials = None;
        registry
    }

    fn with_endpoints(docker_hub_api: String, oci_base_override: Option<String>) -> Self {
        let client = crate::http::apply(
            Client::builder()
                .gzip(true)
                // Authentication realms and pagination are validated explicitly.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10)),
        )
        .build()
        .expect("Failed to create HTTP client for container registries");
        Self {
            client,
            docker_hub_api,
            oci_base_override,
            credentials: Credentials::new(),
            github_credentials: std::env::var("GITHUB_ACTOR")
                .ok()
                .zip(std::env::var("GITHUB_TOKEN").ok())
                .filter(|(actor, token)| !actor.is_empty() && !token.is_empty()),
        }
    }

    /// Keep the image name and its current tag together through the generic
    /// Registry interface. Docker tag selection is channel-sensitive, so an
    /// `alpine` lookup at `3.22` is a different query from `edge` or `latest`.
    pub fn lookup_key(image: &str, current_tag: &str) -> String {
        format!("{image}{LOOKUP_SEPARATOR}{current_tag}")
    }

    fn split_lookup_key(package: &str) -> Result<(&str, &str)> {
        package
            .split_once(LOOKUP_SEPARATOR)
            .ok_or_else(|| anyhow!("invalid Docker lookup '{package}': current tag is missing"))
    }

    fn image_location(image: &str) -> Result<ImageLocation> {
        let mut parts = image.split('/');
        let first = parts.next().unwrap_or_default();
        if first.is_empty() {
            return Err(anyhow!("invalid container image '{image}'"));
        }
        let explicit_registry = first.contains('.') || first.contains(':') || first == "localhost";
        let (registry, mut repository) = if explicit_registry {
            let repository = parts.collect::<Vec<_>>().join("/");
            (first.to_ascii_lowercase(), repository)
        } else {
            ("docker.io".to_string(), image.to_string())
        };
        if repository.is_empty() {
            return Err(anyhow!(
                "invalid container image '{image}': repository is missing"
            ));
        }
        let docker_hub = matches!(
            registry.as_str(),
            "docker.io" | "index.docker.io" | "registry-1.docker.io"
        );
        if docker_hub && !repository.contains('/') {
            repository = format!("library/{repository}");
        }
        Ok(ImageLocation {
            registry,
            repository,
            docker_hub,
        })
    }

    fn select_channel(tags: Vec<VersionMeta>, current: &str) -> Result<Vec<VersionMeta>> {
        let current_pattern = TagPattern::parse(current).ok_or_else(|| {
            anyhow!(
                "Docker tag '{current}' is floating or not numerically versioned; use an explicit numeric tag"
            )
        })?;
        let mut matching: Vec<(TagPattern, VersionMeta)> = tags
            .into_iter()
            .filter_map(|meta| {
                let parsed = TagPattern::parse(&meta.version)?;
                current_pattern
                    .same_channel(&parsed)
                    .then_some((parsed, meta))
            })
            .collect();
        matching.sort_by(|(left, _), (right, _)| left.segments.cmp(&right.segments));
        Ok(matching.into_iter().map(|(_, meta)| meta).collect())
    }

    async fn docker_hub_tags(&self, repository: &str) -> Result<Vec<VersionMeta>> {
        let mut url = format!(
            "{}/v2/repositories/{repository}/tags?page_size=100",
            self.docker_hub_api.trim_end_matches('/')
        );
        let mut tags = Vec::new();
        for _ in 0..MAX_PAGES {
            let response = get_with_retry(&self.client, &url).await?;
            if !response.status().is_success() {
                return Err(anyhow!(
                    "Failed to fetch Docker Hub image '{repository}': HTTP {}",
                    response.status()
                ));
            }
            let page: DockerHubResponse = response
                .json()
                .await
                .with_context(|| format!("Failed to parse Docker Hub tags for '{repository}'"))?;
            tags.extend(page.results.into_iter().map(|tag| VersionMeta {
                prerelease: TagPattern::parse(&tag.name).is_some_and(|parsed| {
                    parsed.suffix.starts_with("-rc")
                        || parsed.suffix.starts_with("-beta")
                        || parsed.suffix.starts_with("-alpha")
                }),
                version: tag.name,
                published_at: tag.last_updated,
                yanked: false,
            }));
            let Some(next) = page.next else {
                return Ok(tags);
            };
            url = next;
        }
        Err(anyhow!(
            "Docker Hub tag listing for '{repository}' exceeded {MAX_PAGES} pages"
        ))
    }

    fn parse_bearer_challenge(value: &str) -> Option<(String, String, String)> {
        let (scheme, params) = value.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        // Commas inside quoted values (notably scopes) are not separators.
        let mut quoted = false;
        let mut escaped = false;
        let parts = params.split(|ch| {
            if escaped {
                escaped = false;
                return false;
            }
            if quoted && ch == '\\' {
                escaped = true;
                return false;
            }
            if ch == '"' {
                quoted = !quoted;
            }
            ch == ',' && !quoted
        });
        let mut realm = None;
        let mut service = None;
        let mut scope = None;
        for part in parts {
            let (key, value) = part.trim().split_once('=')?;
            let value = value.trim().trim_matches('"').to_string();
            match key.trim().to_ascii_lowercase().as_str() {
                "realm" => {
                    if realm.replace(value).is_some() {
                        return None;
                    }
                }
                "service" => service = Some(value),
                "scope" => scope = Some(value),
                _ => {}
            }
        }
        Some((
            realm?,
            service.unwrap_or_default(),
            scope.unwrap_or_default(),
        ))
    }

    fn next_page_url(current: &url::Url, link_header: Option<&str>) -> Result<Option<url::Url>> {
        let Some(target) = link_header.and_then(|header| {
            header.split(',').find_map(|part| {
                if !part.contains("rel=\"next\"") && !part.contains("rel=next") {
                    return None;
                }
                let start = part.find('<')? + 1;
                let end = part[start..].find('>')? + start;
                Some(&part[start..end])
            })
        }) else {
            return Ok(None);
        };
        let next = current
            .join(target)
            .with_context(|| format!("registry returned an invalid pagination link '{target}'"))?;
        let same_origin = current.scheme() == next.scheme()
            && current.host_str() == next.host_str()
            && current.port_or_known_default() == next.port_or_known_default();
        if !same_origin || !next.username().is_empty() || next.password().is_some() {
            return Err(anyhow!(
                "registry pagination link changed origin from '{}' to '{}'",
                current.origin().ascii_serialization(),
                next.origin().ascii_serialization()
            ));
        }
        Ok(Some(next))
    }

    fn may_send_github_credentials(location: &ImageLocation, token_url: &url::Url) -> bool {
        location.registry == "ghcr.io"
            && token_url.scheme() == "https"
            && token_url.host_str() == Some("ghcr.io")
            && token_url.port_or_known_default() == Some(443)
            && token_url.path() == "/token"
    }

    fn token_request(
        &self,
        location: &ImageLocation,
        token_url: url::Url,
    ) -> reqwest::RequestBuilder {
        let mut request = self.client.get(token_url.clone());
        if Self::may_send_github_credentials(location, &token_url)
            && let Some((actor, token)) = &self.github_credentials
        {
            request = request.basic_auth(actor, Some(token));
        }
        request
    }

    fn validate_token_url(&self, registry_url: &url::Url, token_url: &url::Url) -> Result<()> {
        // An HTTPS registry may delegate auth to a separate HTTPS service (e.g. GitLab).
        // The environment GHCR fallback retains its stricter, exact-endpoint rule.
        let secure = registry_url.scheme() == "https" && token_url.scheme() == "https";
        #[cfg(test)]
        let secure = secure
            || (self.oci_base_override.is_some() && registry_url.origin() == token_url.origin());
        if !secure
            || token_url.host_str().is_none()
            || !token_url.username().is_empty()
            || token_url.password().is_some()
            || token_url.fragment().is_some()
        {
            return Err(anyhow!(
                "Registry returned an unsafe authentication URL; HTTPS without embedded credentials is required"
            ));
        }
        Ok(())
    }

    async fn bearer_token(
        &self,
        location: &ImageLocation,
        registry_url: &url::Url,
        challenge: (String, String, String),
        credential: Option<&Credential>,
    ) -> Result<String> {
        let mut token_url = url::Url::parse(&challenge.0)
            .map_err(|_| anyhow!("Registry returned an invalid authentication URL"))?;
        self.validate_token_url(registry_url, &token_url)?;
        // Never accept a server-requested push/delete scope or duplicate OAuth parameters.
        let extra: Vec<(String, String)> = token_url
            .query_pairs()
            .filter(|(k, _)| {
                ![
                    "scope",
                    "service",
                    "client_id",
                    "grant_type",
                    "refresh_token",
                    "account",
                    "offline_token",
                ]
                .contains(&k.as_ref())
            })
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        token_url.set_query(None);
        token_url.query_pairs_mut().extend_pairs(extra);
        let scope = format!("repository:{}:pull", location.repository);
        let request = if let Some(Credential::IdentityToken(token)) = credential {
            self.client.post(token_url).form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", token.as_str()),
                ("service", challenge.1.as_str()),
                ("scope", scope.as_str()),
                ("client_id", "upd"),
            ])
        } else {
            token_url
                .query_pairs_mut()
                .append_pair("service", &challenge.1)
                .append_pair("scope", &scope);
            match credential {
                Some(Credential::Basic(username, password)) => self
                    .client
                    .get(token_url)
                    .basic_auth(username, Some(password)),
                _ => self.token_request(location, token_url),
            }
        };
        let response = request.send().await.map_err(|e| e.without_url())?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to authenticate to registry '{}': HTTP {}; check Docker credentials and repository access",
                location.registry,
                response.status()
            ));
        }
        let token: TokenResponse = response
            .json()
            .await
            .map_err(|_| anyhow!("Registry returned an invalid bearer token response"))?;
        token
            .token
            .filter(|s| !s.is_empty())
            .or(token.access_token.filter(|s| !s.is_empty()))
            .ok_or_else(|| anyhow!("Registry returned no bearer token"))
    }

    async fn oci_tags(&self, location: &ImageLocation) -> Result<Vec<VersionMeta>> {
        let base = self
            .oci_base_override
            .clone()
            .unwrap_or_else(|| format!("https://{}", location.registry));
        let initial_url = format!(
            "{}/v2/{}/tags/list?n=1000",
            base.trim_end_matches('/'),
            location.repository
        );
        let mut url = url::Url::parse(&initial_url)
            .with_context(|| format!("invalid registry URL '{initial_url}'"))?;
        let mut bearer = None;
        let mut basic: Option<Credential> = None;
        let mut tags = Vec::new();

        for _ in 0..MAX_PAGES {
            let mut response = if let Some(token) = &bearer {
                self.client
                    .get(url.clone())
                    .bearer_auth(token)
                    .send()
                    .await
                    .map_err(|e| e.without_url())?
            } else if let Some(Credential::Basic(username, password)) = &basic {
                self.client
                    .get(url.clone())
                    .basic_auth(username, Some(password))
                    .send()
                    .await
                    .map_err(|e| e.without_url())?
            } else {
                get_with_retry(&self.client, url.as_str()).await?
            };
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                let challenges: Vec<&str> = response
                    .headers()
                    .get_all(WWW_AUTHENTICATE)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .collect();
                let credential = self.credentials.get(&location.registry).await?;
                if let Some(challenge) = challenges
                    .iter()
                    .find_map(|v| Self::parse_bearer_challenge(v))
                {
                    let token = self
                        .bearer_token(location, &url, challenge, credential.as_ref())
                        .await?;
                    response = self
                        .client
                        .get(url.clone())
                        .bearer_auth(&token)
                        .send()
                        .await
                        .map_err(|e| e.without_url())?;
                    bearer = Some(token);
                } else if challenges.iter().any(|v| {
                    v.split_whitespace()
                        .next()
                        .is_some_and(|s| s.eq_ignore_ascii_case("basic"))
                }) {
                    self.validate_token_url(&url, &url)?;
                    let Some(Credential::Basic(username, password)) = credential else {
                        return Err(anyhow!(
                            "Registry '{}' requires username/password credentials; run docker login for this registry",
                            location.registry
                        ));
                    };
                    response = self
                        .client
                        .get(url.clone())
                        .basic_auth(&username, Some(&password))
                        .send()
                        .await
                        .map_err(|e| e.without_url())?;
                    basic = Some(Credential::Basic(username, password));
                } else {
                    return Err(anyhow!(
                        "Registry '{}' requires unsupported authentication",
                        location.registry
                    ));
                }
            }
            if !response.status().is_success() {
                return Err(anyhow!(
                    "Failed to fetch container image '{}': HTTP {}",
                    location.repository,
                    response.status()
                ));
            }
            let next = Self::next_page_url(
                &url,
                response
                    .headers()
                    .get(LINK)
                    .and_then(|value| value.to_str().ok()),
            )?;
            let page: OciTagsResponse = response.json().await.with_context(|| {
                format!(
                    "Failed to parse tags for container image '{}/{}'",
                    location.registry, location.repository
                )
            })?;
            tags.extend(page.tags.into_iter().map(|tag| VersionMeta {
                prerelease: TagPattern::parse(&tag).is_some_and(|parsed| {
                    parsed.suffix.starts_with("-rc")
                        || parsed.suffix.starts_with("-beta")
                        || parsed.suffix.starts_with("-alpha")
                }),
                version: tag,
                published_at: None,
                yanked: false,
            }));
            let Some(next) = next else {
                return Ok(tags);
            };
            url = next;
        }
        Err(anyhow!(
            "OCI tag listing for '{}' exceeded {MAX_PAGES} pages",
            location.repository
        ))
    }

    async fn channel_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        let (image, current) = Self::split_lookup_key(package)?;
        let location = Self::image_location(image)?;
        let tags = if location.docker_hub {
            match self.docker_hub_tags(&location.repository).await {
                Ok(tags) => tags,
                Err(hub_error) => {
                    let oci_location = ImageLocation {
                        registry: "registry-1.docker.io".to_string(),
                        repository: location.repository.clone(),
                        docker_hub: false,
                    };
                    self.oci_tags(&oci_location).await.map_err(|oci_error| {
                        anyhow!(
                            "Docker Hub API lookup failed ({hub_error}); OCI fallback failed: {oci_error}"
                        )
                    })?
                }
            }
        } else {
            self.oci_tags(&location).await?
        };
        Self::select_channel(tags, current)
    }
}

impl Default for DockerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for DockerRegistry {
    async fn python_releases(
        &self,
        _package: &str,
    ) -> anyhow::Result<Vec<crate::registry::PythonRelease>> {
        anyhow::bail!("registry does not expose Python compatibility metadata")
    }

    async fn get_latest_version(&self, package: &str) -> Result<String> {
        self.channel_versions(package)
            .await?
            .into_iter()
            .last()
            .map(|meta| meta.version)
            .ok_or_else(|| {
                anyhow!(
                    "No compatible Docker tags found for '{}'",
                    package.replace(LOOKUP_SEPARATOR, ":")
                )
            })
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        self.get_latest_version(package).await
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        _constraints: &str,
    ) -> Result<String> {
        self.get_latest_version(package).await
    }

    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        self.channel_versions(package).await
    }

    async fn list_ref_names(&self, _package: &str) -> Result<Vec<String>> {
        no_ref_names()
    }

    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        Err(ref_resolution_unsupported(self.name(), package, reference))
    }

    async fn tags_at_commit(&self, _package: &str, _commit: &str) -> Result<TagsAtCommit> {
        tags_at_commit_unsupported()
    }

    fn name(&self) -> &'static str {
        "docker"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::AUTHORIZATION;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn authenticated_registry(
        server: &MockServer,
        auth: serde_json::Value,
    ) -> (tempfile::TempDir, DockerRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, serde_json::json!({"auths": auth}).to_string()).unwrap();
        let mut registry = DockerRegistry::with_api_url(server.uri());
        registry.credentials = Credentials::at(Some(path));
        (dir, registry)
    }

    #[tokio::test]
    async fn private_bearer_registry_uses_docker_credentials_and_pull_scope() {
        use wiremock::matchers::header;
        let server = MockServer::start().await;
        let (_dir, mut registry) = authenticated_registry(
            &server,
            serde_json::json!({
                "ghcr.io": {"auth": "dXNlcjpwYXNz"}
            }),
        );
        registry.github_credentials = Some(("fallback".into(), "unused".into()));
        Mock::given(method("GET")).and(path("/v2/team/app/tags/list"))
            .respond_with(ResponseTemplate::new(401).insert_header("www-authenticate", format!(
                "bEaReR realm=\"{}/token?scope=registry:catalog:*\",service=\"registry\",scope=\"repository:team/app:pull,push\"", server.uri())))
            .with_priority(10).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(header("authorization", "Basic dXNlcjpwYXNz"))
            .and(query_param("scope", "repository:team/app:pull"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token":"access"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/team/app/tags/list"))
            .and(header("authorization", "Bearer access"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", "</page2>; rel=\"next\"")
                    .set_body_json(serde_json::json!({"tags":["1.0"]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/page2"))
            .and(header("authorization", "Bearer access"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tags":["1.2"]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            registry
                .get_latest_version(&DockerRegistry::lookup_key("ghcr.io/team/app", "1.0"))
                .await
                .unwrap(),
            "1.2"
        );
        let requests = server.received_requests().await.unwrap();
        assert!(requests[0].headers.get(AUTHORIZATION).is_none());
        let token = requests.iter().find(|r| r.url.path() == "/token").unwrap();
        assert_eq!(
            token
                .url
                .query_pairs()
                .filter(|(k, _)| k == "scope")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn private_hub_identity_token_is_exchanged_in_post_body() {
        use wiremock::matchers::{body_string_contains, header};
        let server = MockServer::start().await;
        let (_dir, registry) = authenticated_registry(
            &server,
            serde_json::json!({
                "https://index.docker.io/v1/": {"identitytoken": "refresh-secret"}
            }),
        );
        Mock::given(path("/v2/repositories/team/app/tags"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(path("/v2/team/app/tags/list"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                format!(
                    "Bearer realm=\"{}/token\",service=\"registry.docker.io\"",
                    server.uri()
                ),
            ))
            .with_priority(10)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=refresh-secret"))
            .and(body_string_contains("scope=repository%3Ateam%2Fapp%3Apull"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"token":"access"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v2/team/app/tags/list"))
            .and(header("authorization", "Bearer access"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tags":["1.1"]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            registry
                .get_latest_version(&DockerRegistry::lookup_key("team/app", "1.0"))
                .await
                .unwrap(),
            "1.1"
        );
        for request in server.received_requests().await.unwrap() {
            assert!(!request.url.as_str().contains("refresh-secret"));
        }
    }

    #[tokio::test]
    async fn private_basic_registry_keeps_auth_on_same_origin_pages() {
        use wiremock::matchers::header;
        let server = MockServer::start().await;
        let (_dir, registry) = authenticated_registry(
            &server,
            serde_json::json!({
                "registry.example:5000": {"username":"user", "password":"pass"}
            }),
        );
        Mock::given(path("/v2/team/app/tags/list"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("www-authenticate", "Basic realm=\"private\""),
            )
            .with_priority(10)
            .mount(&server)
            .await;
        Mock::given(path("/v2/team/app/tags/list"))
            .and(header("authorization", "Basic dXNlcjpwYXNz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", "</next>; rel=\"next\"")
                    .set_body_json(serde_json::json!({"tags":["1.0"]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/next"))
            .and(header("authorization", "Basic dXNlcjpwYXNz"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tags":["1.5"]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            registry
                .get_latest_version(&DockerRegistry::lookup_key(
                    "registry.example:5000/team/app",
                    "1.0"
                ))
                .await
                .unwrap(),
            "1.5"
        );
    }

    #[test]
    fn token_url_requires_https_and_no_embedded_credentials() {
        let registry = DockerRegistry::new();
        let source = url::Url::parse("https://registry.example/v2/team/app/tags/list").unwrap();
        assert!(
            registry
                .validate_token_url(
                    &source,
                    &url::Url::parse("https://auth.example/token").unwrap()
                )
                .is_ok()
        );
        for realm in [
            "http://auth.example/token",
            "https://user:secret@auth.example/token",
            "file:///etc/passwd",
            "https://auth.example/token#fragment",
        ] {
            assert!(
                registry
                    .validate_token_url(&source, &url::Url::parse(realm).unwrap())
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn token_redirect_does_not_forward_password_or_identity_token() {
        let server = MockServer::start().await;
        let target = MockServer::start().await;
        Mock::given(path("/token"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/stolen", target.uri())),
            )
            .mount(&server)
            .await;
        let registry = DockerRegistry::with_api_url(server.uri());
        let location = DockerRegistry::image_location("registry.example/team/app").unwrap();
        for credential in [
            Credential::Basic("user".into(), "secret".into()),
            Credential::IdentityToken("secret".into()),
        ] {
            let error = registry
                .bearer_token(
                    &location,
                    &url::Url::parse(&server.uri()).unwrap(),
                    (
                        format!("{}/token", server.uri()),
                        "registry".into(),
                        "".into(),
                    ),
                    Some(&credential),
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("307"));
            assert!(!format!("{error:#}").contains("secret"));
        }
        assert!(target.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn public_registry_does_not_read_broken_docker_config() {
        let server = MockServer::start().await;
        let (dir, registry) = authenticated_registry(&server, serde_json::json!({}));
        std::fs::write(dir.path().join("config.json"), "invalid").unwrap();
        Mock::given(path("/v2/team/app/tags/list"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tags":["1.1"]})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            registry
                .get_latest_version(&DockerRegistry::lookup_key(
                    "registry.example/team/app",
                    "1.0"
                ))
                .await
                .unwrap(),
            "1.1"
        );
    }

    #[test]
    fn image_names_follow_docker_hub_shorthand_rules() {
        assert_eq!(
            DockerRegistry::image_location("alpine").unwrap(),
            ImageLocation {
                registry: "docker.io".into(),
                repository: "library/alpine".into(),
                docker_hub: true,
            }
        );
        assert_eq!(
            DockerRegistry::image_location("ghcr.io/rvben/upd").unwrap(),
            ImageLocation {
                registry: "ghcr.io".into(),
                repository: "rvben/upd".into(),
                docker_hub: false,
            }
        );
        assert_eq!(
            DockerRegistry::image_location("localhost:5000/team/app")
                .unwrap()
                .registry,
            "localhost:5000"
        );
    }

    #[test]
    fn channel_selection_preserves_suffix_prefix_and_precision() {
        let tags = [
            "1.90-alpine",
            "1.98-alpine",
            "1.99-bookworm",
            "2.0-alpine",
            "v1.99-alpine",
            "1.99.1-alpine",
        ]
        .into_iter()
        .map(|version| VersionMeta {
            version: version.into(),
            published_at: None,
            yanked: false,
            prerelease: false,
        })
        .collect();
        let selected = DockerRegistry::select_channel(tags, "1.90-alpine").unwrap();
        assert_eq!(
            selected
                .into_iter()
                .map(|meta| meta.version)
                .collect::<Vec<_>>(),
            vec!["1.90-alpine", "1.98-alpine", "2.0-alpine"]
        );
    }

    #[test]
    fn floating_tags_are_rejected_instead_of_guessed() {
        let error = DockerRegistry::select_channel(Vec::new(), "latest").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("floating or not numerically versioned")
        );
    }

    #[test]
    fn oci_pagination_never_changes_origin() {
        let current = url::Url::parse("https://registry.example/v2/acme/app/tags/list").unwrap();
        let error = DockerRegistry::next_page_url(
            &current,
            Some("<https://attacker.example/next>; rel=\"next\""),
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed origin"));
    }

    #[test]
    fn github_credentials_only_reach_the_exact_ghcr_token_endpoint() {
        let location = DockerRegistry::image_location("ghcr.io/rvben/upd").unwrap();
        let mut registry = DockerRegistry::new();
        registry.github_credentials = Some(("actor".into(), "token".into()));
        let ghcr =
            url::Url::parse("https://ghcr.io/token?scope=repository:rvben/upd:pull").unwrap();
        let request = registry.token_request(&location, ghcr).build().unwrap();
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Basic YWN0b3I6dG9rZW4="
        );
        let attacker = registry
            .token_request(
                &location,
                url::Url::parse("https://attacker.example/token").unwrap(),
            )
            .build()
            .unwrap();
        assert!(attacker.headers().get(AUTHORIZATION).is_none());
        let wrong_path = registry
            .token_request(&location, url::Url::parse("https://ghcr.io/other").unwrap())
            .build()
            .unwrap();
        assert!(wrong_path.headers().get(AUTHORIZATION).is_none());
    }

    #[tokio::test]
    async fn docker_hub_versions_keep_publish_dates_and_follow_pages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/repositories/library/alpine/tags"))
            .and(query_param("page_size", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"name": "3.22", "last_updated": "2026-01-01T00:00:00Z"}],
                "next": format!("{}/next", server.uri())
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/next"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"name": "3.24", "last_updated": "2026-08-01T00:00:00Z"}],
                "next": null
            })))
            .mount(&server)
            .await;

        let registry = DockerRegistry::with_api_url(server.uri());
        let key = DockerRegistry::lookup_key("alpine", "3.22");
        assert_eq!(registry.get_latest_version(&key).await.unwrap(), "3.24");
        let versions = registry.list_versions(&key).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert!(
            versions
                .iter()
                .all(|version| version.published_at.is_some())
        );
    }

    #[tokio::test]
    async fn docker_hub_failure_falls_back_to_the_oci_registry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/repositories/library/rust/tags"))
            .and(query_param("page_size", "100"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/library/rust/tags/list"))
            .and(query_param("n", "1000"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", "</next>; rel=\"next\"")
                    .set_body_json(serde_json::json!({
                        "name": "library/rust",
                        "tags": ["1.90-alpine", "1.91-alpine", "1.98-bookworm"]
                    })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/next"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "library/rust",
                "tags": ["1.98-alpine"]
            })))
            .mount(&server)
            .await;

        let registry = DockerRegistry::with_api_url(server.uri());
        let key = DockerRegistry::lookup_key("rust", "1.90-alpine");
        assert_eq!(
            registry.get_latest_version(&key).await.unwrap(),
            "1.98-alpine"
        );
    }
}
