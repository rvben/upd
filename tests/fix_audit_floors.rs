//! Integration coverage for the routing-based `--fix-audit` fix phase:
//! uv-constraint / npm-override / cargo-precise version floors, implied
//! `--lock` (and its `--no-lock` opt-out), transactional group rollback, and
//! the structured `fixes[]` JSON report.

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

/// Mounts a single-package OSV batch response (one query, one vuln id) plus
/// the matching GET detail. Only valid for a fixture with exactly ONE audit
/// package, since the batch response is a fixed one-element array.
async fn mount_osv_single(
    server: &wiremock::MockServer,
    id: &str,
    name: &str,
    ecosystem: &str,
    fixed: &str,
) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [ { "vulns": [ { "id": id } ] } ]
            })),
        )
        .mount(server)
        .await;
    mount_vuln_get(server, id, name, ecosystem, fixed).await;
}

/// Mounts only the `GET /vulns/{id}` detail (single-package `affected`
/// shape), for scenarios that build their own POST responder.
async fn mount_vuln_get(
    server: &wiremock::MockServer,
    id: &str,
    name: &str,
    ecosystem: &str,
    fixed: &str,
) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/vulns/{id}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id,
                "summary": "test vulnerability",
                "affected": [{
                    "package": { "name": name, "ecosystem": ecosystem },
                    "ranges": [{ "type": "ECOSYSTEM",
                        "events": [{ "introduced": "0" }, { "fixed": fixed }] }]
                }]
            })),
        )
        .mount(server)
        .await;
}

/// A single-package OSV batch response whose GET detail carries no
/// `affected` field at all, so `fixed_version_for` yields `None` (the "no
/// fixed version" case).
async fn mount_osv_single_no_fix(server: &wiremock::MockServer, id: &str) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [ { "vulns": [ { "id": id } ] } ]
            })),
        )
        .mount(server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/vulns/{id}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id,
                "summary": "test vulnerability with no fix",
                "database_specific": { "severity": "CRITICAL" }
            })),
        )
        .mount(server)
        .await;
}

/// Responds to `/querybatch` by inspecting the request body's `queries`
/// array and answering each `(name, version)` independently. `scan_packages`
/// groups packages in a `HashMap`, so a fixture with more than one audit
/// package cannot rely on a fixed-order results array: this mirrors
/// `audit_lockscan.rs`'s `DupcrateResponder`, generalized to an arbitrary
/// answer table so several multi-package tests can share it. Unmatched
/// queries answer with no vulnerabilities.
struct MultiOsvResponder {
    answers: Vec<(&'static str, &'static str, &'static str)>,
}

impl wiremock::Respond for MultiOsvResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let queries = body["queries"].as_array().expect("queries array");
        let results: Vec<serde_json::Value> = queries
            .iter()
            .map(|q| {
                let name = q["package"]["name"].as_str().unwrap_or_default();
                let version = q["version"].as_str().unwrap_or_default();
                match self
                    .answers
                    .iter()
                    .find(|(n, v, _)| *n == name && *v == version)
                {
                    Some((_, _, id)) => serde_json::json!({ "vulns": [ { "id": id } ] }),
                    None => serde_json::json!({ "vulns": [] }),
                }
            })
            .collect();
        wiremock::ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({ "results": results }))
    }
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

/// (a) A lock-only uv package with no manifest entry gets its floor written
/// to `[tool.uv].constraint-dependencies` and the lock regenerated.
#[cfg(unix)]
#[tokio::test]
async fn uv_lock_only_floor_applies_and_relocks() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-a", "lockonly", "PyPI", "0.49.1").await;

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
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let pyproject = fs::read_to_string(tmp.path().join("pyproject.toml")).unwrap();
    assert!(pyproject.contains("[tool.uv]"), "{pyproject}");
    assert!(pyproject.contains("lockonly>=0.49.1"), "{pyproject}");

    let log_content = fs::read_to_string(&log).unwrap();
    assert!(log_content.contains("lock"), "{log_content}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    assert!(
        fixes.iter().any(|f| f["package"] == "lockonly"
            && f["method"] == "uv-constraint"
            && f["status"] == "applied"
            && f["from_version"] == "0.40.0"
            && f["to_version"] == "0.49.1"),
        "{fixes:?}"
    );
}

/// (b) When the relock fails after the floor is written, the group rolls
/// back byte-for-byte: the constraint write is undone along with whatever
/// the (failing) relock attempt touched.
#[cfg(unix)]
#[tokio::test]
async fn relock_failure_rolls_back_group_byte_for_byte() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-b", "lockonly", "PyPI", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    let pyproject_path = tmp.path().join("pyproject.toml");
    let uv_lock_path = tmp.path().join("uv.lock");
    fs::write(&pyproject_path, PYPROJECT_BARE).unwrap();
    fs::write(&uv_lock_path, uv_lock_at("lockonly", "0.40.0")).unwrap();
    let pyproject_before = fs::read(&pyproject_path).unwrap();
    let uv_lock_before = fs::read(&uv_lock_path).unwrap();

    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    write_fake_tool(
        &bin_dir,
        "uv",
        "#!/bin/sh\necho 'corrupted' >> uv.lock\necho \"error: lockonly>=0.49.1 is unsatisfiable\" >&2\nexit 1\n",
    );

    let (stdout, stderr, code) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read(&pyproject_path).unwrap(),
        pyproject_before,
        "pyproject.toml must be restored byte-for-byte"
    );
    assert_eq!(
        fs::read(&uv_lock_path).unwrap(),
        uv_lock_before,
        "uv.lock must be restored byte-for-byte"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    let entry = fixes.iter().find(|f| f["package"] == "lockonly").unwrap();
    assert_eq!(entry["status"], "rolled_back");
    let error = entry["error"].as_str().unwrap();
    assert!(error.contains("unsatisfiable"), "{error}");
    assert!(error.contains("direct dependency"), "{error}");
}

/// (c) `--no-lock` writes floors but never invokes a relock. Uses "poison
/// pill" fake tools (touch a marker and exit 1 if ever invoked) rather than
/// checking stderr text: if the code has a bug and shells out to a real
/// installed uv/cargo despite `--no-lock`, this must fail loudly rather than
/// silently pass by relying on absence of a log line.
#[cfg(unix)]
#[tokio::test]
async fn no_lock_reports_pending_relock_and_skipped() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(MultiOsvResponder {
            answers: vec![
                ("lockonly", "0.40.0", "GHSA-floors-c-uv"),
                ("dupcrate", "1.2.3", "GHSA-floors-c-cargo"),
            ],
        })
        .mount(&server)
        .await;
    mount_vuln_get(&server, "GHSA-floors-c-uv", "lockonly", "PyPI", "0.49.1").await;
    mount_vuln_get(
        &server,
        "GHSA-floors-c-cargo",
        "dupcrate",
        "crates.io",
        "2.0.1",
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let cargo_lock_content = "version = 4\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"dupcrate\"\nversion = \"1.2.3\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    fs::write(tmp.path().join("Cargo.lock"), cargo_lock_content).unwrap();

    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    let uv_marker = tmp.path().join("uv-invoked.marker");
    let cargo_marker = tmp.path().join("cargo-invoked.marker");
    write_fake_tool(
        &bin_dir,
        "uv",
        &format!("#!/bin/sh\ntouch {}\nexit 1\n", uv_marker.display()),
    );
    write_fake_tool(
        &bin_dir,
        "cargo",
        &format!("#!/bin/sh\ntouch {}\nexit 1\n", cargo_marker.display()),
    );

    let (stdout, stderr, code) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-lock",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !uv_marker.exists(),
        "uv must never be invoked under --no-lock"
    );
    assert!(
        !cargo_marker.exists(),
        "cargo must never be invoked under --no-lock"
    );

    let pyproject = fs::read_to_string(tmp.path().join("pyproject.toml")).unwrap();
    assert!(pyproject.contains("lockonly>=0.49.1"), "{pyproject}");
    assert_eq!(
        fs::read_to_string(tmp.path().join("Cargo.lock")).unwrap(),
        cargo_lock_content,
        "cargo-precise is skipped entirely under --no-lock; Cargo.lock is never touched"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    let uv_entry = fixes.iter().find(|f| f["package"] == "lockonly").unwrap();
    assert_eq!(uv_entry["status"], "pending_relock");
    let cargo_entry = fixes.iter().find(|f| f["package"] == "dupcrate").unwrap();
    assert_eq!(cargo_entry["status"], "skipped");
    assert!(
        cargo_entry["error"]
            .as_str()
            .unwrap()
            .contains("rerun without --no-lock"),
        "{cargo_entry:?}"
    );
}

/// (d) Dry-run lists pending floors, never writes, and exits 1 (or 0 with
/// `--no-fail`).
#[tokio::test]
async fn dry_run_lists_pending_floors_and_exits_1() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-d", "lockonly", "PyPI", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    let pyproject_path = tmp.path().join("pyproject.toml");
    fs::write(&pyproject_path, PYPROJECT_BARE).unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();
    let pyproject_before = fs::read(&pyproject_path).unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["audit", "--fix-audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("would regenerate"), "{stderr}");
    assert!(stderr.contains("uv.lock"), "{stderr}");
    assert_eq!(
        fs::read(&pyproject_path).unwrap(),
        pyproject_before,
        "dry-run must never write"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    let entry = fixes.iter().find(|f| f["package"] == "lockonly").unwrap();
    assert_eq!(entry["status"], "planned");

    let (_stdout2, _stderr2, code2) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--no-fail",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );
    assert_eq!(code2, 0, "--no-fail must exit 0 despite pending fixes");
}

/// (e) An npm transitive floors via a `$name` override; the EOVERRIDE guard
/// means the direct dependency's own spec must also be bumped (companion
/// `ManifestEdit`), since npm refuses a plain-range override for a package
/// that is also a direct dependency. The nested duplicate is LockOnly
/// regardless of whether the direct range would admit its version
/// (positional provenance), while the top-level copy is Manifest-covered and
/// not itself vulnerable.
#[cfg(unix)]
#[tokio::test]
async fn npm_both_direct_and_transitive_writes_dollar_name() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(MultiOsvResponder {
            answers: vec![("examplepkg", "1.2.0", "GHSA-floors-e")],
        })
        .mount(&server)
        .await;
    mount_vuln_get(&server, "GHSA-floors-e", "examplepkg", "npm", "2.5.0").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("package.json"),
        "{\n  \"name\": \"t\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {\n    \"examplepkg\": \"^2.4.0\"\n  }\n}\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("package-lock.json"),
        r#"{ "name": "t", "lockfileVersion": 3, "packages": {
            "": {},
            "node_modules/examplepkg": { "version": "2.4.0" },
            "node_modules/other/node_modules/examplepkg": { "version": "1.2.0" }
        } }"#,
    )
    .unwrap();

    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    write_fake_tool(&bin_dir, "npm", "#!/bin/sh\nexit 0\n");

    let (stdout, stderr, code) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let package_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tmp.path().join("package.json")).unwrap())
            .unwrap();
    assert_eq!(package_json["overrides"]["examplepkg"], "$examplepkg");
    assert_eq!(package_json["dependencies"]["examplepkg"], "^2.5.0");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    assert!(
        fixes.iter().any(|f| f["package"] == "examplepkg"
            && f["method"] == "npm-override"
            && f["status"] == "applied"),
        "{fixes:?}"
    );
    assert!(
        fixes.iter().any(|f| f["package"] == "examplepkg"
            && f["method"] == "manifest"
            && f["status"] == "applied"),
        "{fixes:?}"
    );
}

/// (f) poetry.lock has no floor mechanism upd can write to: a lock-only
/// vulnerable package there is reported unfixable, never blocking the
/// overall exit code (no other fixable/unfixable-without-a-fix target
/// exists).
#[tokio::test]
async fn poetry_lock_only_is_unfixable_with_exit_0() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-f", "poetrydep", "PyPI", "1.0.1").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(
        tmp.path().join("poetry.lock"),
        "[[package]]\nname = \"poetrydep\"\nversion = \"1.0.0\"\ndescription = \"test\"\noptional = false\npython-versions = \">=3.8\"\n\n[metadata]\nlock-version = \"2.0\"\npython-versions = \">=3.8\"\ncontent-hash = \"0000\"\n",
    )
    .unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["audit", "--fix-audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("Cannot auto-fix"), "{stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    let entry = fixes.iter().find(|f| f["package"] == "poetrydep").unwrap();
    assert_eq!(entry["status"], "unfixable");
    assert!(
        entry["error"].as_str().unwrap().contains("poetry"),
        "{entry:?}"
    );
}

/// (g) A lock-only Cargo package floors via `cargo update --precise`, called
/// with the exact `package@locked --precise fixed` spec.
#[cfg(unix)]
#[tokio::test]
async fn cargo_precise_invoked_with_versioned_spec() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-g", "dupcrate", "crates.io", "2.0.1").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"dupcrate\"\nversion = \"1.2.3\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    )
    .unwrap();

    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    let log = tmp.path().join("cargo-invocations.log");
    write_fake_tool(
        &bin_dir,
        "cargo",
        &format!(
            "#!/bin/sh\necho \"$@\" >> {}\ncat > Cargo.lock <<'EOF'\nversion = 4\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"dupcrate\"\nversion = \"2.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nEOF\nexit 0\n",
            log.display()
        ),
    );

    let (stdout, stderr, code) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let log_content = fs::read_to_string(&log).unwrap();
    assert!(
        log_content.contains("update -p dupcrate@1.2.3 --precise 2.0.1"),
        "{log_content}"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    assert!(
        fixes.iter().any(|f| f["package"] == "dupcrate"
            && f["method"] == "cargo-precise"
            && f["status"] == "applied"),
        "{fixes:?}"
    );
}

/// (h) A vulnerability with no fixed version at all makes the package
/// entirely unfixable; routing reports it and the run falls through to the
/// ordinary (non-fix) audit exit code.
#[tokio::test]
async fn all_unfixable_missing_fix_version_falls_through_to_6() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single_no_fix(&server, "GHSA-floors-h").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["audit", "--fix-audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert_eq!(code, 6, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("has no fixed version"), "{stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    let entry = fixes.iter().find(|f| f["package"] == "requests").unwrap();
    assert_eq!(entry["status"], "unfixable");
}

/// (i) Two independent uv projects (unrelated groups: different `path`s)
/// under one run: one project's relock failure rolls back only its own
/// group, leaving the other project's already-applied fix intact.
#[cfg(unix)]
#[tokio::test]
async fn independent_group_survives_other_groups_rollback() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-i", "lockonly", "PyPI", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    fs::write(a.join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(a.join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();
    fs::write(b.join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(b.join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();
    let b_pyproject_before = fs::read(b.join("pyproject.toml")).unwrap();
    let b_uv_lock_before = fs::read(b.join("uv.lock")).unwrap();

    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    write_fake_tool(
        &bin_dir,
        "uv",
        &format!(
            "#!/bin/sh\ncase \"$(pwd)\" in\n*/a) cat > uv.lock <<'EOF'\n{}EOF\nexit 0 ;;\n*/b) echo 'corrupted' >> uv.lock; echo \"error: b group relock failed\" >&2; exit 1 ;;\nesac\n",
            uv_lock_at("lockonly", "0.49.1")
        ),
    );

    let (stdout, stderr, code) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
            "a",
            "b",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");

    let a_pyproject = fs::read_to_string(a.join("pyproject.toml")).unwrap();
    assert!(
        a_pyproject.contains("lockonly>=0.49.1"),
        "a's fix must be kept: {a_pyproject}"
    );
    let a_uv_lock = fs::read_to_string(a.join("uv.lock")).unwrap();
    assert!(
        a_uv_lock.contains("0.49.1"),
        "a's lock must be relocked: {a_uv_lock}"
    );

    assert_eq!(
        fs::read(b.join("pyproject.toml")).unwrap(),
        b_pyproject_before,
        "b must be restored byte-for-byte"
    );
    assert_eq!(
        fs::read(b.join("uv.lock")).unwrap(),
        b_uv_lock_before,
        "b's lock must be restored byte-for-byte"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    assert!(
        fixes.iter().any(|f| f["status"] == "applied"
            && f["path"].as_str().unwrap().contains("a/pyproject.toml")),
        "{fixes:?}"
    );
    assert!(
        fixes.iter().any(|f| f["status"] == "rolled_back"
            && f["path"].as_str().unwrap().contains("b/pyproject.toml")),
        "{fixes:?}"
    );
}

/// (j) An existing `[tool.uv]` constraint already at or above the floor
/// writes nothing (`AlreadySatisfied`), but the group still relocks when the
/// lock itself remains stale: `AlreadySatisfied` describes the WRITE, not
/// whether the lock needs regenerating.
#[cfg(unix)]
#[tokio::test]
async fn already_satisfied_constraint_still_relocks_stale_lock() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-j", "lockonly", "PyPI", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    let pyproject_path = tmp.path().join("pyproject.toml");
    fs::write(
        &pyproject_path,
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n\n[tool.uv]\nconstraint-dependencies = [\"lockonly>=0.49.1\"]\n",
    )
    .unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();
    let pyproject_before = fs::read(&pyproject_path).unwrap();

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
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read(&pyproject_path).unwrap(),
        pyproject_before,
        "already-satisfied constraint must not be rewritten"
    );
    assert!(
        log.exists(),
        "uv must still be invoked to relock the stale lock even though the constraint write was a no-op"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    let entry = fixes.iter().find(|f| f["package"] == "lockonly").unwrap();
    assert_eq!(entry["status"], "already_satisfied");
}

/// (k) A lone `CargoPrecise` target that itself fails keeps status `failed`
/// with its own error - it is never demoted to `rolled_back`, since
/// `rolled_back` only applies to a sibling that had already written or was
/// already satisfied before a DIFFERENT target's failure ended the group
/// (see the adjudicated group test below). The lockfile is still restored
/// byte-for-byte, undoing whatever the failed `cargo` invocation touched.
#[cfg(unix)]
#[tokio::test]
async fn cargo_precise_lone_target_failure_status_is_failed_not_rolled_back() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-k", "dupcrate", "crates.io", "2.0.1").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let cargo_lock_path = tmp.path().join("Cargo.lock");
    fs::write(
        &cargo_lock_path,
        "version = 4\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"dupcrate\"\nversion = \"1.2.3\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    )
    .unwrap();
    let cargo_lock_before = fs::read(&cargo_lock_path).unwrap();

    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    write_fake_tool(
        &bin_dir,
        "cargo",
        "#!/bin/sh\necho 'garbage' >> Cargo.lock\necho \"error: failed to select a version\" >&2\nexit 1\n",
    );

    let (stdout, stderr, code) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read(&cargo_lock_path).unwrap(),
        cargo_lock_before,
        "Cargo.lock must be restored byte-for-byte"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    let entry = fixes.iter().find(|f| f["package"] == "dupcrate").unwrap();
    assert_eq!(
        entry["status"], "failed",
        "the target that itself failed keeps status \"failed\", not \"rolled_back\": {entry:?}"
    );
    assert!(
        entry["error"]
            .as_str()
            .unwrap()
            .contains("failed to select a version"),
        "{entry:?}"
    );
}

/// (l) A package directly declared in the manifest whose requirement already
/// covers the fixed version needs no `ManifestEdit` write (`AlreadySatisfied`
/// via the edit cluster's own satisfied check, a different code path from
/// test (j)'s floor-writer `AlreadySatisfied`), but the group still relocks
/// because the LOCK itself remains stale.
#[cfg(unix)]
#[tokio::test]
async fn manifest_already_satisfied_still_relocks_stale_lock() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-l", "lockonly", "PyPI", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    let pyproject_path = tmp.path().join("pyproject.toml");
    fs::write(
        &pyproject_path,
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = [\"lockonly>=0.49.1\"]\n",
    )
    .unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();
    let pyproject_before = fs::read(&pyproject_path).unwrap();

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
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read(&pyproject_path).unwrap(),
        pyproject_before,
        "manifest requirement already covers the fix; no write expected"
    );
    assert!(
        log.exists(),
        "uv must still be invoked to relock the stale lock even though the manifest cluster wrote nothing"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();
    let entry = fixes.iter().find(|f| f["package"] == "lockonly").unwrap();
    assert_eq!(entry["status"], "already_satisfied");
    assert_eq!(entry["method"], "manifest");
}

/// Adjudicated spec point: in a `CargoPrecise` group with more than one
/// target, a relock-equivalent group failure demotes targets that had
/// already succeeded (`Wrote`/`AlreadySatisfied`) to `rolled_back` carrying
/// the COMBINED failure message, while target(s) that themselves triggered
/// the failure keep status `failed` with their OWN error message. Three
/// packages: `cratea` always succeeds and rewrites only itself; `crateb` and
/// `crateb` always fail with DISTINCT stderr text, so the combined message
/// (visible only on `cratea`'s rolled-back outcome) is distinguishable from
/// either individual failure (visible on `crateb`'s and `cratec`'s own
/// outcomes) - a single-failure fixture could not prove this, since the
/// "combined" and "own" messages would be textually identical by
/// coincidence.
#[cfg(unix)]
#[tokio::test]
async fn cargo_precise_group_failure_distinguishes_failed_from_rolled_back() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(MultiOsvResponder {
            answers: vec![
                ("cratea", "1.0.0", "GHSA-floors-adj"),
                ("crateb", "1.0.0", "GHSA-floors-adj"),
                ("cratec", "1.0.0", "GHSA-floors-adj"),
            ],
        })
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/vulns/GHSA-floors-adj"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "GHSA-floors-adj",
                "summary": "multi-package advisory",
                "affected": [
                    { "package": { "name": "cratea", "ecosystem": "crates.io" },
                      "ranges": [{ "type": "ECOSYSTEM", "events": [{ "introduced": "0" }, { "fixed": "1.1.0" }] }] },
                    { "package": { "name": "crateb", "ecosystem": "crates.io" },
                      "ranges": [{ "type": "ECOSYSTEM", "events": [{ "introduced": "0" }, { "fixed": "1.1.0" }] }] },
                    { "package": { "name": "cratec", "ecosystem": "crates.io" },
                      "ranges": [{ "type": "ECOSYSTEM", "events": [{ "introduced": "0" }, { "fixed": "1.1.0" }] }] }
                ]
            })),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let cargo_lock_path = tmp.path().join("Cargo.lock");
    let cargo_lock_original = "version = 4\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"cratea\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"crateb\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"cratec\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    fs::write(&cargo_lock_path, cargo_lock_original).unwrap();

    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    let cargo_lock_after_cratea = "version = 4\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"cratea\"\nversion = \"1.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"crateb\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"cratec\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    write_fake_tool(
        &bin_dir,
        "cargo",
        &format!(
            "#!/bin/sh\ncase \"$3\" in\ncratea@*) cat > Cargo.lock <<'EOF'\n{cargo_lock_after_cratea}EOF\nexit 0 ;;\ncrateb@*) echo \"error: crateb version conflict\" >&2; exit 1 ;;\ncratec@*) echo \"error: cratec version conflict\" >&2; exit 1 ;;\nesac\n"
        ),
    );

    let (stdout, stderr, code) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read_to_string(&cargo_lock_path).unwrap(),
        cargo_lock_original,
        "the whole group (including cratea's successful write) must be restored"
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let fixes = json["fixes"].as_array().unwrap();

    let cratea = fixes.iter().find(|f| f["package"] == "cratea").unwrap();
    assert_eq!(cratea["status"], "rolled_back");
    let cratea_error = cratea["error"].as_str().unwrap();
    assert!(
        cratea_error.contains("crateb version conflict"),
        "{cratea_error}"
    );
    assert!(
        cratea_error.contains("cratec version conflict"),
        "{cratea_error}"
    );

    let crateb = fixes.iter().find(|f| f["package"] == "crateb").unwrap();
    assert_eq!(crateb["status"], "failed");
    let crateb_error = crateb["error"].as_str().unwrap();
    assert!(
        crateb_error.contains("crateb version conflict"),
        "{crateb_error}"
    );
    assert!(
        !crateb_error.contains("cratec version conflict"),
        "crateb's error must be its OWN message, not the combined one: {crateb_error}"
    );

    let cratec = fixes.iter().find(|f| f["package"] == "cratec").unwrap();
    assert_eq!(cratec["status"], "failed");
    let cratec_error = cratec["error"].as_str().unwrap();
    assert!(
        cratec_error.contains("cratec version conflict"),
        "{cratec_error}"
    );
    assert!(
        !cratec_error.contains("crateb version conflict"),
        "cratec's error must be its OWN message, not the combined one: {cratec_error}"
    );
}

/// (m) An apply-time `Unfixable` outcome (the floor WRITER refuses, as
/// opposed to a routing-time unfixable) must still surface in default
/// text-mode `--fix-audit --apply`: `print_fix_outcome` previously swallowed
/// `FixStatus::Unfixable` on the (wrong) assumption that every unfixable was
/// already reported by the routing-time diagnostics loop, which only walks
/// `routing.unfixable` (targets never routed) - not targets that WERE routed
/// as fixable and only failed once the writer inspected the existing entry.
/// Here `lockonly` routes to a `UvConstraint` floor (lock-only, like fixture
/// (a)), but the pyproject already carries a non-simple multi-clause
/// `[tool.uv] constraint-dependencies` entry the writer refuses to touch, so
/// the outcome resolves to `Unfixable` only after routing succeeded.
#[cfg(unix)]
#[tokio::test]
async fn text_mode_reports_apply_time_unfixable() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-m", "lockonly", "PyPI", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    let pyproject_path = tmp.path().join("pyproject.toml");
    fs::write(
        &pyproject_path,
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n\n[tool.uv]\nconstraint-dependencies = [\"lockonly>=0.30,<0.40\"]\n",
    )
    .unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.35.0")).unwrap();
    let pyproject_before = fs::read(&pyproject_path).unwrap();

    // Poison pill: the writer refuses before any relock would help, so `uv`
    // must never run for this fixture. If it somehow does, fail loudly
    // rather than silently depending on (or hanging on) a real `uv`.
    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    write_fake_tool(&bin_dir, "uv", "#!/bin/sh\nexit 1\n");

    let (stdout, stderr, code) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--output",
            "text",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("Cannot auto-fix"), "{stderr}");
    assert!(stderr.contains("lockonly"), "{stderr}");
    assert!(stderr.contains("not a simple form"), "{stderr}");

    assert_eq!(
        fs::read(&pyproject_path).unwrap(),
        pyproject_before,
        "the writer must refuse without touching the file"
    );
}

/// (n) `FixStatus::PendingRelock` text wording must distinguish a manifest
/// `ManifestEdit` (the direct dependency spec was bumped in place) from a
/// version-floor write (uv-constraint / npm-override): only the latter is a
/// "floor". A plain manifest-covered package with a lockfile sibling, fixed
/// under `--no-lock`, previously printed "floor written to ..." for an edit
/// that never touched a floor mechanism at all.
#[cfg(unix)]
#[tokio::test]
async fn no_lock_manifest_edit_wording_is_not_floor() {
    let server = wiremock::MockServer::start().await;
    mount_osv_single(&server, "GHSA-floors-n", "lockonly", "PyPI", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = [\"lockonly==0.40.0\"]\n",
    )
    .unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();

    // Poison pill: --no-lock must never invoke uv.
    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    let uv_marker = tmp.path().join("uv-invoked.marker");
    write_fake_tool(
        &bin_dir,
        "uv",
        &format!("#!/bin/sh\ntouch {}\nexit 1\n", uv_marker.display()),
    );

    let (stdout, stderr, code) = run_with_env(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-lock",
            "--no-cache",
            "--output",
            "text",
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !uv_marker.exists(),
        "uv must never be invoked under --no-lock"
    );

    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("edit written to"),
        "manifest edit wording expected: {combined}"
    );
    assert!(
        !combined.contains("floor written to"),
        "a manifest edit is not a floor: {combined}"
    );

    let pyproject = fs::read_to_string(tmp.path().join("pyproject.toml")).unwrap();
    assert!(
        pyproject.contains("0.49.1"),
        "manifest entry should have been bumped in place: {pyproject}"
    );
}
