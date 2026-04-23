//! End-to-end cooldown test: run the binary against a fixture with
//! `.updrc.toml` configured for a 7-day cooldown, using a mock PyPI.

use std::process::Command;

use chrono::{Duration, Utc};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "multi_thread")]
async fn cooldown_holds_back_fresh_versions_end_to_end() {
    let mock = MockServer::start().await;

    // Dates are computed relative to now so the test stays stable over time.
    // Under a 7-day cooldown, 2.31.0 (3d old) must be held back to 2.30.0 (32d old).
    let now = Utc::now();
    let fresh = (now - Duration::days(3)).to_rfc3339();
    let safe = (now - Duration::days(32)).to_rfc3339();
    let body = format!(
        r#"{{"releases":{{
            "2.31.0":[{{"yanked":false,"upload_time_iso_8601":"{fresh}"}}],
            "2.30.0":[{{"yanked":false,"upload_time_iso_8601":"{safe}"}}]
        }}}}"#,
    );

    Mock::given(method("GET"))
        .and(path("/pypi/requests/json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
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
    let cache_dir = dir.path().join("cache");

    let output = Command::new(env!("CARGO_BIN_EXE_upd"))
        .arg("--apply")
        .arg(&req)
        .env("UV_INDEX_URL", mock.uri())
        .env("UPD_CACHE_DIR", &cache_dir)
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
