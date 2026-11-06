//! End-to-end Deno ecosystem tests through the public CLI: discovery of
//! `deno.json`/`deno.jsonc`, `jsr:` and `npm:` resolution against a local
//! registry, comment-preserving writes, and the `--lock` delegation to
//! `deno install --lockfile-only`.

#![cfg(unix)]

// The built binary's path, provided by Cargo for integration tests.
fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE_MANIFEST: &str = include_str!("fixtures/deno/deno.jsonc");
const FIXTURE_LOCK: &str = include_str!("fixtures/deno/deno.lock");

/// The abbreviated npm document for chalk, in the shape the registry answers
/// with (extra fields are ignored, like npm_cooldown_e2e relies on).
const NPM_CHALK: &str = r#"{
  "name": "chalk",
  "dist-tags": {"latest": "5.6.2"},
  "versions": {"5.0.0": {"version": "5.0.0"}, "5.6.2": {"version": "5.6.2"}},
  "time": {
    "5.0.0": "2021-01-01T00:00:00.000Z",
    "5.6.2": "2025-01-01T00:00:00.000Z"
  }
}"#;

const JSR_PATH_META: &str = r#"{
  "scope": "std",
  "name": "path",
  "versions": {
    "1.0.0": { "createdAt": "2024-01-01T00:00:00.000000Z" },
    "1.1.6": { "createdAt": "2025-01-01T00:00:00.000000Z" }
  }
}"#;

/// A wiremock registry pair plus the environment that points a spawned `upd`
/// at them.
struct Registries {
    jsr: String,
    npm: String,
    _server: MockServer,
    _npm_server: MockServer,
}

async fn start_registries() -> Registries {
    let jsr_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/@std/path/meta.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(JSR_PATH_META))
        .mount(&jsr_server)
        .await;

    let npm_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/chalk"))
        .respond_with(ResponseTemplate::new(200).set_body_string(NPM_CHALK))
        .mount(&npm_server)
        .await;

    Registries {
        jsr: jsr_server.uri(),
        npm: npm_server.uri(),
        _server: jsr_server,
        _npm_server: npm_server,
    }
}

/// Run `upd` with the registry overrides and a private cache directory.
fn run_upd(args: &[&str], cwd: &Path, registries: &Registries) -> (String, String, i32) {
    let mut cmd = Command::new(upd_bin());
    cmd.args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .env("JSR_REGISTRY", &registries.jsr)
        .env("NPM_REGISTRY", &registries.npm)
        .env("UPD_CACHE_DIR", cwd.join("upd-cache"));
    let output = cmd.output().expect("upd ran");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    )
}

/// A copied fixture project: the manifest, the lockfile it resolved, and an
/// empty `deno.lock` scratch the test can assert on.
fn scaffold(manifest_name: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join(manifest_name);
    fs::write(&manifest, FIXTURE_MANIFEST).unwrap();
    fs::write(dir.path().join("deno.lock"), FIXTURE_LOCK).unwrap();
    (dir, manifest)
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_reports_deno_jsonc_updates_without_writing() {
    let registries = start_registries().await;
    let (dir, manifest) = scaffold("deno.jsonc");

    let (stdout, stderr, code) = run_upd(
        &[
            "--dry-run",
            "--no-cache",
            "--output",
            "json",
            manifest.to_str().unwrap(),
        ],
        dir.path(),
        &registries,
    );

    // A dry run with pending updates exits 1: that is the contract
    // `decide_exit_code` documents for --check/--dry-run runs.
    assert_eq!(code, 1, "upd failed: {stdout}\n{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = report["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["file_type"], "deno_json");
    assert_eq!(files[0]["lang"], "deno");
    let updates = files[0]["updates"].as_array().unwrap();
    let mut names: Vec<&str> = updates
        .iter()
        .map(|u| u["package"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["@std/path", "chalk"]);

    // Dry run: the manifest on disk still names the old releases.
    let after = fs::read_to_string(&manifest).unwrap();
    assert!(after.contains("jsr:@std/path@^1.0.0"));
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_rewrites_specifiers_and_preserves_jsonc_comments() {
    let registries = start_registries().await;
    let (dir, manifest) = scaffold("deno.jsonc");

    let (stdout, stderr, code) = run_upd(
        &[
            "--apply",
            "--no-cache",
            "--output",
            "json",
            manifest.to_str().unwrap(),
        ],
        dir.path(),
        &registries,
    );

    assert_eq!(code, 0, "upd failed: {stdout}\n{stderr}");
    let after = fs::read_to_string(&manifest).unwrap();
    assert!(after.contains("jsr:@std/path@^1.1.6"), "{after}");
    assert!(after.contains("npm:chalk@^5.6.2"), "{after}");
    // The comment the fixture carries survives the write.
    assert!(after.contains("out of date"), "{after}");
    // The lockfile is never hand-edited by an update.
    assert_eq!(
        fs::read_to_string(dir.path().join("deno.lock")).unwrap(),
        FIXTURE_LOCK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_refresh_delegates_to_deno_install_lockfile_only() {
    let registries = start_registries().await;
    let (dir, manifest) = scaffold("deno.jsonc");

    let bin_dir = TempDir::new().unwrap();
    let marker = dir.path().join("deno-invocations");
    fs::write(
        bin_dir.path().join("deno"),
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"deno 2.9.8 (fake)\"; exit 0; fi\necho \"$@\" >> {}\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        bin_dir.path().join("deno"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut cmd = Command::new(upd_bin());
    cmd.args([
        "--apply",
        "--lock",
        "--no-cache",
        "--output",
        "json",
        manifest.to_str().unwrap(),
    ])
    .current_dir(dir.path())
    .env("NO_COLOR", "1")
    .env("JSR_REGISTRY", &registries.jsr)
    .env("NPM_REGISTRY", &registries.npm)
    .env("UPD_CACHE_DIR", dir.path().join("upd-cache"))
    .env("PATH", &path);
    let output = cmd.output().expect("upd ran");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "upd failed: {stdout}\n{stderr}");

    // The fake deno saw the documented lockfile-only invocation.
    let invocations = fs::read_to_string(&marker).unwrap();
    assert_eq!(invocations.trim(), "install --lockfile-only");
    // And the manifest moved to the newer releases.
    let after = fs::read_to_string(&manifest).unwrap();
    assert!(after.contains("jsr:@std/path@^1.1.6"));
}

#[tokio::test(flavor = "multi_thread")]
async fn lang_filters_select_deno_files_like_any_ecosystem() {
    let registries = start_registries().await;
    let (dir, _manifest) = scaffold("deno.json");

    let (stdout, stderr, code) = run_upd(
        &[
            "--dry-run",
            "--no-cache",
            "--lang",
            "deno",
            "--output",
            "json",
            dir.path().to_str().unwrap(),
        ],
        dir.path(),
        &registries,
    );
    assert_eq!(code, 1, "upd failed: {stdout}\n{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["files"].as_array().unwrap().len(), 1);

    // Another ecosystem's selection leaves the file unread.
    let (stdout, stderr, code) = run_upd(
        &[
            "--dry-run",
            "--no-cache",
            "--lang",
            "node",
            "--output",
            "json",
            dir.path().to_str().unwrap(),
        ],
        dir.path(),
        &registries,
    );
    assert_eq!(code, 0, "upd failed: {stdout}\n{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        report["files"].as_array().unwrap().len(),
        0,
        "--lang node must not open a deno.json: {stdout}"
    );
}

/// Align reads each deno.json at the release its specifiers name and writes
/// the highest one back into the import that carries it, aliased or not, and
/// never into a `"scopes"` entry of the same key.
#[tokio::test]
async fn align_lines_deno_imports_up_across_manifests() {
    let registries = start_registries().await;
    let dir = TempDir::new().unwrap();
    let behind = r#"{
  "scopes": {
    "./vendor/": {
      "chalk": "npm:chalk@^5.0.0"
    }
  },
  "imports": {
    "chalk": "npm:chalk@^5.0.0",
    "colors": "npm:lodash@4.17.15"
  }
}
"#;
    let ahead = r#"{
  "imports": {
    "chalk": "npm:chalk@^5.6.0",
    "lodash": "npm:lodash@4.17.21"
  }
}
"#;
    for (member, manifest) in [("behind", behind), ("ahead", ahead)] {
        fs::create_dir(dir.path().join(member)).unwrap();
        fs::write(dir.path().join(member).join("deno.json"), manifest).unwrap();
    }

    let (stdout, stderr, code) = run_upd(&["align", "--apply", "."], dir.path(), &registries);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let written = fs::read_to_string(dir.path().join("behind/deno.json")).unwrap();
    assert_eq!(
        written,
        behind
            .replacen(
                "\"chalk\": \"npm:chalk@^5.0.0\",",
                "\"chalk\": \"npm:chalk@^5.6.0\",",
                1
            )
            .replace("npm:lodash@4.17.15", "npm:lodash@4.17.21"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("ahead/deno.json")).unwrap(),
        ahead
    );
}
