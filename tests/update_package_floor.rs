//! Integration coverage for `upd update --package <name>` version floors:
//! a requested name that matches no manifest occurrence but resolves via a
//! scanned lockfile is floored to the registry latest (or a config pin)
//! through the lock's own mechanism, reusing the routing/apply machinery
//! built for `audit --fix-audit`. See `.superpowers/sdd/task-9-brief.md`
//! for the full rule set this file exercises.

use std::fs;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run_with_env(
    args: &[&str],
    cwd: &std::path::Path,
    env: &[(&str, &str)],
) -> (String, String, i32) {
    let mut cmd = Command::new(upd_bin());
    cmd.args(args).current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to run upd");
    (
        String::from_utf8(output.stdout).expect("stdout not UTF-8"),
        String::from_utf8(output.stderr).expect("stderr not UTF-8"),
        output.status.code().unwrap_or(-1),
    )
}

#[cfg(unix)]
fn write_fake_tool(bin_dir: &std::path::Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = bin_dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn path_with(bin_dir: &std::path::Path) -> String {
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

const PYPROJECT_BARE: &str = "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n";

fn uv_lock_at(package: &str, version: &str) -> String {
    format!(
        "version = 1\n\n[[package]]\nname = \"{package}\"\nversion = \"{version}\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\n"
    )
}

/// Mounts `/simple/{name}/` -> 404 (forces the legacy JSON API fallback) and
/// `/pypi/{name}/json` -> a single-release `releases` body, matching
/// `tests/cooldown_e2e.rs`'s convention.
async fn mount_pypi_latest(server: &wiremock::MockServer, name: &str, version: &str) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/simple/{name}/")))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/pypi/{name}/json")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "releases": {
                    version: [{"yanked": false, "upload_time_iso_8601": "2024-01-01T00:00:00Z"}]
                }
            })),
        )
        .mount(server)
        .await;
}

/// Same as [`mount_pypi_latest`], but the legacy JSON endpoint answers 500,
/// simulating a registry outage after a lock-only name has been resolved.
async fn mount_pypi_failure(server: &wiremock::MockServer, name: &str) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/simple/{name}/")))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/pypi/{name}/json")))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .mount(server)
        .await;
}

/// Mounts `GET {registry}/{name}` with an abbreviated npm metadata document
/// (`dist-tags.latest` + a matching `versions` entry).
async fn mount_npm_latest(server: &wiremock::MockServer, name: &str, version: &str) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/{name}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": name,
                "dist-tags": { "latest": version },
                "versions": { version: { "name": name, "version": version } }
            })),
        )
        .mount(server)
        .await;
}

/// `name` is locked only as a NESTED copy under `node_modules/host/`, never
/// as a top-level `node_modules/{name}` entry. `classify()`'s npm branch
/// treats a top-level locator whose path segment matches a direct
/// dependency key as `Manifest`-owned regardless of the spec's shape (even
/// a `file:` spec), so a top-level entry here would route through
/// `route_manifest_covered` instead of the lock-only EOVERRIDE path this
/// fixture is meant to exercise. A nested locator has no top-level
/// identity, so `classify()` marks it `LockOnly` and routing reaches
/// `route_npm_lock_only`, which is what test 6 needs.
fn npm_lock_with(name: &str, version: &str) -> String {
    format!(
        r#"{{
  "name": "t", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
  "packages": {{
    "": {{ "name": "t", "version": "1.0.0" }},
    "node_modules/host": {{ "version": "1.0.0" }},
    "node_modules/host/node_modules/{name}": {{ "version": "{version}" }}
  }}
}}"#
    )
}

/// The scan root is passed explicitly as `.` (a bare `tempfile::tempdir()`
/// is not inside a git repo, so `resolve_scan_paths` needs an explicit
/// path), which makes every discovered file's JSON `path` render with a
/// leading `./`. A logical path can also legitimately appear more than once
/// in `files[]`: the normal per-file scan produces one report, and a
/// lock-only `--package` floor produces a separate one via `floor_reports`,
/// since `emit_update_json` appends them without merging by path. Collect
/// the given array field across every file whose path ends with `filename`
/// so assertions don't depend on the `./` prefix or on which of the two
/// entries happens to hold the data.
fn collect_for_path(
    files: &[serde_json::Value],
    filename: &str,
    field: &str,
) -> Vec<serde_json::Value> {
    files
        .iter()
        .filter(|f| {
            f["path"]
                .as_str()
                .map(|p| p.ends_with(filename))
                .unwrap_or(false)
        })
        .flat_map(|f| f[field].as_array().cloned().unwrap_or_default())
        .collect()
}

/// (1) A lock-only uv package with no manifest entry is floored to the
/// mocked registry latest in a dry run: exit 1, a `uv-constraint` `planned`
/// entry on `pyproject.toml`, and the manifest itself untouched.
#[tokio::test]
async fn lock_only_package_floors_from_mocked_registry_dry_run() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "lockonly", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "lockonly",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap();
    let updates = collect_for_path(files, "pyproject.toml", "updates");
    assert!(
        updates.iter().any(|u| u["package"] == "lockonly"
            && u["current"] == "0.40.0"
            && u["latest"] == "0.49.1"
            && u["method"] == "uv-constraint"
            && u["status"] == "planned"),
        "{updates:?}"
    );

    assert_eq!(json["summary"]["updates_total"], 1, "{}", json["summary"]);
    assert_eq!(
        json["summary"]["files_with_changes"], 1,
        "{}",
        json["summary"]
    );

    let pyproject = fs::read_to_string(tmp.path().join("pyproject.toml")).unwrap();
    assert_eq!(pyproject, PYPROJECT_BARE, "dry run must not write");
}

/// (2) A candidate that exceeds `--max-bump` is silently skipped: no floor
/// entry, exit 0.
#[tokio::test]
async fn max_bump_caps_floor() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "lockonly", "1.2.0").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "lockonly",
            "--max-bump",
            "minor",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap();
    let has_floor_entry = files.iter().any(|f| {
        f["updates"]
            .as_array()
            .map(|u| u.iter().any(|e| e["package"] == "lockonly"))
            .unwrap_or(false)
    });
    assert!(!has_floor_entry, "{files:?}");
    assert_eq!(json["summary"]["updates_total"], 0, "{}", json["summary"]);
}

/// (3) A requested name that DOES occur in the manifest keeps today's
/// normal update path untouched: an update entry with no `method` field,
/// and no duplicate floor report for the same file.
#[tokio::test]
async fn manifest_matched_package_keeps_normal_path() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "examplepkg", "2.0.0").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = [\"examplepkg>=1.0\"]\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("uv.lock"),
        uv_lock_at("examplepkg", "1.0.0"),
    )
    .unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "examplepkg",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap();
    let pyproject_files: Vec<_> = files
        .iter()
        .filter(|f| {
            f["path"]
                .as_str()
                .map(|p| p.ends_with("pyproject.toml"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        pyproject_files.len(),
        1,
        "manifest-matched package must not also get a floor report: {files:?}"
    );
    let updates = pyproject_files[0]["updates"].as_array().unwrap();
    let entry = updates
        .iter()
        .find(|u| u["package"] == "examplepkg")
        .unwrap_or_else(|| panic!("no examplepkg entry in {updates:?}"));
    assert!(
        entry.get("method").is_none(),
        "normal update entries must not carry a floor method: {entry:?}"
    );
}

/// (4) A lockscan discovery warning (an ancestor lock outside the scanned
/// paths) surfaces in the update JSON's top-level `warnings` and is counted
/// in `summary.warnings`, without changing the exit code.
#[tokio::test]
async fn lockscan_warnings_surface_in_update_json() {
    let tmp = tempfile::tempdir().unwrap();
    // Simulate `upd update --package anything member/` inside a git repo:
    // the workspace root (with lock + manifest) is above the scan root but
    // within the repo, mirroring
    // `member_only_scan_warns_about_ancestor_lock_within_git_root` in
    // src/lockscan/discover.rs.
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\n",
    )
    .unwrap();
    fs::write(tmp.path().join("Cargo.lock"), "version = 4\n").unwrap();
    fs::create_dir_all(tmp.path().join("member")).unwrap();
    fs::write(
        tmp.path().join("member/Cargo.toml"),
        "[package]\nname = \"m\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "anything",
            "--format",
            "json",
            "--no-cache",
            "member/",
        ],
        tmp.path(),
        &[],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let warnings = json["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap_or_default()
            .contains("outside the scanned paths")),
        "{warnings:?}"
    );
    assert!(
        json["summary"]["warnings"].as_u64().unwrap() >= 1,
        "{}",
        json["summary"]
    );
}

/// (5) `--apply` writes the uv constraint through a fake `uv` binary and
/// reports the entry as applied.
#[cfg(unix)]
#[tokio::test]
async fn apply_writes_floor_and_reports_applied() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "lockonly", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();

    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    let log = tmp.path().join("uv-invocations.log");
    write_fake_tool(
        &bin_dir,
        "uv",
        &format!(
            "#!/bin/sh\necho \"$@\" >> {}\ncat > uv.lock <<'EOF'\n{}EOF\nexit 0\n",
            log.display(),
            uv_lock_at("lockonly", "0.49.1")
        ),
    );

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "lockonly",
            "--apply",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[
            ("UV_INDEX_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let pyproject = fs::read_to_string(tmp.path().join("pyproject.toml")).unwrap();
    assert!(pyproject.contains("[tool.uv]"), "{pyproject}");
    assert!(pyproject.contains("lockonly>=0.49.1"), "{pyproject}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap();
    let updates = collect_for_path(files, "pyproject.toml", "updates");
    assert!(
        updates
            .iter()
            .any(|u| u["package"] == "lockonly" && u["status"] == "applied"),
        "{updates:?}"
    );
}

/// (5b) A registry failure while resolving a floor candidate is an update
/// error (exit 2), never a silent no-op: it surfaces as an `ErrorEntry` on
/// the affected file, mentioning the package.
#[tokio::test]
async fn registry_failure_is_an_update_error() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_failure(&server, "lockonly").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "lockonly",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap();
    let has_matching_error = files.iter().any(|f| {
        f["errors"]
            .as_array()
            .map(|errs| {
                errs.iter().any(|e| {
                    e["message"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("lockonly")
                })
            })
            .unwrap_or(false)
    });
    assert!(has_matching_error, "{files:?}");
}

/// (6) A package that IS a direct npm dependency, but under a spec upd
/// cannot bump (`file:../local`), only shows up as a lock-scanned nested
/// copy. The EOVERRIDE guard requires bumping the direct spec via a
/// companion manifest edit, which is impossible here, so routing reports
/// `unfixable` with guidance naming the key and writes NO override.
#[tokio::test]
async fn own_name_direct_with_unbumpable_spec_is_unfixable() {
    let server = wiremock::MockServer::start().await;
    mount_npm_latest(&server, "examplepkg", "9.9.9").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("package.json"),
        r#"{"name": "t", "version": "1.0.0", "dependencies": {"examplepkg": "file:../local"}}"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("package-lock.json"),
        npm_lock_with("examplepkg", "1.2.0"),
    )
    .unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "examplepkg",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[("NPM_REGISTRY", &server.uri())],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let package_json_before = fs::read_to_string(tmp.path().join("package.json")).unwrap();

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap();
    let updates = collect_for_path(files, "package.json", "updates");
    let entry = updates
        .iter()
        .find(|u| u["package"] == "examplepkg")
        .unwrap_or_else(|| panic!("no examplepkg entry in {updates:?}"));
    assert_eq!(entry["status"], "unfixable", "{entry:?}");
    let error = entry["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("direct dependency \"examplepkg\" has a spec upd cannot bump"),
        "{error}"
    );

    let package_json_after = fs::read_to_string(tmp.path().join("package.json")).unwrap();
    assert_eq!(
        package_json_before, package_json_after,
        "no override may be written for an unfixable target"
    );
    assert!(
        !package_json_after.contains("overrides"),
        "{package_json_after}"
    );

    // The entry itself stays visible in files[].updates[], but an
    // `unfixable` floor is a zero-change diagnostic: it must not inflate
    // updates_total/updates_major or files_with_changes the way a real
    // planned/applied floor does.
    assert_eq!(json["summary"]["updates_total"], 0, "{}", json["summary"]);
    assert_eq!(json["summary"]["updates_major"], 0, "{}", json["summary"]);
    assert_eq!(
        json["summary"]["files_with_changes"], 0,
        "{}",
        json["summary"]
    );
}

/// (7) A config `ignore` entry suppresses the floor entirely, before any
/// registry call: exit 0, no floor update entry, and the package reported
/// under the floor file report's `ignored[]`.
#[tokio::test]
async fn ignored_lock_only_package_gets_no_floor() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();
    fs::write(tmp.path().join(".updrc.toml"), "ignore = [\"lockonly\"]\n").unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "lockonly",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap();
    let has_floor_entry = files.iter().any(|f| {
        f["updates"]
            .as_array()
            .map(|u| u.iter().any(|e| e["package"] == "lockonly"))
            .unwrap_or(false)
    });
    assert!(!has_floor_entry, "{files:?}");

    let ignored = collect_for_path(files, "pyproject.toml", "ignored");
    assert!(
        ignored.iter().any(|e| e["package"] == "lockonly"),
        "{ignored:?}"
    );
    assert_eq!(json["summary"]["ignored"], 1, "{}", json["summary"]);
    assert_eq!(json["summary"]["updates_total"], 0, "{}", json["summary"]);
    assert_eq!(
        json["summary"]["files_with_changes"], 0,
        "{}",
        json["summary"]
    );
}

/// (8) `--no-lock` under `update --package` writes the constraint but never
/// relocks, reporting `pending_relock`. A `pending_relock` entry is still a
/// would-be change: it must count in `updates_total`/`files_with_changes`
/// the same way `planned`/`applied` do, guarding the
/// `floor_entry_counts_as_update` predicate arm that distinguishes it from
/// `already_satisfied`/`unfixable` (zero-change diagnostics, see test 6). No
/// fake `uv` binary is needed: `--no-lock` never shells out.
#[cfg(unix)]
#[tokio::test]
async fn update_no_lock_pending_relock_counts_in_summary() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "lockonly", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "lockonly",
            "--apply",
            "--no-lock",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let pyproject = fs::read_to_string(tmp.path().join("pyproject.toml")).unwrap();
    assert!(pyproject.contains("[tool.uv]"), "{pyproject}");
    assert!(pyproject.contains("lockonly>=0.49.1"), "{pyproject}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap();
    let updates = collect_for_path(files, "pyproject.toml", "updates");
    assert!(
        updates.iter().any(|u| u["package"] == "lockonly"
            && u["method"] == "uv-constraint"
            && u["status"] == "pending_relock"),
        "{updates:?}"
    );

    assert_eq!(json["summary"]["updates_total"], 1, "{}", json["summary"]);
    assert_eq!(
        json["summary"]["files_with_changes"], 1,
        "{}",
        json["summary"]
    );
}

// Rule 9 (the `--interactive` early-path note for a lock-only `--package`
// name) is covered by `interactive_lock_only_package_gets_note` in
// src/main.rs's own unit test module, not here: `run_interactive_update`'s
// TTY guard (see tests/interactive_tty.rs) unconditionally rejects non-TTY
// stdin before the rule-9 note check ever runs, so a subprocess spawned by
// an integration test can never reach it without a real pty. The detection
// helper (`is_lock_only_name`) and the note text (`lock_only_interactive_note`)
// are private to the binary crate and directly testable there.
