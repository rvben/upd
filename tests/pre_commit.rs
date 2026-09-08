//! CLI coverage for both pre-commit formats, using only local registry responses.
use serde_json::{Value, json};
use std::process::{Command, Output};
use tempfile::TempDir;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

fn run(dir: &TempDir, server: &MockServer, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("UV_INDEX_URL", server.uri())
        .env(
            "PIP_CONFIG_FILE",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .current_dir(dir.path())
        .args(args)
        .args(["--no-cache", "--output", "json", "."])
        .output()
        .unwrap()
}

fn report(output: Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn aliases_discover_both_formats_and_check_apply_and_filters_agree() {
    let server = MockServer::start().await;
    Mock::given(path("/simple/demo/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            json!({"files":[{"filename":"demo-0.0.2.tar.gz"}]}).to_string(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let files = [
        (
            ".pre-commit-config.yaml",
            "repos:\n- repo: local\n  hooks:\n  - id: demo\n    language: python\n    additional_dependencies: ['demo==0.0.1'] # keep\n",
        ),
        (
            "prek.toml",
            "[[repos]]\nrepo = 'local'\n[[repos.hooks]]\nid = 'demo'\nlanguage = 'python'\nadditional_dependencies = ['demo==0.0.1'] # keep\n",
        ),
    ];
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    for alias in ["pre-commit", "prek"] {
        let json = report(
            run(
                &dir,
                &server,
                &[
                    "--lang",
                    alias,
                    "--check",
                    "--package",
                    "demo",
                    "--max-bump",
                    "patch",
                ],
            ),
            1,
        );
        assert_eq!(json["summary"]["files_scanned"], 2);
        assert_eq!(json["summary"]["updates_total"], 2);
        assert_eq!(json["summary"]["updates_patch"], 2);
        assert!(
            json["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["updates"][0]["source"] == "pypi")
        );
        for (name, body) in files {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                body
            );
        }
    }
    let json = report(
        run(
            &dir,
            &server,
            &["--lang", "prek", "--apply", "--package", "demo"],
        ),
        0,
    );
    assert_eq!(json["summary"]["updates_total"], 2);
    for (name, body) in files {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(name)).unwrap(),
            body.replace("0.0.1", "0.0.2")
        );
    }
    let json = report(
        run(
            &dir,
            &server,
            &["--lang", "prek", "--check", "--package", "demo"],
        ),
        0,
    );
    assert_eq!(json["summary"]["updates_total"], 0);
}

#[test]
fn explicit_toml_path_can_apply_revision_pins_without_a_network_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let content =
        "[[repos]]\nrepo = 'https://github.com/owner/repo'\nrev = 'v1.0.0' # keep\nhooks = []\n";
    std::fs::write(dir.path().join("prek.toml"), content).unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "[pin]\n'owner/repo' = 'v2.0.0'\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .env("HOME", dir.path())
        .current_dir(dir.path())
        .args(["--apply", "--lang", "prek", "--output", "json", "prek.toml"])
        .output()
        .unwrap();
    let json = report(output, 0);
    assert_eq!(json["files"][0]["pinned"][0]["pinned_to"], "v2.0.0");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("prek.toml")).unwrap(),
        content.replace("v1.0.0", "v2.0.0")
    );
}
