//! End-to-end npm cooldown regressions exercised through the public CLI.

use std::process::Command;

use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn run_npm_update(installed: &str, registry_document: &str) -> serde_json::Value {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/astro"))
        .respond_with(ResponseTemplate::new(200).set_body_string(registry_document))
        .mount(&mock)
        .await;

    let dir = TempDir::new().unwrap();
    let package_json = dir.path().join("package.json");
    std::fs::write(
        &package_json,
        format!(
            r#"{{"name":"catechize-fixture","private":true,"dependencies":{{"astro":"^{installed}"}}}}"#
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_upd"))
        .arg("--dry-run")
        .arg("--no-cache")
        .arg("--min-age")
        .arg("7d")
        .arg("--max-bump")
        .arg("minor")
        .arg("--lang")
        .arg("node")
        .arg("--output")
        .arg("json")
        .arg(&package_json)
        .env("NPM_REGISTRY", mock.uri())
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .current_dir(dir.path())
        .output()
        .expect("upd ran");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "upd failed: {stdout}\n{stderr}");
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn current_npm_dependency_is_not_reported_as_a_cooldown_skip() {
    // Mirrors the shape that exposed the Catechize regression: npm's stable
    // dist-tag is already installed, while the full metadata also contains an
    // unrelated historical prerelease. Both abbreviated and full metadata
    // requests can consume this document because serde ignores extra fields.
    let report = run_npm_update(
        "7.2.2",
        r#"{
              "name": "astro",
              "dist-tags": {"latest": "7.2.2"},
              "versions": {
                "0.0.0-data-astro-transition-20240111220209": {
                  "version": "0.0.0-data-astro-transition-20240111220209"
                },
                "7.2.2": {"version": "7.2.2"}
              },
              "time": {
                "0.0.0-data-astro-transition-20240111220209": "2024-01-11T22:02:09.000Z",
                "7.2.2": "2026-07-01T12:00:00.000Z"
              }
            }"#,
    )
    .await;
    assert_eq!(report["summary"]["updates_total"], 0, "{report}");
    assert_eq!(report["summary"]["errors"], 0, "{report}");
    assert!(
        report["files"][0].get("skipped_by_cooldown").is_none(),
        "an up-to-date dependency must be a clean no-op: {report}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_latest_behind_installed_version_never_downgrades_or_reports_cooldown() {
    let report = run_npm_update(
        "7.3.0",
        r#"{
          "name": "astro",
          "dist-tags": {"latest": "7.2.2"},
          "versions": {
            "0.0.0-data-astro-transition-20240111220209": {
              "version": "0.0.0-data-astro-transition-20240111220209"
            },
            "7.2.2": {"version": "7.2.2"},
            "7.3.0": {"version": "7.3.0"}
          },
          "time": {
            "0.0.0-data-astro-transition-20240111220209": "2024-01-11T22:02:09.000Z",
            "7.2.2": "2026-07-01T12:00:00.000Z",
            "7.3.0": "2026-08-01T12:00:00.000Z"
          }
        }"#,
    )
    .await;

    assert_eq!(report["summary"]["updates_total"], 0, "{report}");
    assert_eq!(report["summary"]["errors"], 0, "{report}");
    assert!(
        report["files"][0].get("skipped_by_cooldown").is_none(),
        "a registry lag must remain a clean no-op: {report}"
    );
}

/// A successful latest-version lookup does not make a failed date lookup safe.
/// Each blocked dependency remains unchanged and reports an error, even in check mode.
#[tokio::test(flavor = "multi_thread")]
async fn strict_publication_lookup_failures_are_per_package_errors() {
    use wiremock::matchers::header;
    let server = MockServer::start().await;
    for name in ["alpha", "beta"] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .and(header("accept", "application/vnd.npm.install-v1+json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "dist-tags": { "latest": "1.1.0" }, "versions": { "1.0.0": {}, "1.1.0": {} }
            })))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(403))
            .with_priority(2)
            .mount(&server)
            .await;
    }
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("package.json");
    let original = "{\"dependencies\":{\"alpha\":\"^1.0.0\",\"beta\":\"^1.0.0\"}}\n";
    std::fs::write(&file, original).unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "[cooldown]\ndefault = '7d'\nstrict = true\n",
    )
    .unwrap();
    for flags in [
        vec!["--apply"],
        vec!["--check"],
        vec!["--check", "--fail-on-blocked"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_upd"))
            .args(flags)
            .args(["--no-cache", "--output", "json"])
            .arg(&file)
            .env("NPM_REGISTRY", server.uri())
            .env("UPD_CACHE_DIR", dir.path().join("cache"))
            .env_remove("NPM_TOKEN")
            .env_remove("NODE_AUTH_TOKEN")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let errors = report["files"][0]["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 2, "{report}");
        let text = errors
            .to_vec()
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        for name in ["alpha", "beta"] {
            assert!(text.contains(name), "{report}");
        }
        assert!(text.contains("publication date lookup failed"), "{report}");
        assert!(
            report["files"][0].get("skipped_by_cooldown").is_none(),
            "{report}"
        );
    }
}
