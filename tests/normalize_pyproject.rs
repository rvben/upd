//! CLI contract for opt-in pyproject specifier normalization.

use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn serve_release(server: &MockServer, package: &str, version: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/simple/{package}/")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/pypi/{package}/json")))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"releases":{{"{version}":[{{"yanked":false,"upload_time_iso_8601":"2026-01-01T00:00:00Z"}}]}}}}"#
        )))
        .mount(server)
        .await;
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\ndependencies = ['click']\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "[normalize.pyproject]\ndependencies = \"exact\"\n",
    )
    .unwrap();
    dir
}

fn run(dir: &TempDir, server: &MockServer, args: &[&str]) -> std::process::Output {
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .args(args)
        .arg("pyproject.toml")
        .env("HOME", home)
        .env("PIP_CONFIG_FILE", pip_config_null_device())
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .env("UV_INDEX_URL", server.uri())
        .current_dir(dir.path())
        .output()
        .expect("upd ran")
}

#[cfg(unix)]
fn pip_config_null_device() -> &'static str {
    "/dev/null"
}

#[cfg(windows)]
fn pip_config_null_device() -> &'static str {
    "NUL"
}

#[tokio::test]
async fn dry_run_reports_a_distinct_normalization_and_exits_one() {
    let server = MockServer::start().await;
    serve_release(&server, "click", "8.2.1").await;
    let dir = fixture();
    let original = std::fs::read_to_string(dir.path().join("pyproject.toml")).unwrap();

    let output = run(
        &dir,
        &server,
        &["--dry-run", "--format", "json", "--no-cache"],
    );
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("pyproject.toml")).unwrap(),
        original
    );

    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["updates_total"], 0);
    assert_eq!(report["summary"]["normalized"], 1);
    assert_eq!(report["files"][0]["normalized"][0]["package"], "click");
    assert_eq!(
        report["files"][0]["normalized"][0]["section"],
        "project.dependencies"
    );
    assert!(report["files"][0]["normalized"][0]["previous_spec"].is_null());
    assert_eq!(report["files"][0]["normalized"][0]["new_spec"], "==8.2.1");
}

#[tokio::test]
async fn apply_writes_the_configured_shape_and_preserves_literal_quotes() {
    let server = MockServer::start().await;
    serve_release(&server, "click", "8.2.1").await;
    let dir = fixture();

    let output = run(
        &dir,
        &server,
        &["--apply", "--format", "text", "--no-cache"],
    );
    assert!(output.status.success(), "{:?}", output);
    let content = std::fs::read_to_string(dir.path().join("pyproject.toml")).unwrap();
    assert!(
        content.contains("dependencies = ['click==8.2.1']"),
        "{content}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Normalized click (no specifier) → ==8.2.1"),
        "{stdout}"
    );
}

#[tokio::test]
async fn check_treats_normalization_as_pending_without_writing() {
    let server = MockServer::start().await;
    serve_release(&server, "click", "8.2.1").await;
    let dir = fixture();
    let original = std::fs::read_to_string(dir.path().join("pyproject.toml")).unwrap();

    let output = run(
        &dir,
        &server,
        &["--check", "--format", "json", "--no-cache"],
    );
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("pyproject.toml")).unwrap(),
        original
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["normalized"], 1);
}

#[tokio::test]
async fn mixed_update_and_normalization_keep_distinct_report_channels() {
    let server = MockServer::start().await;
    serve_release(&server, "click", "8.2.1").await;
    serve_release(&server, "requests", "2.34.0").await;
    let dir = fixture();
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\ndependencies = ['click', \"requests==2.0.0\"]\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &server,
        &["--apply", "--format", "json", "--no-cache"],
    );
    assert!(output.status.success(), "{:?}", output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["updates_total"], 1);
    assert_eq!(report["summary"]["normalized"], 1);
    assert_eq!(report["files"][0]["updates"][0]["package"], "requests");
    assert_eq!(report["files"][0]["normalized"][0]["package"], "click");
    let content = std::fs::read_to_string(dir.path().join("pyproject.toml")).unwrap();
    assert!(content.contains("'click==8.2.1'"), "{content}");
    assert!(content.contains("\"requests==2.34.0\""), "{content}");
}
