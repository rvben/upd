//! PEP 440 arbitrary equality (`===`) across every Python path that reads or
//! writes a version.
//!
//! `===` is one operator, not `==` followed by a stray `=`. A reader that
//! matches `==` first consumes two of the three characters and takes the
//! remaining `=` for part of the version, which makes `six === 1.16.0` parse as
//! the version `"="`. The spaced and unspaced spellings fail differently under
//! that misreading, so both are asserted here, and `==` is carried alongside as
//! the control that the operator alternation still prefers the longest match.

use serde_json::{Value, json};
use std::process::{Command, Output};
use tempfile::TempDir;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

async fn serve(server: &MockServer, name: &str, files: Value) {
    Mock::given(path(format!("/simple/{name}/")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            json!({ "files": files }).to_string(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(server)
        .await;
}

fn upd(dir: &TempDir, server: Option<&MockServer>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_upd"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env(
            "PIP_CONFIG_FILE",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .current_dir(dir.path())
        .args(args)
        .args(["--no-cache", "--output", "json", "."]);
    if let Some(server) = server {
        command.env("UV_INDEX_URL", server.uri());
    }
    command.output().unwrap()
}

fn report(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write(dir: &TempDir, name: &str, content: &str) {
    let target = dir.path().join(name);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, content).unwrap();
}

fn read(dir: &TempDir, name: &str) -> String {
    std::fs::read_to_string(dir.path().join(name)).unwrap()
}

async fn serve_six(server: &MockServer) {
    serve(
        server,
        "six",
        json!([{"filename":"six-1.16.0.tar.gz"},{"filename":"six-1.17.0.tar.gz"}]),
    )
    .await;
}

// ── update: the version an `===` pin names must be the version, not "=" ──────

/// The spaced spelling. `==` matched first leaves `=` as the whole version
/// token, which is not a PEP 440 version, so the package was dropped with a
/// warning naming `"="` as its current version.
#[tokio::test]
async fn a_spaced_arbitrary_equality_pin_is_updated_in_requirements() {
    let server = MockServer::start().await;
    serve_six(&server).await;
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "requirements.txt", "six === 1.16.0\n");

    let result = report(&upd(&dir, Some(&server), &["--apply"]), 0);
    let file = &result["files"][0];

    assert_eq!(
        file["warnings"].as_array().map(Vec::len),
        Some(0),
        "an `===` pin is readable and must not warn: {}",
        file["warnings"]
    );
    assert_eq!(file["updates"][0]["package"], "six");
    assert_eq!(
        file["updates"][0]["current"], "1.16.0",
        "the current version is what `===` names, not the third `=`"
    );
    assert_eq!(file["updates"][0]["latest"], "1.17.0");
    assert_eq!(
        read(&dir, "requirements.txt"),
        "six === 1.17.0\n",
        "the operator and its surrounding spacing must survive the rewrite"
    );
}

/// The unspaced spelling, which parses by accident today: the version token
/// swallows the third `=` and the clause reader then strips it back off. It
/// must keep working once the operator is read properly.
#[tokio::test]
async fn an_unspaced_arbitrary_equality_pin_is_updated_in_requirements() {
    let server = MockServer::start().await;
    serve_six(&server).await;
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "requirements.txt", "six===1.16.0\n");

    let result = report(&upd(&dir, Some(&server), &["--apply"]), 0);
    assert_eq!(result["files"][0]["updates"][0]["current"], "1.16.0");
    assert_eq!(read(&dir, "requirements.txt"), "six===1.17.0\n");
}

/// The control: `==` must still be read as `==`. A fix that prefers `===`
/// everywhere without anchoring it would break the ordinary exact pin.
#[tokio::test]
async fn an_ordinary_exact_pin_is_unaffected() {
    let server = MockServer::start().await;
    serve_six(&server).await;
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "requirements.txt", "six == 1.16.0\n");

    let result = report(&upd(&dir, Some(&server), &["--apply"]), 0);
    assert_eq!(result["files"][0]["updates"][0]["current"], "1.16.0");
    assert_eq!(read(&dir, "requirements.txt"), "six == 1.17.0\n");
}

// ── align: the rewrite must land on the version, never inside the operator ───
//
// Alignment needs no registry: it moves every declaration of a package to the
// highest version already declared somewhere in the tree.

/// The defect with teeth. Reading the version as `"="` made the rewrite replace
/// that `=` instead, so the file was left holding `six ==1 1.16.0`: not a
/// requirement any resolver accepts, written silently with exit 0.
#[tokio::test]
async fn align_rewrites_a_spaced_arbitrary_equality_pin_without_corrupting_it() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a/requirements.txt", "six === 1.16.0\n");
    write(&dir, "b/requirements.txt", "six==1.17.0\n");

    upd(&dir, None, &["align", "--apply"]);

    assert_eq!(read(&dir, "a/requirements.txt"), "six === 1.17.0\n");
    assert_eq!(read(&dir, "b/requirements.txt"), "six==1.17.0\n");
}

/// The unspaced spelling reads correctly but the rewrite pattern still stops at
/// `==`, so the edit found nothing to replace and the run failed with exit 2
/// having changed nothing.
#[tokio::test]
async fn align_rewrites_an_unspaced_arbitrary_equality_pin() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a/requirements.txt", "six===1.16.0\n");
    write(&dir, "b/requirements.txt", "six==1.17.0\n");

    let result = report(&upd(&dir, None, &["align", "--apply"]), 0);

    assert_eq!(result["command"], "align");
    assert_eq!(read(&dir, "a/requirements.txt"), "six===1.17.0\n");
}

/// `pyproject.toml` reads `===` correctly already; only its rewrite pattern
/// was short. The symptom there was an honest failure rather than a corrupt
/// file, which is why it needs its own case.
#[tokio::test]
async fn align_rewrites_an_arbitrary_equality_pin_in_pyproject() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "a/pyproject.toml",
        "[project]\nname = \"a\"\nversion = \"0.1.0\"\ndependencies = [\"six === 1.16.0\"]\n",
    );
    write(
        &dir,
        "b/pyproject.toml",
        "[project]\nname = \"b\"\nversion = \"0.1.0\"\ndependencies = [\"six==1.17.0\"]\n",
    );

    let result = report(&upd(&dir, None, &["align", "--apply"]), 0);

    assert_eq!(result["command"], "align");
    assert!(
        read(&dir, "a/pyproject.toml").contains("\"six === 1.17.0\""),
        "{}",
        read(&dir, "a/pyproject.toml")
    );
}
