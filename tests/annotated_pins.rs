//! End-to-end tests for comment-annotated version pins, driven through the
//! built binary against a mock PyPI. These are the discovery tests: which
//! files are claimed, which are not, and that a claimed file round-trips.

use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Minimal PyPI JSON API body. Every release needs at least one file entry:
/// the parser treats a release whose file list is empty as fully yanked
/// (`files.iter().all(|f| f.yanked)`, `src/registry/pypi.rs:473`) and drops it.
fn pypi_releases(versions: &[&str]) -> String {
    let entries: Vec<String> = versions
        .iter()
        .map(|v| {
            format!(r#""{v}":[{{"yanked":false,"upload_time_iso_8601":"2024-01-01T00:00:00Z"}}]"#)
        })
        .collect();
    format!(r#"{{"releases":{{{}}}}}"#, entries.join(","))
}

async fn mount_pypi(mock: &MockServer, package: &str, versions: &[&str]) {
    Mock::given(method("GET"))
        .and(path(format!("/pypi/{package}/json")))
        .respond_with(ResponseTemplate::new(200).set_body_string(pypi_releases(versions)))
        .mount(mock)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/simple/{package}/")))
        .respond_with(ResponseTemplate::new(404))
        .mount(mock)
        .await;
}

/// Run the binary in `dir` with a mock PyPI and an isolated cache.
fn run(dir: &TempDir, index_url: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_upd"))
        .args(args)
        .env("UV_INDEX_URL", index_url)
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .current_dir(dir.path())
        .output()
        .expect("upd ran")
}

fn json_of(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}):\n{stdout}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_annotated_makefile_is_updated_end_to_end() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(
        &makefile,
        "BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n\nbuild:\n\techo 2.6.1\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "upd failed:\n{stderr}");

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "BAO_VERSION ?= 2.7.0  # upd: pypi openbao-cli\n\nbuild:\n\techo 2.6.1\n",
        "only the annotated line is rewritten"
    );

    let report = json_of(&output);
    assert_eq!(report["summary"]["files_scanned"], 1);
    assert_eq!(report["summary"]["updates_total"], 1);
    assert_eq!(report["files"][0]["file_type"], "annotated");
    assert_eq!(report["files"][0]["lang"], "annotated");
    assert_eq!(report["files"][0]["updates"][0]["package"], "openbao-cli");
    assert_eq!(report["files"][0]["updates"][0]["source"], "pypi");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_makefile_is_claimed_at_the_root_and_in_a_subdirectory() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let line = "BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n";
    std::fs::write(dir.path().join("Makefile"), line).unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("common.mk"), line).unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(output.status.success());

    let report = json_of(&output);
    assert_eq!(
        report["summary"]["files_scanned"], 2,
        "the match target is the file name, not the whole path: {report}"
    );
    assert_eq!(report["summary"]["updates_total"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_walk_skips_markdown_but_an_explicit_path_reaches_it() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let readme = dir.path().join("README.md");
    let line = "    BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n";
    std::fs::write(&readme, line).unwrap();

    let walked = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert_eq!(
        std::fs::read_to_string(&readme).unwrap(),
        line,
        "a directory walk must never open Markdown: {}",
        String::from_utf8_lossy(&walked.stdout)
    );

    let named = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "README.md"],
    );
    assert!(named.status.success());
    assert_eq!(
        std::fs::read_to_string(&readme).unwrap(),
        "    BAO_VERSION ?= 2.7.0  # upd: pypi openbao-cli\n",
        "naming the file explicitly bypasses the pattern set"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_file_with_no_annotation_is_scanned_and_reports_nothing() {
    let mock = MockServer::start().await;

    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("some-notes.txt"), "nothing to see here\n").unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--output", "json", "some-notes.txt"],
    );
    assert!(output.status.success());

    let report = json_of(&output);
    assert_eq!(
        report["summary"]["files_scanned"], 1,
        "the file was read and had no annotation, which is the honest report"
    );
    assert_eq!(report["summary"]["updates_total"], 0);
    assert_eq!(report["summary"]["errors"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_utf8_explicit_file_is_an_error_not_a_panic() {
    let mock = MockServer::start().await;

    let dir = TempDir::new().unwrap();
    let blob = dir.path().join("blob.bin");
    std::fs::write(&blob, [0xff_u8, 0xfe, 0x00, 0x42]).unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--output", "json", "blob.bin"],
    );

    assert_ne!(
        output.status.code(),
        Some(101),
        "a Rust panic exits 101; this must take the read_file_safe error path"
    );
    let report = json_of(&output);
    assert_eq!(report["summary"]["files_scanned"], 1);
    assert_eq!(
        report["summary"]["errors"], 1,
        "an unreadable file is an error entry, not a silent skip: {report}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lang_python_selects_the_pypi_pin_and_skips_the_github_pin() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(
        &makefile,
        "BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n\
         BUN_VERSION ?= v1.1.0  # upd: github-releases oven-sh/bun\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &[
            "update", "--apply", "--lang", "python", "--output", "json", ".",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "upd failed:\n{stderr}");

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "BAO_VERSION ?= 2.7.0  # upd: pypi openbao-cli\n\
         BUN_VERSION ?= v1.1.0  # upd: github-releases oven-sh/bun\n"
    );

    let report = json_of(&output);
    assert_eq!(report["files"][0]["updates"].as_array().unwrap().len(), 1);
    assert_eq!(
        report["files"][0]["errors"].as_array().unwrap().len(),
        0,
        "the github-releases line must be dropped before its lookup, not after \
         it fails: an error entry here means the langs gate runs too late: {report}"
    );
}
