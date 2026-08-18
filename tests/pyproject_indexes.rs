//! End-to-end tests for package indexes declared in pyproject.toml, driven
//! through the built binary against two mock indexes: the process default
//! (`UV_INDEX_URL`, standing in for PyPI) and a private-only index declared in
//! the manifest that carries internal packages and nothing else.
//!
//! The defect these guard: a declared `[[tool.uv.index]]` used to replace the
//! default index instead of being consulted alongside it, so every public
//! dependency in such a file resolved to HTTP 404 while the run still closed
//! with a green "all dependencies up to date".

use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Minimal PyPI JSON API body; a release needs at least one non-yanked file.
fn pypi_releases(versions: &[&str]) -> String {
    let entries: Vec<String> = versions
        .iter()
        .map(|v| {
            format!(r#""{v}":[{{"yanked":false,"upload_time_iso_8601":"2024-01-01T00:00:00Z"}}]"#)
        })
        .collect();
    format!(r#"{{"releases":{{{}}}}}"#, entries.join(","))
}

/// Serve `versions` for `package`. `PyPiRegistry` tries the Simple API first
/// and falls back to the JSON API, so both routes are mounted.
async fn serve(mock: &MockServer, package: &str, versions: &[&str]) {
    Mock::given(method("GET"))
        .and(path(format!("/simple/{package}/")))
        .respond_with(ResponseTemplate::new(404))
        .mount(mock)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/pypi/{package}/json")))
        .respond_with(ResponseTemplate::new(200).set_body_string(pypi_releases(versions)))
        .mount(mock)
        .await;
}

/// The index does not carry `package` on either route.
async fn missing(mock: &MockServer, package: &str) {
    for route in [
        format!("/simple/{package}/"),
        format!("/pypi/{package}/json"),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(404))
            .mount(mock)
            .await;
    }
}

/// The index must never be asked about `package` at all.
async fn never_asked(mock: &MockServer, package: &str) {
    for route in [
        format!("/simple/{package}/"),
        format!("/pypi/{package}/json"),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(mock)
            .await;
    }
}

/// Run the binary in `dir` with the default index pointed at `default_index`
/// and everything else isolated from the host (no pip.conf, no inherited
/// registry or credential configuration, a fixture-local cache).
fn run(dir: &TempDir, default_index: &str, args: &[&str]) -> std::process::Output {
    let home = dir.path().join("home");
    let xdg_config = dir.path().join("xdg-config");
    std::fs::create_dir_all(&home).expect("fixture HOME created");
    std::fs::create_dir_all(&xdg_config).expect("fixture XDG config created");

    Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .arg("pyproject.toml")
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg_config)
        .env("PIP_CONFIG_FILE", pip_config_null_device())
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .env("UV_INDEX_URL", default_index)
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

fn write_pyproject(dir: &TempDir, body: &str) {
    std::fs::write(dir.path().join("pyproject.toml"), body).expect("pyproject written");
}

fn json_of(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}):\n{stdout}\nstderr:\n{stderr}"))
}

/// `(package, latest)` pairs the run would apply, sorted by package.
fn planned_updates(report: &Value) -> Vec<(String, String)> {
    let mut updates: Vec<(String, String)> = report["files"]
        .as_array()
        .expect("files[] is an array")
        .iter()
        .flat_map(|f| f["updates"].as_array().cloned().unwrap_or_default())
        .map(|u| {
            (
                u["package"].as_str().unwrap().to_string(),
                u["latest"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    updates.sort();
    updates
}

fn error_count(report: &Value) -> usize {
    report["files"]
        .as_array()
        .expect("files[] is an array")
        .iter()
        .map(|f| f["errors"].as_array().map_or(0, Vec::len))
        .sum()
}

const DRY_RUN_JSON: &[&str] = &["--dry-run", "--no-cache", "--output", "json"];

/// The reported case: a private-only index declared without `default = true`
/// is consulted in addition to the default index, so the internal package
/// resolves from the private index and the public one from the default.
#[tokio::test(flavor = "multi_thread")]
async fn declared_uv_index_is_layered_over_the_default_index() {
    let default = MockServer::start().await;
    serve(&default, "requests", &["2.28.0", "2.32.0"]).await;
    missing(&default, "hda-common").await;

    let private = MockServer::start().await;
    serve(&private, "hda-common", &["1.0.908", "1.0.909"]).await;
    missing(&private, "requests").await;

    let dir = TempDir::new().unwrap();
    write_pyproject(
        &dir,
        &format!(
            r#"[project]
name = "demo"
version = "0.1.0"
dependencies = [
    "requests>=2.28.0",
    "hda-common>=1.0.908",
]

[[tool.uv.index]]
name = "nexus"
url = "{}/simple/"
publish-url = "{}/"
"#,
            private.uri(),
            private.uri()
        ),
    );

    let output = run(&dir, &default.uri(), DRY_RUN_JSON);
    let report = json_of(&output);

    assert_eq!(
        error_count(&report),
        0,
        "no lookup may fail: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        planned_updates(&report),
        vec![
            ("hda-common".to_string(), "1.0.909".to_string()),
            ("requests".to_string(), "2.32.0".to_string()),
        ]
    );
    assert_eq!(output.status.code(), Some(1), "updates available exit 1");
}

/// `default = true` is the one thing that removes the default index. The
/// public package then genuinely cannot be resolved, and the default index is
/// never contacted.
#[tokio::test(flavor = "multi_thread")]
async fn uv_default_true_replaces_the_default_index() {
    let default = MockServer::start().await;
    never_asked(&default, "requests").await;
    never_asked(&default, "hda-common").await;

    let private = MockServer::start().await;
    serve(&private, "hda-common", &["1.0.908", "1.0.909"]).await;
    missing(&private, "requests").await;

    let dir = TempDir::new().unwrap();
    write_pyproject(
        &dir,
        &format!(
            r#"[project]
name = "demo"
dependencies = [
    "requests>=2.28.0",
    "hda-common>=1.0.908",
]

[[tool.uv.index]]
name = "nexus"
url = "{}/simple/"
default = true
"#,
            private.uri()
        ),
    );

    let output = run(&dir, &default.uri(), DRY_RUN_JSON);
    let report = json_of(&output);

    assert_eq!(
        planned_updates(&report),
        vec![("hda-common".to_string(), "1.0.909".to_string())]
    );
    assert_eq!(error_count(&report), 1, "requests is not on the only index");
    assert_eq!(output.status.code(), Some(2), "a failed lookup exits 2");
}

/// An explicit index is only used for packages pinned to it via
/// `[tool.uv.sources]`. The pinned package takes the explicit index's answer
/// even though the default index carries a higher version, and an unpinned
/// package never touches the explicit index.
#[tokio::test(flavor = "multi_thread")]
async fn explicit_uv_index_serves_only_the_packages_pinned_to_it() {
    let default = MockServer::start().await;
    serve(&default, "torch", &["99.0.0"]).await;
    serve(&default, "numpy", &["1.26.0", "2.0.0"]).await;

    let private = MockServer::start().await;
    serve(&private, "torch", &["2.0.0", "2.1.0"]).await;
    never_asked(&private, "numpy").await;

    let dir = TempDir::new().unwrap();
    write_pyproject(
        &dir,
        &format!(
            r#"[project]
name = "demo"
dependencies = [
    "torch>=2.0.0",
    "numpy>=1.26.0",
]

[[tool.uv.index]]
name = "pytorch"
url = "{}/simple/"
explicit = true

[tool.uv.sources]
torch = {{ index = "pytorch" }}
"#,
            private.uri()
        ),
    );

    let output = run(&dir, &default.uri(), DRY_RUN_JSON);
    let report = json_of(&output);

    assert_eq!(error_count(&report), 0);
    assert_eq!(
        planned_updates(&report),
        vec![
            ("numpy".to_string(), "2.0.0".to_string()),
            ("torch".to_string(), "2.1.0".to_string()),
        ]
    );
}
