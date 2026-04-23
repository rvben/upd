//! End-to-end cooldown test: run the binary against a fixture with
//! `.updrc.toml` configured for a 7-day cooldown, using a mock PyPI.

use std::process::Command;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "multi_thread")]
async fn cooldown_holds_back_fresh_versions_end_to_end() {
    let mock = MockServer::start().await;

    // Latest (2.31.0) is 3 days old; 2.30.0 is 32 days old and safe.
    // Under a 7-day cooldown, 2.31.0 is in the window and must be held back to 2.30.0.
    Mock::given(method("GET"))
        .and(path("/pypi/requests/json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"releases":{
                "2.31.0":[{"yanked":false,"upload_time_iso_8601":"2026-04-20T12:00:00Z"}],
                "2.30.0":[{"yanked":false,"upload_time_iso_8601":"2026-03-22T12:00:00Z"}]
            }}"#,
        ))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/requests/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock)
        .await;

    let dir = TempDir::new().unwrap();
    let req = dir.path().join("requirements.txt");
    std::fs::write(&req, "requests==2.28.0\n").unwrap();
    let rc = dir.path().join(".updrc.toml");
    std::fs::write(
        &rc,
        r#"
[cooldown]
default = "7d"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_upd"))
        .arg("--apply")
        .arg(&req)
        .env("UV_INDEX_URL", mock.uri())
        .current_dir(dir.path())
        .output()
        .expect("upd ran");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "upd failed: {stdout}\n{stderr}");
    assert!(
        stdout.contains("Held back"),
        "expected 'Held back' in output, got:\n{stdout}"
    );
    let contents = std::fs::read_to_string(&req).unwrap();
    assert!(
        contents.contains("2.30.0"),
        "file should pin the safer 2.30.0; got:\n{contents}"
    );
    assert!(
        !contents.contains("2.31.0"),
        "file must NOT be on 2.31.0; got:\n{contents}"
    );
}
