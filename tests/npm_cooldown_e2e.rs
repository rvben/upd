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
