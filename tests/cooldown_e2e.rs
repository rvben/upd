//! End-to-end cooldown test: run the binary against a fixture with
//! `.updrc.toml` configured for a 7-day cooldown, using a mock PyPI.

use std::process::Command;

mod common;

use common::upd_bin;

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

    let output = Command::new(upd_bin())
        .arg("--apply")
        .arg("--output")
        .arg("text")
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

/// A `[cooldown.ecosystem]` key naming a language outranks the key naming the
/// registry that answers for it. Here the registry key switches cooldown off
/// and the language key turns it back on, so nothing but the language key can
/// produce a hold-back.
#[tokio::test(flavor = "multi_thread")]
async fn a_language_key_outranks_the_registry_key_end_to_end() {
    let fixture = language_key_fixture().await;
    let output = fixture.run();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "upd failed: {stdout}\n{stderr}");
    let contents = std::fs::read_to_string(fixture.requirements()).unwrap();
    assert!(
        contents.contains("2.30.0") && !contents.contains("2.31.0"),
        "the python key should have held 2.31.0 back to 2.30.0; got:\n{contents}"
    );
}

/// The window named in the report is the window the decision used. The report
/// resolves the policy on its own path, so a language key that reaches only one
/// of the two would print a cooldown no version was ever measured against.
#[tokio::test(flavor = "multi_thread")]
async fn the_reported_window_is_the_language_keys_window() {
    let fixture = language_key_fixture().await;
    let output = fixture.run();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cooldown 7d"),
        "expected the python key's 7-day window in the held-back line; got:\n{stdout}"
    );
}

/// Interactive mode runs its own scan, so the language key has to reach that
/// one too. The registry key switches cooldown off, which leaves nothing but
/// the language key able to keep the three-day-old 2.31.0 out of the offer.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_interactive_session_reads_the_language_key() {
    let fixture = language_key_fixture().await;
    let dir = fixture.dir.path();
    let cache = dir.join("cache");
    let (_code, output) = common::run_on_a_terminal(
        &["--interactive", dir.to_str().expect("non-UTF-8 path")],
        dir,
        &[
            ("UV_INDEX_URL", fixture.index_url.as_str()),
            ("UPD_CACHE_DIR", cache.to_str().expect("non-UTF-8 path")),
        ],
        // Decline the offer: what is offered is the whole question here.
        "n\n",
    );

    assert!(
        output.contains("2.30.0") && !output.contains("2.31.0"),
        "the python key should have held the session's offer at 2.30.0; got:\n{output}"
    );
}

struct LanguageKeyFixture {
    dir: TempDir,
    _mock: MockServer,
    index_url: String,
}

impl LanguageKeyFixture {
    fn requirements(&self) -> std::path::PathBuf {
        self.dir.path().join("requirements.txt")
    }

    fn run(&self) -> std::process::Output {
        Command::new(upd_bin())
            .arg("--apply")
            .arg("--output")
            .arg("text")
            .arg(self.requirements())
            .env("UV_INDEX_URL", &self.index_url)
            .env("UPD_CACHE_DIR", self.dir.path().join("cache"))
            .current_dir(self.dir.path())
            .output()
            .expect("upd ran")
    }
}

async fn language_key_fixture() -> LanguageKeyFixture {
    let mock = MockServer::start().await;
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
    std::fs::write(dir.path().join("requirements.txt"), "requests==2.28.0\n").unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        r#"
[cooldown.ecosystem]
pypi = "0d"
python = "7d"
"#,
    )
    .unwrap();

    let index_url = mock.uri();
    LanguageKeyFixture {
        dir,
        _mock: mock,
        index_url,
    }
}
