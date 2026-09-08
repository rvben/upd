//! Maven Central libraries and Gradle Plugin Portal marker artifacts.
use super::{Registry, VersionMeta, get_with_retry, http_error_message};
use crate::version::gradle::{compare, is_literal, is_prerelease};
use anyhow::{Result, anyhow, bail};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct GradleRegistry {
    client: Client,
    maven_url: String,
    plugin_url: String,
}

#[derive(Deserialize)]
struct Metadata {
    versioning: Versioning,
}
#[derive(Deserialize)]
struct Versioning {
    versions: Versions,
}
#[derive(Deserialize)]
struct Versions {
    #[serde(rename = "version", default)]
    values: Vec<String>,
}

impl Default for GradleRegistry {
    fn default() -> Self {
        Self::new()
    }
}
impl GradleRegistry {
    pub fn new() -> Self {
        Self::with_urls(
            "https://repo.maven.apache.org/maven2".into(),
            "https://plugins.gradle.org/m2".into(),
        )
    }
    pub fn with_urls(maven_url: String, plugin_url: String) -> Self {
        let client = crate::http::apply(
            Client::builder()
                .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10)),
        )
        .build()
        .expect("Failed to create HTTP client");
        Self {
            client,
            maven_url,
            plugin_url,
        }
    }
    fn metadata_url(&self, package: &str) -> Result<String> {
        fn valid(s: &str) -> bool {
            !s.is_empty()
                && s.split('.').all(|p| !p.is_empty())
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        }
        let (base, group, artifact) = if let Some(id) = package.strip_prefix("gradle-plugin:") {
            (&self.plugin_url, id, format!("{id}.gradle.plugin"))
        } else {
            let (group, artifact) = package
                .split_once(':')
                .ok_or_else(|| anyhow!("expected group:artifact: {package}"))?;
            (&self.maven_url, group, artifact.to_string())
        };
        if !valid(group) || !valid(&artifact) {
            bail!("invalid Gradle coordinates: {package}");
        }
        Ok(format!(
            "{}/{}/{}/maven-metadata.xml",
            base.trim_end_matches('/'),
            group.replace('.', "/"),
            artifact
        ))
    }
    async fn latest(&self, package: &str, prereleases: bool) -> Result<String> {
        let response = get_with_retry(&self.client, &self.metadata_url(package)?).await?;
        if !response.status().is_success() {
            bail!(
                "{}",
                http_error_message(response.status(), "Gradle package", package, None)
            );
        }
        let metadata: Metadata = quick_xml::de::from_str(&response.text().await?)
            .map_err(|e| anyhow!("invalid Maven metadata for {package}: {e}"))?;
        metadata
            .versioning
            .versions
            .values
            .into_iter()
            .filter(|v| is_literal(v) && (prereleases || !is_prerelease(v)))
            .max_by(|a, b| compare(a, b))
            .ok_or_else(|| {
                anyhow!(
                    "{package} has no supported {}versions",
                    if prereleases { "" } else { "stable " }
                )
            })
    }
}

#[async_trait::async_trait]
impl Registry for GradleRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        self.latest(package, false).await
    }
    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        self.latest(package, true).await
    }
    async fn python_releases(&self, _: &str) -> Result<Vec<super::PythonRelease>> {
        bail!("registry does not expose Python metadata")
    }
    // Maven metadata's lastUpdated is not an individual release's publish date.
    async fn list_versions(&self, _: &str) -> Result<Vec<VersionMeta>> {
        super::no_version_metadata()
    }
    async fn list_ref_names(&self, _: &str) -> Result<Vec<String>> {
        super::no_ref_names()
    }
    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        Err(super::ref_resolution_unsupported(
            self.name(),
            package,
            reference,
        ))
    }
    async fn tags_at_commit(&self, _: &str, _: &str) -> Result<super::TagsAtCommit> {
        super::tags_at_commit_unsupported()
    }
    fn name(&self) -> &'static str {
        "gradle"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    #[tokio::test]
    async fn library_and_plugin_metadata_routing_and_ordering() {
        let server = MockServer::start().await;
        let xml = "<metadata><versioning><latest>99.0-SNAPSHOT</latest><versions><version>1.9</version><version>1.10</version><version>2.0-RC2</version><version>2.0-RC10</version><version>99.0-SNAPSHOT</version></versions></versioning></metadata>";
        for route in [
            "/maven/org/example/lib/maven-metadata.xml",
            "/plugins/org/example/plugin/org.example.plugin.gradle.plugin/maven-metadata.xml",
        ] {
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(ResponseTemplate::new(200).set_body_string(xml))
                .mount(&server)
                .await;
        }
        let registry = GradleRegistry::with_urls(
            format!("{}/maven", server.uri()),
            format!("{}/plugins", server.uri()),
        );
        for package in ["org.example:lib", "gradle-plugin:org.example.plugin"] {
            assert_eq!(registry.get_latest_version(package).await.unwrap(), "1.10");
            assert_eq!(
                registry
                    .get_latest_version_including_prereleases(package)
                    .await
                    .unwrap(),
                "2.0-RC10"
            );
        }
        for p in [
            "../secret:foo",
            "g:a:1",
            "gradle-plugin:../bad",
            "g:a?secret",
        ] {
            assert!(registry.metadata_url(p).is_err());
        }
    }
    #[tokio::test]
    async fn malformed_missing_and_empty_metadata_are_errors() {
        let server = MockServer::start().await;
        let registry = GradleRegistry::with_urls(server.uri(), server.uri());
        assert!(registry.get_latest_version("g:missing").await.is_err());
        for (name, body) in [
            ("bad", "<metadata>"),
            (
                "empty",
                "<metadata><versioning><versions/></versioning></metadata>",
            ),
        ] {
            Mock::given(path(format!("/g/{name}/maven-metadata.xml")))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            assert!(
                registry
                    .get_latest_version(&format!("g:{name}"))
                    .await
                    .is_err()
            );
        }
    }
}
