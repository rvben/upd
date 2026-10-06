//! Real HTTP fixtures for exact-release scaffolding and shared checksum rules.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use upd::annotation_tools::{ScaffoldRequest, SnippetSyntax};
use upd::registry::GitHubReleasesRegistry;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const URL: &str = "https://github.com/acme/tool/releases/download/v1.2.3/tool-v1.2.3.tar.gz";
const ASSET: &str = "tool-v1.2.3.tar.gz";
const META: &str = "/repos/acme/tool/releases/tags/v1.2.3";

fn asset(name: &str, digest: Option<&str>, state: &str, url: String) -> Value {
    json!({"name": name, "digest": digest, "state": state, "url": url})
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn request(syntax: SnippetSyntax, manifest: Option<&str>) -> ScaffoldRequest {
    ScaffoldRequest::new(
        URL,
        Some("CUSTOM"),
        syntax,
        manifest,
        Some("tool-{tag}.tar.gz"),
    )
    .unwrap()
}

#[tokio::test]
async fn a_manifest_cannot_name_the_binary_and_is_refused_before_http() {
    let server = MockServer::start().await;
    let error = ScaffoldRequest::new(
        URL,
        None,
        SnippetSyntax::Shell,
        Some("tool-{tag}.tar.gz"),
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("different release asset"));
    let registry = GitHubReleasesRegistry::with_api_url_and_token(server.uri(), None);
    let error = upd::updater::resolve_release_checksum(
        &registry,
        "acme/tool",
        "v1.2.3",
        ASSET,
        Some(ASSET),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("different release asset"));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn resolves_only_exact_metadata_and_explicit_manifests_across_all_syntaxes() {
    for syntax in [
        SnippetSyntax::Shell,
        SnippetSyntax::Docker,
        SnippetSyntax::Toml,
        SnippetSyntax::Yaml,
        SnippetSyntax::Javascript,
    ] {
        for mode in ["digest", "gnu", "bsd", "sidecar", "legacy"] {
            let server = MockServer::start().await;
            let sha = "b".repeat(64);
            let manifest = if mode == "sidecar" {
                "tool-{tag}.tar.gz.sha256"
            } else {
                "checksums-{tag}.txt"
            };
            let manifest_name = if mode == "sidecar" {
                format!("{ASSET}.sha256")
            } else {
                "checksums-v1.2.3.txt".into()
            };
            let body = match mode {
                "bsd" => format!("SHA256 ({ASSET}) = {}\n", sha.to_ascii_uppercase()),
                "sidecar" => format!("{sha}\n"),
                _ => format!("{}  unrelated.tar.gz\n{sha} *./{ASSET}\n", "a".repeat(64)),
            };
            let archive_digest = format!("sha256:{}", sha.to_ascii_uppercase());
            let body_digest = format!("sha256:{}", hash(body.as_bytes()));
            Mock::given(method("GET")).and(path(META)).respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"assets": [
                    asset(ASSET, (mode != "legacy").then_some(archive_digest.as_str()), "uploaded", format!("{}/binary", server.uri())),
                    asset(&manifest_name, Some(&body_digest), "uploaded", format!("{}/manifest", server.uri()))
                ]}))).expect(1).mount(&server).await;
            Mock::given(method("GET"))
                .and(path("/manifest"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.as_bytes()))
                .expect(if mode == "digest" { 0 } else { 1 })
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/binary"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
            let registry = GitHubReleasesRegistry::with_api_url_and_token(server.uri(), None);
            let scaffold = request(syntax, (mode != "digest").then_some(manifest))
                .resolve(&registry)
                .await
                .unwrap();
            assert_eq!(scaffold.checksum, sha);
            assert_eq!(scaffold.tag, "v1.2.3");
            assert_eq!(scaffold.asset_template, "tool-{tag}.tar.gz");
            assert_eq!(
                scaffold.checksum_source,
                if mode == "digest" {
                    "github-asset-digest"
                } else {
                    &manifest_name
                }
            );
            let report = serde_json::to_value(&scaffold).unwrap();
            assert_eq!(report["checksum_source"], scaffold.checksum_source);
            let file_type = if matches!(syntax, SnippetSyntax::Docker) {
                upd::FileType::Dockerfile
            } else {
                upd::FileType::Annotated
            };
            let validation = upd::updater::validate_annotations(&scaffold.snippet, file_type);
            assert!(validation.diagnostics.is_empty());
            assert_eq!((validation.versions, validation.checksums), (1, 1));
            assert!(scaffold.snippet.contains("CUSTOM_VERSION"));
            let requests = server.received_requests().await.unwrap();
            assert!(
                requests
                    .iter()
                    .all(|request| matches!(request.url.path(), META | "/manifest"))
            );
            assert!(requests.iter().all(|request| {
                request
                    .headers
                    .get("user-agent")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("upd/")
            }));
            server.verify().await;
        }
    }
}

#[tokio::test]
async fn refuses_missing_ambiguous_unuploaded_and_invalid_digest_assets_without_downloads() {
    for (mode, reason) in [
        ("missing", "exactly one asset"),
        ("duplicate", "exactly one asset"),
        ("uploading", "not fully uploaded"),
        ("algorithm", "unsupported digest algorithm"),
        ("malformed", "invalid SHA-256"),
        ("no-digest", "--checksums"),
        ("unauthorized", "HTTP 401"),
    ] {
        let server = MockServer::start().await;
        let digest = match mode {
            "algorithm" => "sha512:abc".into(),
            "malformed" => "sha256:abc".into(),
            _ => format!("sha256:{}", "b".repeat(64)),
        };
        let archive = asset(
            ASSET,
            (mode != "no-digest").then_some(digest.as_str()),
            if mode == "uploading" {
                "new"
            } else {
                "uploaded"
            },
            format!("{}/binary", server.uri()),
        );
        let assets = match mode {
            "missing" => vec![],
            "duplicate" => vec![archive.clone(), archive],
            _ => vec![archive],
        };
        Mock::given(method("GET"))
            .and(path(META))
            .respond_with(if mode == "unauthorized" {
                ResponseTemplate::new(401)
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"assets": assets}))
            })
            .expect(1)
            .mount(&server)
            .await;
        let registry = GitHubReleasesRegistry::with_api_url_and_token(server.uri(), None);
        // Manifest mode must verify the archive before downloading anything.
        let error = request(
            SnippetSyntax::Docker,
            (mode != "no-digest").then_some("sums.txt"),
        )
        .resolve(&registry)
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{mode}: {error:#}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        server.verify().await;
    }
}

#[tokio::test]
async fn refuses_untrustworthy_manifests_without_fallback_or_binary_downloads() {
    for (mode, reason, downloads) in [
        ("missing-manifest", "exactly one asset", 0),
        ("duplicate-manifest", "exactly one asset", 0),
        ("uploading-manifest", "not fully uploaded", 0),
        ("download-fails", "HTTP 404", 1),
        ("body-mismatch", "SHA-256 mismatch", 1),
        ("archive-conflict", "disagrees with GitHub", 1),
        ("duplicate-entry", "exactly one checksum entry", 1),
        ("wrong-entry", "exactly one checksum entry", 1),
        ("bare-digest", "exactly one checksum entry", 1),
        ("invalid-utf8", "not UTF-8", 1),
        ("oversized", "exceeds 2 MiB", 1),
    ] {
        let server = MockServer::start().await;
        let sha = "b".repeat(64);
        let entry = format!("{sha}  {ASSET}\n");
        let body = match mode {
            "duplicate-entry" => format!("{entry}{entry}").into_bytes(),
            "wrong-entry" => format!("{sha}  wrong.tar.gz\n").into_bytes(),
            "bare-digest" => sha.as_bytes().to_vec(),
            "invalid-utf8" => vec![0xff],
            "oversized" => vec![b'x'; 2 * 1024 * 1024 + 1],
            _ => entry.into_bytes(),
        };
        let body_digest = format!(
            "sha256:{}",
            if mode == "body-mismatch" {
                "a".repeat(64)
            } else {
                hash(&body)
            }
        );
        let manifest = asset(
            "sums.txt",
            Some(&body_digest),
            if mode == "uploading-manifest" {
                "new"
            } else {
                "uploaded"
            },
            format!("{}/manifest", server.uri()),
        );
        let archive_digest = format!(
            "sha256:{}",
            if mode == "archive-conflict" {
                "a".repeat(64)
            } else {
                sha.clone()
            }
        );
        let mut assets = vec![asset(
            ASSET,
            Some(&archive_digest),
            "uploaded",
            format!("{}/binary", server.uri()),
        )];
        if mode != "missing-manifest" {
            assets.push(manifest.clone());
        }
        if mode == "duplicate-manifest" {
            assets.push(manifest);
        }
        Mock::given(method("GET"))
            .and(path(META))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"assets": assets})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/manifest"))
            .respond_with(if mode == "download-fails" {
                ResponseTemplate::new(404)
            } else {
                ResponseTemplate::new(200).set_body_bytes(body)
            })
            .expect(downloads)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/binary"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let registry = GitHubReleasesRegistry::with_api_url_and_token(server.uri(), None);
        let error = request(SnippetSyntax::Shell, Some("sums.txt"))
            .resolve(&registry)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{mode}: {error:#}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1 + downloads as usize
        );
        server.verify().await;
    }
}
