//! CLI discovery, cache, filtering, pins and byte-preserving apply without network.
use serde_json::{Value, json};
use std::{
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("gradle")).unwrap();
    std::fs::create_dir(dir.path().join("cache")).unwrap();
    std::fs::write(dir.path().join("gradle/libs.versions.toml"),"[versions]\nlib = '1.0'\n[libraries]\nlib = { module = 'g:lib', version.ref = 'lib' }\nlsp = { module = 'org.eclipse.lsp4j:org.eclipse.lsp4j', version = '0.21.1' }\n").unwrap();
    std::fs::write(
        dir.path().join("settings.gradle.kts"),
        "plugins { id(\"org.example\") version \"1.0\" }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "[pin]\n\"org.eclipse.lsp4j:org.eclipse.lsp4j\" = \"0.21.1\"\n",
    )
    .unwrap();
    let fetched_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let cache = json!({"gradle":{
        "g:lib":{"version":"1.2.3","fetched_at":fetched_at},
        "gradle-plugin:org.example":{"version":"2.0.0","fetched_at":fetched_at}
    }});
    std::fs::write(dir.path().join("cache/versions.json"), cache.to_string()).unwrap();
    dir
}
fn run(dir: &TempDir, args: &[&str]) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .env("HOME", dir.path().join("home"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .current_dir(dir.path())
        .args([".", "--lang", "gradle", "--output", "json"])
        .args(args)
        .output()
        .unwrap();
    let body = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap(), body)
}
#[test]
fn gradle_discovery_cached_preview_and_apply_preserve_ide_pin() {
    let dir = fixture();
    let catalog = dir.path().join("gradle/libs.versions.toml");
    let before = std::fs::read_to_string(&catalog).unwrap();
    let (code, report) = run(&dir, &["--dry-run"]);
    assert_eq!(code, 1, "{report}");
    assert_eq!(report["summary"]["updates_total"], 2);
    assert_eq!(report["summary"]["errors"], 0);
    assert_eq!(std::fs::read_to_string(&catalog).unwrap(), before);
    let (code, report) = run(&dir, &["--apply"]);
    assert_eq!(code, 0, "{report}");
    assert_eq!(
        std::fs::read_to_string(&catalog).unwrap(),
        before.replace("lib = '1.0'", "lib = '1.2.3'")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("settings.gradle.kts")).unwrap(),
        "plugins { id(\"org.example\") version \"2.0.0\" }\n"
    );
}
#[test]
fn gradle_package_and_bump_filters_reach_both_file_types() {
    let dir = fixture();
    let (_, report) = run(&dir, &["--dry-run", "--package", "g:lib"]);
    assert_eq!(report["summary"]["updates_total"], 1, "{report}");
    let (_, report) = run(&dir, &["--dry-run", "--max-bump", "minor"]);
    assert_eq!(report["summary"]["updates_total"], 1, "{report}");
    assert!(
        report["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["capped"].as_array().is_some_and(|c| !c.is_empty())),
        "{report}"
    );
}
#[test]
fn gradle_unsupported_versions_are_errors_not_up_to_date() {
    let dir = fixture();
    std::fs::write(
        dir.path().join("gradle/libs.versions.toml"),
        "[libraries]\nlib = 'g:lib:1.+'\n",
    )
    .unwrap();
    let (code, report) = run(&dir, &["--dry-run"]);
    assert_eq!(code, 2, "{report}");
    assert_eq!(report["summary"]["errors"], 1, "{report}");
}
