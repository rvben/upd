//! Integration coverage for `upd update --package <name>` version floors:
//! a requested name that matches no manifest occurrence but resolves via a
//! scanned lockfile is floored to the registry latest (or a config pin)
//! through the lock's own mechanism, reusing the routing/apply machinery
//! built for `audit --fix-audit`. This file exercises the full rule set.

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

/// Mounts the crates.io API endpoint `CARGO_REGISTRIES_CRATES_IO_INDEX`
/// resolves to (`{index}/api/v1/crates/{name}`) with a single-release body.
async fn mount_crates_latest(server: &wiremock::MockServer, name: &str, version: &str) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/api/v1/crates/{name}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "crate": { "max_stable_version": version },
                "versions": [{
                    "num": version, "yanked": false,
                    "created_at": "2024-01-01T00:00:00Z"
                }]
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

/// FORWARD GUARD, vacuous in v1. An annotated Makefile pinning the same
/// lock-only name must not suppress the `constraint-dependencies` floor. Today
/// it cannot: the occurrence is keyed `Lang::Annotated` and
/// `matches_manifest_occurrence` (`src/main.rs:473-490`) never sees it. Under
/// V2.1 the same occurrence arrives as `Lang::Python`, the name looks like a
/// direct dependency, and the floor disappears with no diagnostic - which is
/// exactly the failure a forward guard is for.
///
/// Two controls. `lock_only_package_floors_from_mocked_registry_dry_run` above
/// is the same fixture WITHOUT the Makefile, so the annotation is the only
/// variable between them. And the Makefile's own planned update is asserted
/// here, which proves the annotated file was scanned at all - without it this
/// test would pass on a build that never opened the Makefile.
#[tokio::test]
async fn an_annotated_pin_does_not_suppress_a_lock_only_floor() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "lockonly", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "0.40.0")).unwrap();
    fs::write(
        tmp.path().join("Makefile"),
        "LOCKONLY ?= 0.40.0  # upd: pypi lockonly\n",
    )
    .unwrap();

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

    let floors = collect_for_path(files, "pyproject.toml", "updates");
    assert!(
        floors.iter().any(|u| u["package"] == "lockonly"
            && u["current"] == "0.40.0"
            && u["latest"] == "0.49.1"
            && u["method"] == "uv-constraint"
            && u["status"] == "planned"),
        "the floor must survive the annotated pin: {floors:?}"
    );

    let annotated = collect_for_path(files, "Makefile", "updates");
    assert!(
        annotated
            .iter()
            .any(|u| u["package"] == "lockonly" && u["current"] == "0.40.0"),
        "control: the Makefile must have been scanned and planned: {annotated:?}"
    );

    let pyproject = fs::read_to_string(tmp.path().join("pyproject.toml")).unwrap();
    assert_eq!(pyproject, PYPROJECT_BARE, "dry run must not write");
}

/// (2) A candidate that exceeds `--max-bump` is not floored, and is not
/// silently dropped either: it is reported as held back, in the floor file's
/// `capped[]` and in `summary.capped`. Writing nothing and reporting nothing
/// is how a lock-only dependency several majors behind reads as up to date
/// forever.
///
/// Exit stays 0 and `updates_total` stays 0: the ceiling exists to keep such a
/// change out of the gate, and nothing was written.
#[tokio::test]
async fn max_bump_reports_a_capped_floor_as_held_back() {
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
    assert!(
        !has_floor_entry,
        "an above-ceiling candidate must not be floored: {files:?}"
    );
    assert_eq!(json["summary"]["updates_total"], 0, "{}", json["summary"]);
    assert_eq!(
        json["summary"]["files_with_changes"], 0,
        "{}",
        json["summary"]
    );

    let capped = collect_for_path(files, "pyproject.toml", "capped");
    let entry = capped
        .iter()
        .find(|c| c["package"] == "lockonly")
        .unwrap_or_else(|| panic!("no held-back entry for lockonly in {files:?}"));
    assert_eq!(entry["current"], "0.40.0", "{entry:?}");
    assert_eq!(entry["available"], "1.2.0", "{entry:?}");
    assert_eq!(
        entry["bump"], "major",
        "naming the bump says what raising the ceiling would let through: {entry:?}"
    );
    assert_eq!(json["summary"]["capped"], 1, "{}", json["summary"]);
}

/// The negative control for the test above: the same fixture with a candidate
/// INSIDE the ceiling floors normally and reports nothing as held back.
/// Without it, an implementation that called every floor capped would pass.
#[tokio::test]
async fn a_floor_within_the_ceiling_is_not_reported_as_held_back() {
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
            "--max-bump",
            "major",
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
        updates
            .iter()
            .any(|u| u["package"] == "lockonly" && u["status"] == "planned"),
        "{updates:?}"
    );
    assert_eq!(
        json["summary"]["capped"].as_u64().unwrap_or(0),
        0,
        "{}",
        json["summary"]
    );
}

/// Two sibling projects whose locks both resolve the same lock-only package at
/// the same version. The floor loop resolves one representative per
/// `(name, version, ecosystem)` triple, so this is the fixture where a result
/// reported straight from that loop reaches one manifest instead of both.
fn two_projects_locking(tmp: &std::path::Path, package: &str, version: &str) {
    for dir in ["a", "b"] {
        fs::create_dir_all(tmp.join(dir)).unwrap();
        fs::write(tmp.join(dir).join("pyproject.toml"), PYPROJECT_BARE).unwrap();
        fs::write(tmp.join(dir).join("uv.lock"), uv_lock_at(package, version)).unwrap();
    }
}

/// (2b) A held-back floor has to reach every lockfile that resolves the triple,
/// not just the one the floor loop happened to resolve it from. A floor that IS
/// taken fans out through `route_fix_targets`; one reported from the loop
/// bypasses that fan-out, and the manifests it misses read as up to date -
/// exactly the silence `capped[]` exists to end, relocated one level down.
///
/// `a_floor_within_the_ceiling_reaches_every_lockfile_that_holds_it` is the
/// control: it fixes the expected fan-out at 2 on this same fixture, so the
/// count asserted here is what routing produces rather than a number that
/// happens to match.
#[tokio::test]
async fn a_capped_floor_reaches_every_lockfile_that_holds_it() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "lockonly", "1.2.0").await;

    let tmp = tempfile::tempdir().unwrap();
    two_projects_locking(tmp.path(), "lockonly", "0.40.0");

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
    for dir in ["a", "b"] {
        let capped = collect_for_path(files, &format!("{dir}/pyproject.toml"), "capped");
        assert!(
            capped
                .iter()
                .any(|c| c["package"] == "lockonly" && c["available"] == "1.2.0"),
            "{dir}/pyproject.toml has no held-back entry: {files:?}"
        );
    }
    assert_eq!(json["summary"]["capped"], 2, "{}", json["summary"]);
}

/// The control for the test above: the same two-project fixture with a
/// candidate INSIDE the ceiling floors both manifests, which is what fixes the
/// expected fan-out at 2.
#[tokio::test]
async fn a_floor_within_the_ceiling_reaches_every_lockfile_that_holds_it() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "lockonly", "0.49.1").await;

    let tmp = tempfile::tempdir().unwrap();
    two_projects_locking(tmp.path(), "lockonly", "0.40.0");

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "lockonly",
            "--max-bump",
            "major",
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
    for dir in ["a", "b"] {
        let updates = collect_for_path(files, &format!("{dir}/pyproject.toml"), "updates");
        assert!(
            updates
                .iter()
                .any(|u| u["package"] == "lockonly" && u["status"] == "planned"),
            "{dir}/pyproject.toml has no floor: {files:?}"
        );
    }
    assert_eq!(json["summary"]["updates_total"], 2, "{}", json["summary"]);
    assert_eq!(
        json["summary"]["capped"].as_u64().unwrap_or(0),
        0,
        "{}",
        json["summary"]
    );
}

fn write_poetry_project(tmp: &std::path::Path) {
    fs::write(tmp.join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    fs::write(
        tmp.join("poetry.lock"),
        "[[package]]\nname = \"lockonly\"\nversion = \"0.40.0\"\noptional = false\npython-versions = \"*\"\n",
    )
    .unwrap();
}

fn floor_json(
    stdout: &str,
    stderr: &str,
    code: i32,
    filename: &str,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let json: serde_json::Value = serde_json::from_str(stdout).unwrap();
    let files = json["files"].as_array().unwrap().clone();
    (
        collect_for_path(&files, filename, "updates"),
        collect_for_path(&files, filename, "capped"),
    )
}

/// (2c) `poetry.lock` has no floor mechanism at all: routing reports every
/// lock-only floor there as `unfixable`, whatever version was found. The
/// ceiling therefore changes nothing about what upd can write, and reporting
/// such a candidate as held back would tell the reader that raising the
/// ceiling releases an update that no ceiling was ever blocking.
///
/// Both ceilings are run in one test because the claim IS the equivalence: the
/// `major` run is what proves the `minor` run's answer is the honest one rather
/// than a second silence. `max_bump_reports_a_capped_floor_as_held_back` is the
/// other side of the control, a uv floor that CAN be written and so must still
/// report as held back.
#[tokio::test]
async fn a_poetry_floor_is_unfixable_whatever_the_ceiling_says() {
    for max_bump in ["major", "minor"] {
        let server = wiremock::MockServer::start().await;
        mount_pypi_latest(&server, "lockonly", "1.2.0").await;
        let tmp = tempfile::tempdir().unwrap();
        write_poetry_project(tmp.path());

        let (stdout, stderr, code) = run_with_env(
            &[
                "update",
                "--package",
                "lockonly",
                "--max-bump",
                max_bump,
                "--format",
                "json",
                "--no-cache",
                ".",
            ],
            tmp.path(),
            &[("UV_INDEX_URL", &server.uri())],
        );

        let (updates, capped) = floor_json(&stdout, &stderr, code, "poetry.lock");
        let entry = updates
            .iter()
            .find(|u| u["package"] == "lockonly")
            .unwrap_or_else(|| panic!("--max-bump {max_bump}: no entry in {updates:?}"));
        assert_eq!(entry["status"], "unfixable", "--max-bump {max_bump}");
        assert!(
            entry["error"]
                .as_str()
                .unwrap_or_default()
                .contains("no floor mechanism exists for poetry.lock"),
            "--max-bump {max_bump}: {entry:?}"
        );
        assert!(
            capped.is_empty(),
            "--max-bump {max_bump}: a floor that can never be written is not held back by a ceiling: {capped:?}"
        );
    }
}

/// (2d) The same rule for a target upd cannot write for a reason other than the
/// lock's kind: an npm direct dependency whose spec cannot be bumped fails the
/// override guard (see `own_name_direct_with_unbumpable_spec_is_unfixable`,
/// which is this fixture in cap). Above the ceiling it must reach the same
/// `unfixable` verdict rather than being reported as merely held back.
#[tokio::test]
async fn an_npm_floor_upd_cannot_write_is_unfixable_not_held_back() {
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
            "--max-bump",
            "minor",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[("NPM_REGISTRY", &server.uri())],
    );

    let (updates, capped) = floor_json(&stdout, &stderr, code, "package.json");
    let entry = updates
        .iter()
        .find(|u| u["package"] == "examplepkg")
        .unwrap_or_else(|| panic!("no entry in {updates:?}"));
    assert_eq!(entry["status"], "unfixable", "{entry:?}");
    assert!(
        entry["error"]
            .as_str()
            .unwrap_or_default()
            .contains("has a spec upd cannot bump"),
        "{entry:?}"
    );
    assert!(
        capped.is_empty(),
        "a floor the override guard refuses is not held back by a ceiling: {capped:?}"
    );
}

/// (2e) One `(name, version, ecosystem)` triple can resolve in two locks with
/// different floor mechanisms, and the verdict above the ceiling belongs to the
/// lock, not to the package. Here `a/uv.lock` can take a constraint floor and
/// is genuinely waiting on the ceiling alone; `b/poetry.lock` can never take
/// one. A single verdict for the whole triple gets one of the two wrong: it
/// either promises the poetry project an update that raising the ceiling
/// cannot deliver, or silences the uv project that raising the ceiling would
/// update, leaving it looking up to date.
///
/// This fixture carries its own controls: each project is the other's, since
/// the same package at the same version must come out held back in one and
/// unfixable in the other.
#[tokio::test]
async fn a_capped_floor_is_classified_per_lock_not_per_package() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "lockonly", "1.2.0").await;

    let tmp = tempfile::tempdir().unwrap();
    for dir in ["a", "b"] {
        fs::create_dir_all(tmp.path().join(dir)).unwrap();
        fs::write(tmp.path().join(dir).join("pyproject.toml"), PYPROJECT_BARE).unwrap();
    }
    fs::write(
        tmp.path().join("a/uv.lock"),
        uv_lock_at("lockonly", "0.40.0"),
    )
    .unwrap();
    fs::write(
        tmp.path().join("b/poetry.lock"),
        "[[package]]\nname = \"lockonly\"\nversion = \"0.40.0\"\noptional = false\npython-versions = \"*\"\n",
    )
    .unwrap();

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
    let files = json["files"].as_array().unwrap().clone();

    let capped = collect_for_path(&files, "a/pyproject.toml", "capped");
    assert_eq!(
        capped.len(),
        1,
        "the uv project can take this floor, so the ceiling is what held it: {json}"
    );
    assert_eq!(capped[0]["package"], "lockonly");
    assert_eq!(capped[0]["available"], "1.2.0");

    assert!(
        collect_for_path(&files, "b/pyproject.toml", "capped").is_empty(),
        "the poetry project has no floor mechanism, so no ceiling held it: {json}"
    );
    let poetry = collect_for_path(&files, "poetry.lock", "updates");
    assert_eq!(poetry.len(), 1, "{json}");
    assert_eq!(poetry[0]["status"], "unfixable", "{json}");

    assert_eq!(json["summary"]["capped"].as_u64().unwrap_or(0), 1, "{json}");
    assert_eq!(
        json["summary"]["unfixable"].as_u64().unwrap_or(0),
        1,
        "{json}"
    );
}

/// (2f) A newer release `upd` has no mechanism to write is still a newer
/// release, so a text run that found one must not close on a green tick
/// claiming every dependency is current. The reason goes to stderr per
/// package; the summary line on stdout is what stops the run reading as clean.
///
/// Both ceilings run because neither is what refuses the write, and the same
/// fixture with the registry serving the locked version is the control that
/// the tick still prints when nothing is actually waiting.
#[tokio::test]
async fn an_unfixable_floor_is_not_reported_as_up_to_date() {
    for (latest, waiting) in [("1.2.0", true), ("0.40.0", false)] {
        for max_bump in ["major", "minor"] {
            let server = wiremock::MockServer::start().await;
            mount_pypi_latest(&server, "lockonly", latest).await;
            let tmp = tempfile::tempdir().unwrap();
            write_poetry_project(tmp.path());

            let (stdout, stderr, code) = run_with_env(
                &[
                    "update",
                    "--package",
                    "lockonly",
                    "--max-bump",
                    max_bump,
                    "--format",
                    "text",
                    "--no-cache",
                    ".",
                ],
                tmp.path(),
                &[("UV_INDEX_URL", &server.uri())],
            );

            let case = format!("latest {latest}, --max-bump {max_bump}");
            assert_eq!(code, 0, "{case}: stdout: {stdout}\nstderr: {stderr}");
            assert_eq!(
                stdout.contains("all dependencies up to date"),
                !waiting,
                "{case}: stdout: {stdout}"
            );
            assert_eq!(
                stdout.contains("Cannot auto-fix 1 package(s) with a newer release available"),
                waiting,
                "{case}: stdout: {stdout}"
            );
        }
    }
}

/// One writer-refusal fixture: a manifest whose existing floor entry the writer
/// will not rewrite, paired with the writable spelling of the same manifest as
/// its control.
struct WriterCase {
    label: &'static str,
    manifest: &'static str,
    lock: &'static str,
    lock_body: String,
    npm: bool,
    package: &'static str,
    latest: &'static str,
    writable: &'static str,
    unwritable: &'static str,
    refusal: &'static str,
}

/// (2g) Routing places a floor target without ever reading the manifest, so a
/// routed target is not proof the floor can be written. An existing uv
/// constraint `upd` will not rewrite, or an npm override already in the nested
/// form, refuses the write wherever the ceiling sits; reporting such a
/// candidate as held back tells the reader that raising `--max-bump` applies
/// it, when raising it produces the refusal instead. The refusal is also the
/// only answer carrying the guidance for fixing it by hand, so the held-back
/// wording costs the reader the one thing they could have acted on.
///
/// The writable spelling of each manifest is the control, and it is what makes
/// the equality mean anything: it shows the fixture DOES observe a
/// ceiling-dependent answer (`planned` in cap, `capped` above it), so the
/// unwritable spelling reporting identically under both ceilings is an
/// invariant rather than a fixture blind to `--max-bump`.
#[tokio::test]
async fn a_floor_the_writer_refuses_is_unfixable_under_every_ceiling() {
    let cases = [
        WriterCase {
            label: "uv constraint",
            manifest: "pyproject.toml",
            lock: "uv.lock",
            lock_body: uv_lock_at("lockonly", "0.40.0"),
            npm: false,
            package: "lockonly",
            latest: "1.2.0",
            writable: PYPROJECT_BARE,
            unwritable: "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n\n[tool.uv]\nconstraint-dependencies = [\"lockonly>=0.1,<1.0\"]\n",
            refusal: "is not a simple form",
        },
        WriterCase {
            label: "npm override",
            manifest: "package.json",
            lock: "package-lock.json",
            lock_body: npm_lock_with("examplepkg", "1.2.0"),
            npm: true,
            package: "examplepkg",
            latest: "9.9.9",
            writable: r#"{"name": "t", "version": "1.0.0", "dependencies": {"host": "^1.0.0"}}"#,
            unwritable: r#"{"name": "t", "version": "1.0.0", "dependencies": {"host": "^1.0.0"}, "overrides": {"examplepkg": {".": ">=1.0.0"}}}"#,
            refusal: "nested override form",
        },
    ];

    for case in cases {
        for (shape, manifest) in [("writable", case.writable), ("unwritable", case.unwritable)] {
            for max_bump in ["major", "minor"] {
                let server = wiremock::MockServer::start().await;
                if case.npm {
                    mount_npm_latest(&server, case.package, case.latest).await;
                } else {
                    mount_pypi_latest(&server, case.package, case.latest).await;
                }
                let tmp = tempfile::tempdir().unwrap();
                fs::write(tmp.path().join(case.manifest), manifest).unwrap();
                fs::write(tmp.path().join(case.lock), &case.lock_body).unwrap();

                let env_key = if case.npm {
                    "NPM_REGISTRY"
                } else {
                    "UV_INDEX_URL"
                };
                let (stdout, stderr, _) = run_with_env(
                    &[
                        "update",
                        "--package",
                        case.package,
                        "--max-bump",
                        max_bump,
                        "--format",
                        "json",
                        "--no-cache",
                        ".",
                    ],
                    tmp.path(),
                    &[(env_key, &server.uri())],
                );

                let who = format!("{} / {shape} / --max-bump {max_bump}", case.label);
                let json: serde_json::Value = serde_json::from_str(&stdout)
                    .unwrap_or_else(|e| panic!("{who}: {e}\nstdout: {stdout}\nstderr: {stderr}"));
                let files = json["files"].as_array().unwrap().clone();
                let updates = collect_for_path(&files, case.manifest, "updates");
                let capped = collect_for_path(&files, case.manifest, "capped");
                let entry = updates.iter().find(|u| u["package"] == case.package);

                if shape == "writable" {
                    // The control: this manifest CAN take the floor, so the
                    // ceiling alone decides, and the two runs must differ.
                    if max_bump == "major" {
                        assert_eq!(
                            entry.map(|e| e["status"].clone()),
                            Some(serde_json::json!("planned")),
                            "{who}: {json}"
                        );
                        assert!(capped.is_empty(), "{who}: {json}");
                    } else {
                        assert_eq!(capped.len(), 1, "{who}: {json}");
                        assert_eq!(capped[0]["available"], case.latest, "{who}: {json}");
                        assert!(entry.is_none(), "{who}: {json}");
                    }
                    continue;
                }

                let entry = entry.unwrap_or_else(|| panic!("{who}: no entry in {json}"));
                assert_eq!(entry["status"], "unfixable", "{who}: {json}");
                assert!(
                    entry["error"]
                        .as_str()
                        .unwrap_or_default()
                        .contains(case.refusal),
                    "{who}: the guidance for fixing it by hand is the whole point of the \
                     unfixable answer: {json}"
                );
                assert!(
                    capped.is_empty(),
                    "{who}: no ceiling is holding a floor the writer refuses: {json}"
                );
                assert_eq!(
                    json["summary"]["unfixable"].as_u64().unwrap_or(0),
                    1,
                    "{who}: {json}"
                );
            }
        }
    }
}

/// A lock holding TWO nested copies of `name`, one under each host, so a
/// single `--package` run resolves the same name at two different versions.
/// Both are nested for the reason [`npm_lock_with`] documents.
fn npm_lock_with_two(name: &str, first: &str, second: &str) -> String {
    format!(
        r#"{{
  "name": "t", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
  "packages": {{
    "": {{ "name": "t", "version": "1.0.0" }},
    "node_modules/hosta": {{ "version": "1.0.0" }},
    "node_modules/hosta/node_modules/{name}": {{ "version": "{first}" }},
    "node_modules/hostb": {{ "version": "1.0.0" }},
    "node_modules/hostb/node_modules/{name}": {{ "version": "{second}" }}
  }}
}}"#
    )
}

/// (2h) One floor is written per manifest and package, whatever the lock holds:
/// an `overrides` entry at `>=1.2.0` lifts every copy of the package at once.
/// The in-cap path merges the copies into that one floor, so the report above
/// the ceiling has to describe the same single floor.
///
/// Two shapes, because the fan-out breaks it in two different ways. With one
/// copy in cap and one above it, the in-cap copy's floor already covers the
/// capped one, so reporting the capped copy as held back asks for a
/// `--max-bump` that changes nothing about what gets written. With both copies
/// above the ceiling, the report has to fold to one entry at the highest locked
/// version, which is the version the merge keeps.
///
/// The `--max-bump major` run of the both-capped shape is the control: it is
/// the same fixture going through the merging path, and it pins the entry count
/// and the version the capped report has to match.
#[tokio::test]
async fn capped_copies_fold_into_the_one_floor_that_would_be_written() {
    let manifest = r#"{"name": "t", "version": "1.0.0", "dependencies": {"hosta": "^1.0.0"}}"#;

    // One copy in cap (1.0.0 -> 1.2.0 is minor), one above it (0.9.0 -> 1.2.0
    // is major): the planned floor covers both, so nothing is held back.
    let server = wiremock::MockServer::start().await;
    mount_npm_latest(&server, "examplepkg", "1.2.0").await;
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("package.json"), manifest).unwrap();
    fs::write(
        tmp.path().join("package-lock.json"),
        npm_lock_with_two("examplepkg", "0.9.0", "1.0.0"),
    )
    .unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "examplepkg",
            "--max-bump",
            "minor",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[("NPM_REGISTRY", &server.uri())],
    );
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap().clone();
    let updates = collect_for_path(&files, "package.json", "updates");
    let capped = collect_for_path(&files, "package.json", "capped");
    assert_eq!(updates.len(), 1, "{json}");
    assert_eq!(updates[0]["status"], "planned", "{json}");
    assert_eq!(updates[0]["latest"], "1.2.0", "{json}");
    assert!(
        capped.is_empty(),
        "the planned floor at 1.2.0 lifts the 0.9.0 copy too, so no ceiling is \
         holding it back: {json}"
    );

    // Both copies above the ceiling: one entry, at the higher locked version.
    for max_bump in ["minor", "major"] {
        let server = wiremock::MockServer::start().await;
        mount_npm_latest(&server, "examplepkg", "1.2.0").await;
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), manifest).unwrap();
        fs::write(
            tmp.path().join("package-lock.json"),
            npm_lock_with_two("examplepkg", "0.8.0", "0.9.0"),
        )
        .unwrap();

        let (stdout, stderr, _) = run_with_env(
            &[
                "update",
                "--package",
                "examplepkg",
                "--max-bump",
                max_bump,
                "--format",
                "json",
                "--no-cache",
                ".",
            ],
            tmp.path(),
            &[("NPM_REGISTRY", &server.uri())],
        );
        let json: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("{max_bump}: {e}\nstdout: {stdout}\nstderr: {stderr}"));
        let files = json["files"].as_array().unwrap().clone();
        let updates = collect_for_path(&files, "package.json", "updates");
        let capped = collect_for_path(&files, "package.json", "capped");

        // The control: in cap the merge produces one floor at the higher locked
        // version, which is the entry the capped report has to mirror.
        let (entries, other) = if max_bump == "major" {
            (updates, capped)
        } else {
            (capped, updates)
        };
        assert_eq!(entries.len(), 1, "{max_bump}: {json}");
        assert_eq!(entries[0]["current"], "0.9.0", "{max_bump}: {json}");
        assert!(other.is_empty(), "{max_bump}: {json}");
    }
}

/// (2i) `--no-lock` refuses a `cargo-precise` floor outright: that floor
/// mutates nothing but `Cargo.lock`, so there is nothing left for it to do.
/// The refusal does not depend on the ceiling, so reporting the candidate as
/// held back would send the reader to `--max-bump`, which produces the same
/// refusal, instead of to the flag that is actually blocking it.
///
/// The run without `--no-lock` is the control: it is the same fixture with a
/// ceiling-dependent answer (`planned` in cap, `capped` above it), which is what
/// makes the `--no-lock` runs answering identically an invariant rather than a
/// fixture that cannot see `--max-bump`.
#[tokio::test]
async fn a_cargo_floor_blocked_by_no_lock_is_skipped_under_every_ceiling() {
    const CARGO_TOML: &str = "[package]\nname = \"t\"\nversion = \"0.1.0\"\n";
    const CARGO_LOCK: &str = "version = 4\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"dupcrate\"\nversion = \"1.2.3\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";

    for no_lock in [true, false] {
        for max_bump in ["major", "minor"] {
            let server = wiremock::MockServer::start().await;
            mount_crates_latest(&server, "dupcrate", "2.0.1").await;
            let tmp = tempfile::tempdir().unwrap();
            fs::write(tmp.path().join("Cargo.toml"), CARGO_TOML).unwrap();
            fs::write(tmp.path().join("Cargo.lock"), CARGO_LOCK).unwrap();

            let mut args = vec![
                "update",
                "--package",
                "dupcrate",
                "--max-bump",
                max_bump,
                "--format",
                "json",
                "--no-cache",
                ".",
            ];
            if no_lock {
                args.insert(1, "--no-lock");
            }
            let (stdout, stderr, _) = run_with_env(
                &args,
                tmp.path(),
                &[("CARGO_REGISTRIES_CRATES_IO_INDEX", &server.uri())],
            );

            let who = format!("no_lock={no_lock} / --max-bump {max_bump}");
            let json: serde_json::Value = serde_json::from_str(&stdout)
                .unwrap_or_else(|e| panic!("{who}: {e}\nstdout: {stdout}\nstderr: {stderr}"));
            let files = json["files"].as_array().unwrap().clone();
            let updates = collect_for_path(&files, "Cargo.lock", "updates");
            let capped = collect_for_path(&files, "Cargo.lock", "capped");

            if !no_lock {
                // The control: the ceiling alone decides when nothing else
                // blocks the floor.
                if max_bump == "major" {
                    assert_eq!(
                        updates.first().map(|u| u["status"].clone()),
                        Some(serde_json::json!("planned")),
                        "{who}: {json}"
                    );
                    assert!(capped.is_empty(), "{who}: {json}");
                } else {
                    assert_eq!(capped.len(), 1, "{who}: {json}");
                    assert_eq!(capped[0]["available"], "2.0.1", "{who}: {json}");
                    assert!(updates.is_empty(), "{who}: {json}");
                }
                continue;
            }

            let entry = updates
                .iter()
                .find(|u| u["package"] == "dupcrate")
                .unwrap_or_else(|| panic!("{who}: no entry in {json}"));
            assert_eq!(entry["status"], "skipped", "{who}: {json}");
            assert!(
                entry["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("rerun without --no-lock"),
                "{who}: the blocking flag is the one thing the reader can act on: {json}"
            );
            assert!(
                capped.is_empty(),
                "{who}: no ceiling is holding a floor --no-lock refuses: {json}"
            );
        }
    }
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

/// (2m) A manifest can already carry a floor at or above the candidate while
/// its lockfile still holds the older version. The floor needs no rewrite, but
/// the lock does, and `--lock` regenerating it is exactly the work the ceiling
/// is holding back: the candidate got into the capped set only because the lock
/// sits below it. Reporting the target as satisfied there answers a question
/// nobody asked - the floor's state - and hides the update.
///
/// The `major` run is the control that makes the claim falsifiable: the same
/// fixture with the ceiling raised has nothing held back, so a `capped` entry
/// under `minor` can only be the ceiling talking.
#[tokio::test]
async fn a_capped_floor_already_in_the_manifest_is_still_held_back() {
    for max_bump in ["major", "minor"] {
        let server = wiremock::MockServer::start().await;
        mount_pypi_latest(&server, "lockonly", "2.0.0").await;
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n\n[tool.uv]\nconstraint-dependencies = [\"lockonly>=2.0.0\"]\n",
        )
        .unwrap();
        fs::write(tmp.path().join("uv.lock"), uv_lock_at("lockonly", "1.0.0")).unwrap();

        let (stdout, stderr, code) = run_with_env(
            &[
                "update",
                "--package",
                "lockonly",
                "--max-bump",
                max_bump,
                "--format",
                "json",
                "--no-cache",
                ".",
            ],
            tmp.path(),
            &[("UV_INDEX_URL", &server.uri())],
        );

        let (_, capped) = floor_json(&stdout, &stderr, code, "pyproject.toml");
        let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let label = format!("--max-bump {max_bump}");

        if max_bump == "major" {
            assert!(
                capped.is_empty(),
                "{label}: no ceiling is above the candidate: {json}"
            );
            continue;
        }

        let entry = capped
            .iter()
            .find(|c| c["package"] == "lockonly")
            .unwrap_or_else(|| panic!("{label}: no held-back entry in {json}"));
        assert_eq!(entry["current"], "1.0.0", "{label}: {entry:?}");
        assert_eq!(entry["available"], "2.0.0", "{label}: {entry:?}");
        assert_eq!(json["summary"]["capped"], 1, "{label}: {}", json["summary"]);
    }
}
