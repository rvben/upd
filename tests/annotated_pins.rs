//! End-to-end tests for comment-annotated version pins, driven through the
//! built binary against a mock PyPI. These are the discovery tests: which
//! files are claimed, which are not, and that a claimed file round-trips.

use std::process::Command;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use tempfile::TempDir;
use upd::annotation::UNSUPPORTED_SOURCE_PREFIX;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Minimal PyPI JSON API body. Every release needs at least one file entry:
/// the parser treats a release whose file list is empty as fully yanked
/// (`files.iter().all(|f| f.yanked)`, `src/registry/pypi.rs:473`) and drops it.
fn pypi_releases(versions: &[&str]) -> String {
    let entries: Vec<String> = versions
        .iter()
        .map(|v| {
            format!(r#""{v}":[{{"yanked":false,"upload_time_iso_8601":"2024-01-01T00:00:00Z"}}]"#)
        })
        .collect();
    format!(r#"{{"releases":{{{}}}}}"#, entries.join(","))
}

async fn mount_pypi_body(mock: &MockServer, package: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(format!("/pypi/{package}/json")))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(mock)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/simple/{package}/")))
        .respond_with(ResponseTemplate::new(404))
        .mount(mock)
        .await;
}

async fn mount_pypi(mock: &MockServer, package: &str, versions: &[&str]) {
    mount_pypi_body(mock, package, pypi_releases(versions)).await;
}

/// `pypi_releases` with an explicit publication time per version. `list_versions`
/// takes the earliest `upload_time_iso_8601` across a release's files
/// (`src/registry/pypi.rs:906-941`), so one file per release fixes its age.
fn pypi_releases_at(versions: &[(&str, DateTime<Utc>)]) -> String {
    let entries: Vec<String> = versions
        .iter()
        .map(|(v, at)| {
            format!(
                r#""{v}":[{{"yanked":false,"upload_time_iso_8601":"{}"}}]"#,
                at.to_rfc3339()
            )
        })
        .collect();
    format!(r#"{{"releases":{{{}}}}}"#, entries.join(","))
}

fn ago(d: Duration) -> DateTime<Utc> {
    Utc::now() - d
}

/// Minimal npm abbreviated-metadata body. Both `dist-tags` and `versions` are
/// required: `NpmAbbreviatedResponse` gives neither a serde default
/// (`src/registry/npm.rs:37-46`), so a body missing either fails to deserialize
/// and the lookup surfaces as an error instead of an answer.
fn npm_metadata(latest: &str, versions: &[&str]) -> String {
    let entries: Vec<String> = versions.iter().map(|v| format!(r#""{v}":{{}}"#)).collect();
    format!(
        r#"{{"dist-tags":{{"latest":"{latest}"}},"versions":{{{}}}}}"#,
        entries.join(",")
    )
}

/// `NpmRegistry` builds its URL as `{registry_url}/{package}`
/// (`src/registry/npm.rs:349`) and reads `NPM_REGISTRY` verbatim with no
/// trailing-slash normalization (`:286`), so `mock.uri()` goes in unchanged.
async fn mount_npm(mock: &MockServer, package: &str, latest: &str, versions: &[&str]) {
    Mock::given(method("GET"))
        .and(path(format!("/{package}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(npm_metadata(latest, versions)))
        .mount(mock)
        .await;
}

/// Run the binary in `dir` with an isolated cache and a controlled environment.
/// The baseline contains fixture-local config/cache directories and disables
/// pip config discovery; registry URLs are added explicitly by each test.
/// Clearing the inherited environment keeps host proxy, credential, CA,
/// registry, and package-manager configuration out of both registry
/// construction and exact request counts. No covered path spawns an external
/// tool, so `PATH` is not restored. Every variable is child-only, which lets
/// these tests run in parallel with no `#[serial]`.
fn run_env(dir: &TempDir, envs: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    let home = dir.path().join("home");
    let xdg_config = dir.path().join("xdg-config");
    std::fs::create_dir_all(&home).expect("fixture HOME created");
    std::fs::create_dir_all(&xdg_config).expect("fixture XDG config created");

    let mut command = Command::new(env!("CARGO_BIN_EXE_upd"));
    command
        .env_clear()
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg_config)
        .env("PIP_CONFIG_FILE", pip_config_null_device())
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .current_dir(dir.path());
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("upd ran")
}

#[cfg(unix)]
fn pip_config_null_device() -> &'static str {
    "/dev/null"
}

#[cfg(windows)]
fn pip_config_null_device() -> &'static str {
    "NUL"
}

/// Run the binary against a mock PyPI, the common case.
fn run(dir: &TempDir, index_url: &str, args: &[&str]) -> std::process::Output {
    run_env(dir, &[("UV_INDEX_URL", index_url)], args)
}

fn json_of(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}):\n{stdout}"))
}

/// `emit_update_json` builds `files[]` from scan results collected in
/// `buffer_unordered` completion order, so `files[0]` is only meaningful when
/// exactly one file was scanned. Every multi-file assertion in this suite goes
/// through this instead.
fn file_named<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["files"]
        .as_array()
        .expect("files[] is an array")
        .iter()
        .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with(name)))
        .unwrap_or_else(|| panic!("no file report whose path ends in {name}:\n{report}"))
}

/// Occurrence count, not presence. Several tests here fail only on a *second*
/// print of a diagnostic that is correct once.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_annotated_makefile_is_updated_end_to_end() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(
        &makefile,
        "BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n\nbuild:\n\techo 2.6.1\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "upd failed:\n{stderr}");

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "BAO_VERSION ?= 2.7.0  # upd: pypi openbao-cli\n\nbuild:\n\techo 2.6.1\n",
        "only the annotated line is rewritten"
    );

    let report = json_of(&output);
    assert_eq!(report["summary"]["files_scanned"], 1);
    assert_eq!(report["summary"]["updates_total"], 1);
    assert_eq!(report["files"][0]["file_type"], "annotated");
    assert_eq!(report["files"][0]["lang"], "annotated");
    assert_eq!(report["files"][0]["updates"][0]["package"], "openbao-cli");
    assert_eq!(report["files"][0]["updates"][0]["source"], "pypi");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_included_yaml_file_is_discovered_and_updated_end_to_end() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "shinyhub", &["0.11.16", "0.12.6"]).await;

    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "include = [\"ansible/roles/*/vars/*.yml\"]\n",
    )
    .unwrap();
    let vars = dir.path().join("ansible/roles/shinyhub/vars");
    std::fs::create_dir_all(&vars).unwrap();
    let main_yml = vars.join("main.yml");
    std::fs::write(
        &main_yml,
        "---\nshinyhub_version: \"0.11.16\"  # upd: pypi shinyhub\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "upd failed:\n{stderr}");
    assert_eq!(
        std::fs::read_to_string(&main_yml).unwrap(),
        "---\nshinyhub_version: \"0.12.6\"  # upd: pypi shinyhub\n"
    );

    let report = json_of(&output);
    assert_eq!(report["summary"]["files_scanned"], 1);
    assert_eq!(report["summary"]["updates_total"], 1);
    assert_eq!(report["files"][0]["file_type"], "annotated");
    assert_eq!(report["files"][0]["updates"][0]["package"], "shinyhub");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_makefile_is_claimed_at_the_root_and_in_a_subdirectory() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let line = "BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n";
    std::fs::write(dir.path().join("Makefile"), line).unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("common.mk"), line).unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(output.status.success());

    let report = json_of(&output);
    assert_eq!(
        report["summary"]["files_scanned"], 2,
        "the match target is the file name, not the whole path: {report}"
    );
    assert_eq!(report["summary"]["updates_total"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_walk_skips_markdown_but_an_explicit_path_reaches_it() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let readme = dir.path().join("README.md");
    let line = "    BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n";
    std::fs::write(&readme, line).unwrap();

    let walked = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert_eq!(
        std::fs::read_to_string(&readme).unwrap(),
        line,
        "a directory walk must never open Markdown: {}",
        String::from_utf8_lossy(&walked.stdout)
    );

    let named = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "README.md"],
    );
    assert!(named.status.success());
    assert_eq!(
        std::fs::read_to_string(&readme).unwrap(),
        "    BAO_VERSION ?= 2.7.0  # upd: pypi openbao-cli\n",
        "naming the file explicitly bypasses the pattern set"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_file_with_no_annotation_is_scanned_and_reports_nothing() {
    let mock = MockServer::start().await;

    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("some-notes.txt"), "nothing to see here\n").unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--output", "json", "some-notes.txt"],
    );
    assert!(output.status.success());

    let report = json_of(&output);
    assert_eq!(
        report["summary"]["files_scanned"], 1,
        "the file was read and had no annotation, which is the honest report"
    );
    assert_eq!(report["summary"]["updates_total"], 0);
    assert_eq!(report["summary"]["errors"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_utf8_explicit_file_is_an_error_not_a_panic() {
    let mock = MockServer::start().await;

    let dir = TempDir::new().unwrap();
    let blob = dir.path().join("blob.bin");
    std::fs::write(&blob, [0xff_u8, 0xfe, 0x00, 0x42]).unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--output", "json", "blob.bin"],
    );

    assert_ne!(
        output.status.code(),
        Some(101),
        "a Rust panic exits 101; this must take the read_file_safe error path"
    );
    let report = json_of(&output);
    assert_eq!(report["summary"]["files_scanned"], 1);
    assert_eq!(
        report["summary"]["errors"], 1,
        "an unreadable file is an error entry, not a silent skip: {report}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lang_python_selects_the_pypi_pin_and_skips_the_github_pin() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(
        &makefile,
        "BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n\
         BUN_VERSION ?= v1.1.0  # upd: github-releases oven-sh/bun\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &[
            "update", "--apply", "--lang", "python", "--output", "json", ".",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "upd failed:\n{stderr}");

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "BAO_VERSION ?= 2.7.0  # upd: pypi openbao-cli\n\
         BUN_VERSION ?= v1.1.0  # upd: github-releases oven-sh/bun\n"
    );

    let report = json_of(&output);
    assert_eq!(report["files"][0]["updates"].as_array().unwrap().len(), 1);
    assert_eq!(
        report["files"][0]["errors"].as_array().unwrap().len(),
        0,
        "the github-releases line must be dropped before its lookup, not after \
         it fails: an error entry here means the langs gate runs too late: {report}"
    );
}

/// A mixed-source file driven end to end. In-process coverage proves the same
/// routing including `github-releases`, which has no base-URL
/// override and so cannot be mocked here. This is the CLI half, using the two
/// sources that do have overrides.
#[tokio::test(flavor = "multi_thread")]
async fn two_sources_in_one_file_each_reach_their_own_registry_through_the_cli() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;
    mount_npm(&mock, "express", "4.18.2", &["4.17.1", "4.18.2"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(
        &makefile,
        "BAO ?= 2.6.1  # upd: pypi openbao-cli\nEXPRESS ?= 4.17.1  # upd: npm express\n",
    )
    .unwrap();

    let uri = mock.uri();
    let output = run_env(
        &dir,
        &[
            ("UV_INDEX_URL", uri.as_str()),
            ("NPM_REGISTRY", uri.as_str()),
        ],
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "BAO ?= 2.7.0  # upd: pypi openbao-cli\nEXPRESS ?= 4.18.2  # upd: npm express\n"
    );

    let report = json_of(&output);
    assert_eq!(report["summary"]["updates_total"], 2, "{report}");
    let sources: Vec<&str> = file_named(&report, "Makefile")["updates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            u["source"]
                .as_str()
                .expect("every annotated entry carries a source")
        })
        .collect();
    assert!(
        sources.contains(&"pypi") && sources.contains(&"npm"),
        "each entry reports its own source, not the file's: {sources:?}"
    );
}

/// `--max-bump` is applied per line by `UpdateOptions::allows_bump` after the
/// downgrade guard. One capped line and one allowed line in the same file give
/// the negative and the positive control in one run.
#[tokio::test(flavor = "multi_thread")]
async fn max_bump_patch_caps_the_minor_line_and_allows_the_patch_line() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "minor-jump", &["1.0.0", "1.1.0"]).await;
    mount_pypi(&mock, "patch-jump", &["2.0.0", "2.0.1"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(
        &makefile,
        "A ?= 1.0.0  # upd: pypi minor-jump\nB ?= 2.0.0  # upd: pypi patch-jump\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &[
            "update",
            "--apply",
            "--max-bump",
            "patch",
            "--output",
            "json",
            ".",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "A ?= 1.0.0  # upd: pypi minor-jump\nB ?= 2.0.1  # upd: pypi patch-jump\n",
        "the minor jump is capped and the patch jump is not"
    );
    let report = json_of(&output);
    assert_eq!(report["summary"]["updates_total"], 1, "{report}");
    assert_eq!(
        file_named(&report, "Makefile")["updates"][0]["package"],
        "patch-jump"
    );
}

/// `2.6` is two release segments and `2.6.1` is three, so
/// `match_version_precision` (`src/version/mod.rs:26`) truncates the answer back
/// to `2.6`, which equals the current token and takes the equality exit.
/// `--full-precision` skips the truncation and the same registry answer becomes
/// a real change. The exit codes are the discriminator: 0 and 1 from identical
/// inputs.
#[tokio::test(flavor = "multi_thread")]
async fn precision_is_matched_unless_full_precision_is_asked_for() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6", "2.6.1"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(&makefile, "BAO ?= 2.6  # upd: pypi openbao-cli\n").unwrap();

    let checked = run(
        &dir,
        &mock.uri(),
        &["update", "--check", "--output", "json", "."],
    );
    assert_eq!(
        checked.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert_eq!(json_of(&checked)["summary"]["updates_total"], 0);

    let full = run(
        &dir,
        &mock.uri(),
        &[
            "update",
            "--check",
            "--full-precision",
            "--output",
            "json",
            ".",
        ],
    );
    assert_eq!(
        full.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&full.stderr)
    );
    let report = json_of(&full);
    assert_eq!(report["summary"]["updates_total"], 1, "{report}");
    assert_eq!(
        file_named(&report, "Makefile")["updates"][0]["latest"],
        "2.6.1"
    );

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "BAO ?= 2.6  # upd: pypi openbao-cli\n",
        "--check never writes"
    );
}

/// Ignore and pin lines both short-circuit before the lookup, so no
/// registry is mounted at all and `summary.errors == 0` is the proof that
/// neither line was resolved: a lookup against an empty mock returns 404, which
/// would surface as an error entry.
///
/// The pin writes `3.1`, not `3.1.4`: a pin goes through `choose_write_value`
/// like any other answer, and `2.0` is two release segments.
#[tokio::test(flavor = "multi_thread")]
async fn an_ignore_and_a_pin_from_updrc_reach_annotated_lines() {
    let mock = MockServer::start().await;

    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "ignore = [\"left-alone\"]\n\n[pin]\npinned-tool = \"3.1.4\"\n",
    )
    .unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(
        &makefile,
        "A ?= 1.0.0  # upd: pypi left-alone\nB ?= 2.0  # upd: pypi pinned-tool\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("requirements.txt"), "left-alone==1.0.0\n").unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = json_of(&output);
    assert_eq!(
        report["summary"]["errors"], 0,
        "no registry was contacted: {report}"
    );
    assert_eq!(report["summary"]["updates_total"], 0, "{report}");
    assert!(
        mock.received_requests()
            .await
            .expect("recording is on")
            .is_empty(),
        "ignore and pin gates both run before registry resolution"
    );

    let annotated = file_named(&report, "Makefile");
    assert_eq!(annotated["ignored"][0]["package"], "left-alone");
    assert_eq!(annotated["ignored"][0]["source"], "pypi");
    assert_eq!(annotated["pinned"][0]["package"], "pinned-tool");
    assert_eq!(annotated["pinned"][0]["pinned_to"], "3.1");
    assert_eq!(annotated["pinned"][0]["source"], "pypi");

    // The negative control: the same ignore, on a file whose ecosystem comes
    // from the file itself, carries no source at all. Without this an
    // implementation that stamps `Some("pypi")` on every entry passes.
    let requirements = file_named(&report, "requirements.txt");
    assert_eq!(requirements["ignored"][0]["package"], "left-alone");
    assert!(
        requirements["ignored"][0]["source"].is_null(),
        "source is omitted when the file has one ecosystem: {requirements}"
    );

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "A ?= 1.0.0  # upd: pypi left-alone\nB ?= 3.1  # upd: pypi pinned-tool\n"
    );
}

fn write_unsupported_source_fixture(dir: &TempDir) {
    std::fs::write(
        dir.path().join("Makefile"),
        "X ?= 1.0.0  # upd: cargo thing\n",
    )
    .unwrap();
}

/// `upd align` scans with `ParseWarnings::Print`, so the warning is printed as
/// it is discovered and never enters a report: exactly one owner.
#[tokio::test(flavor = "multi_thread")]
async fn align_prints_the_annotation_warning_on_stderr_and_not_in_its_report() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    write_unsupported_source_fixture(&dir);

    let output = run(&dir, &mock.uri(), &["align", "--output", "json", "."]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "align failed:\n{stderr}");
    assert_eq!(
        count_occurrences(&stderr, UNSUPPORTED_SOURCE_PREFIX),
        1,
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Makefile"),
        "the printed warning names its file: {stderr}"
    );
    assert!(
        !stdout.contains(UNSUPPORTED_SOURCE_PREFIX),
        "align's parse-only scan owns the warning on stderr, not in its document:\n{stdout}"
    );
}

/// `upd update --output json` collects the same warning into the file report.
/// Printing it to stderr as well would give it a second owner.
#[tokio::test(flavor = "multi_thread")]
async fn update_reports_the_annotation_warning_in_json_and_not_on_stderr() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    write_unsupported_source_fixture(&dir);

    let output = run(&dir, &mock.uri(), &["update", "--output", "json", "."]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = json_of(&output);
    let warnings = file_named(&report, "Makefile")["warnings"]
        .as_array()
        .unwrap();
    assert_eq!(warnings.len(), 1, "{report}");
    let warning = warnings[0].as_str().unwrap();
    assert!(warning.starts_with("line 1:"), "{warning}");
    assert!(warning.contains(UNSUPPORTED_SOURCE_PREFIX), "{warning}");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        count_occurrences(&stderr, UNSUPPORTED_SOURCE_PREFIX),
        0,
        "the report owns the warning in JSON mode:\n{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_unsupported_sources_are_reported_once_per_normalized_source() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("Makefile"),
        "A ?= 1.0.0  # upd: Cargo first\nB ?= 2.0.0  # upd: cargo second\nC ?= 3.0.0  # upd: Helm third\n",
    )
    .unwrap();

    let output = run(&dir, &mock.uri(), &["update", "--output", "json", "."]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = json_of(&output);
    let warnings = file_named(&report, "Makefile")["warnings"]
        .as_array()
        .unwrap();
    assert_eq!(warnings.len(), 2, "{report}");
    assert!(warnings[0].as_str().unwrap().starts_with("line 1:"));
    assert!(warnings[0].as_str().unwrap().contains("'cargo'"));
    assert!(warnings[1].as_str().unwrap().starts_with("line 3:"));
    assert!(warnings[1].as_str().unwrap().contains("'helm'"));
    assert_eq!(
        count_occurrences(
            &String::from_utf8_lossy(&output.stderr),
            UNSUPPORTED_SOURCE_PREFIX
        ),
        0,
        "the JSON report remains the warning owner"
    );
}

/// The text-mode half: `print_file_result` prints warnings to stderr
/// (`src/main.rs:4300-4380`), once.
#[tokio::test(flavor = "multi_thread")]
async fn update_prints_the_annotation_warning_once_in_text_mode() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    write_unsupported_source_fixture(&dir);

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--no-color", "--output", "text", "."],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "update failed:\n{stderr}");
    assert_eq!(
        count_occurrences(&stderr, UNSUPPORTED_SOURCE_PREFIX),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("Makefile"),
        "the printed warning names its file: {stderr}"
    );
}

/// The fourth invocation, and the only one that can see a duplicate. `--package`
/// is what sends `run_update` into the version-floor branch, where
/// `scan_packages` opens every discovered file a second time
/// (`src/main.rs:1391`). That call site uses `ParseWarnings::Suppress`; a
/// `Print` there prints the refusal the JSON report already owns, and no
/// invocation without `--package` can tell.
///
/// Naming a *different* package is the other half of the fixture's design. The
/// refusal is recorded by `scan_annotated` before the `--package`
/// gate, so `warnings[]` holds it even though line 1 is filtered out. An
/// implementation that recorded it after the filter reports zero here, and
/// naming `thing` itself would hide that difference.
#[tokio::test(flavor = "multi_thread")]
async fn a_package_filtered_run_reports_the_warning_once_and_never_on_stderr() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "other", &["2.0.0", "2.1.0"]).await;

    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("Makefile"),
        "X ?= 1.0.0  # upd: cargo thing\nY ?= 2.0.0  # upd: pypi other\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--package", "other", "--output", "json", "."],
    );
    let report = json_of(&output);

    // 1, not 2: the refusal is a warning, and only an `errors[]` entry would
    // make `decide_exit_code` return 2 (`src/lib.rs:45-53`).
    assert_eq!(
        output.status.code(),
        Some(1),
        "one pending update under a dry run:\n{report}"
    );
    assert_eq!(report["summary"]["updates_total"], 1, "{report}");

    let file = file_named(&report, "Makefile");
    let warnings = file["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{report}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .contains(UNSUPPORTED_SOURCE_PREFIX),
        "{report}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        count_occurrences(&stderr, UNSUPPORTED_SOURCE_PREFIX),
        0,
        "the floor branch's second scan re-printed a warning the report owns:\n{stderr}"
    );
}

/// The refusal is recorded by `scan_annotated` before any gate, so `--package
/// other` cannot hide it: a refused line is not in `scan.lines` at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_scan_abort_reports_the_annotation_warning_exactly_once_in_both_modes() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    write_unsupported_source_fixture(&dir);
    std::fs::write(dir.path().join("package.json"), "{ this is not json").unwrap();

    let json = run(
        &dir,
        &mock.uri(),
        &["update", "--package", "other", "--output", "json", "."],
    );
    let json_stderr = String::from_utf8_lossy(&json.stderr);
    assert_ne!(
        json.status.code(),
        Some(0),
        "the scan aborted:\n{json_stderr}"
    );
    assert!(
        json_stderr.contains("Error scanning files:"),
        "{json_stderr}"
    );
    assert!(
        String::from_utf8_lossy(&json.stdout).trim().is_empty(),
        "no report is emitted on the abort path: {}",
        String::from_utf8_lossy(&json.stdout)
    );
    assert_eq!(
        count_occurrences(&json_stderr, UNSUPPORTED_SOURCE_PREFIX),
        1,
        "Task 7's flush is the only carrier left:\n{json_stderr}"
    );

    // The same run in text mode. `print_file_result` already printed it during
    // the per-file loop, and the flush is gated on `json_mode`, so a second
    // occurrence here means that gate was dropped or inverted.
    let text = run(
        &dir,
        &mock.uri(),
        &[
            "update",
            "--package",
            "other",
            "--no-color",
            "--output",
            "text",
            ".",
        ],
    );
    let text_stderr = String::from_utf8_lossy(&text.stderr);
    assert_ne!(
        text.status.code(),
        Some(0),
        "the text-mode scan aborted:\n{text_stderr}"
    );
    assert!(
        text_stderr.contains("Error scanning files:"),
        "{text_stderr}"
    );
    assert!(
        String::from_utf8_lossy(&text.stdout).trim().is_empty(),
        "no text report is emitted on the abort path: {}",
        String::from_utf8_lossy(&text.stdout)
    );
    assert_eq!(
        count_occurrences(&text_stderr, UNSUPPORTED_SOURCE_PREFIX),
        1,
        "{text_stderr}"
    );
}

/// A write failure keeps the warnings and drops the updates.
/// `write_file_atomic` creates `.<name>.upd.tmp` beside the target
/// (`src/updater/mod.rs:103-138`); a directory at that path makes `File::create`
/// fail with EISDIR while everything before the write succeeds normally.
///
/// The io error text carries no path, so this asserts the shape rather than the
/// wording: exit 2, one error, no updates, bytes unchanged, warnings preserved.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_write_is_reported_as_an_error_and_changes_nothing() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    let original = "BAO ?= 2.6.1  # upd: pypi openbao-cli\nY ?= 1.0.0  # upd: cargo thing\n";
    std::fs::write(&makefile, original).unwrap();
    std::fs::create_dir(dir.path().join(".Makefile.upd.tmp")).unwrap();

    let failed = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert_eq!(
        failed.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );

    let report = json_of(&failed);
    assert_eq!(report["summary"]["errors"], 1, "{report}");
    assert_eq!(
        report["summary"]["updates_total"], 0,
        "an update that never reached disk is not reported as done: {report}"
    );
    let file = file_named(&report, "Makefile");
    assert!(file["updates"].as_array().unwrap().is_empty(), "{file}");
    assert_eq!(
        count_occurrences(&file["warnings"].to_string(), UNSUPPORTED_SOURCE_PREFIX),
        1,
        "the warnings collected before the failure survive it: {file}"
    );
    assert_eq!(std::fs::read_to_string(&makefile).unwrap(), original);

    // Positive control: the identical run with the obstacle removed. Without
    // it, an implementation that never writes anything passes everything above.
    std::fs::remove_dir(dir.path().join(".Makefile.upd.tmp")).unwrap();
    let ok = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(json_of(&ok)["summary"]["updates_total"], 1);
    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "BAO ?= 2.7.0  # upd: pypi openbao-cli\nY ?= 1.0.0  # upd: cargo thing\n"
    );
}

/// Guard 3: an annotated path never enters `changed_by_dir`, so the note names
/// the manifest that actually owns a lockfile. Without guard 1 the annotated
/// path is also in `updated_files`, its directory is processed too, and a
/// second note naming `Makefile` appears.
#[tokio::test(flavor = "multi_thread")]
async fn a_lockfile_note_names_the_manifest_not_the_annotated_file() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;
    mount_pypi(&mock, "ruff", &["0.1.0", "0.2.0"]).await;

    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("Makefile"),
        "BAO ?= 2.6.1  # upd: pypi openbao-cli\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(
        dir.path().join("sub").join("requirements.txt"),
        "ruff==0.1.0\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &[
            "update",
            "--apply",
            "--lock",
            "--no-color",
            "--output",
            "text",
            ".",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    assert_eq!(
        count_occurrences(&stderr, "note: no lockfile found for"),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("requirements.txt"), "{stderr}");
    assert!(
        !stderr.contains("Makefile"),
        "an annotated file is never a lockfile owner: {stderr}"
    );
}

/// Guards 1 and 3 together: with nothing but an annotated file changed,
/// `updated_files` is empty and the whole `--lock` block is skipped. Removing
/// guard 1 produces one note naming `Makefile` and fails this test.
#[tokio::test(flavor = "multi_thread")]
async fn the_lock_flag_is_silent_when_only_annotated_files_changed() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("Makefile"),
        "BAO ?= 2.6.1  # upd: pypi openbao-cli\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &[
            "update",
            "--apply",
            "--lock",
            "--no-color",
            "--output",
            "text",
            ".",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    assert!(
        !stderr.contains("note: no lockfile found"),
        "guard 1 keeps the annotated path out of updated_files: {stderr}"
    );
    assert!(!stdout.contains("Regenerating lockfiles"), "{stdout}");
    assert!(
        stdout.contains("openbao-cli"),
        "the update itself still happened: {stdout}"
    );
}

/// `apply_cooldown` resolves the policy through `registry.name()`
/// (`src/updater/mod.rs:632-680`) and the report resolves it again through
/// `entry_cooldown`. Those are independent code paths and both must land on
/// `pypi`, so this asserts both: that the hold-back happened at all, and that
/// the number attached to it is seven days rather than zero.
///
/// `1.3.1` is an hour old and inside the window; `1.3.0` is 31 days old and
/// outside it. The walk-down answer is precision-matched back to `1.3`, which is
/// what reaches the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_back_entry_carries_the_annotated_lines_own_cooldown() {
    let mock = MockServer::start().await;
    mount_pypi_body(
        &mock,
        "thing",
        pypi_releases_at(&[
            ("1.2", ago(Duration::days(400))),
            ("1.3.0", ago(Duration::days(31))),
            ("1.3.1", ago(Duration::hours(1))),
        ]),
    )
    .await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(&makefile, "X ?= 1.2  # upd: pypi thing\n").unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "[cooldown.ecosystem]\npypi = \"7d\"\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = json_of(&output);
    let held = &file_named(&report, "Makefile")["held_back"][0];
    assert_eq!(held["package"], "thing", "{report}");
    assert_eq!(held["current"], "1.2");
    assert_eq!(held["chosen"], "1.3");
    assert_eq!(held["skipped_latest"], "1.3.1");
    assert_eq!(
        held["cooldown_seconds"], 604_800,
        "entry_cooldown resolved the entry's source, not the file's: {held}"
    );
    assert_eq!(held["source"], "pypi");
    assert_eq!(report["summary"]["updates_total"], 1, "{report}");

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "X ?= 1.3  # upd: pypi thing\n"
    );
}

/// The `skipped_by_cooldown` half, in text mode. Two things are under test that
/// nothing else covers. The renderer resolves cooldown per entry inside the
/// loop; a version left outside it compiles and
/// prints `cooldown disabled` for every annotated entry. And the exit code:
/// `has_checkable_manifest_changes` deliberately excludes a cooldown-only skip
/// (`src/main.rs:677-685`), so `--check` must exit 0 here or this is a CI
/// failure that never clears.
#[tokio::test(flavor = "multi_thread")]
async fn a_cooldown_skip_renders_the_entrys_own_cooldown_in_text_mode() {
    let mock = MockServer::start().await;
    mount_pypi_body(
        &mock,
        "thing",
        pypi_releases_at(&[
            ("1.2", ago(Duration::days(400))),
            ("1.3.0", ago(Duration::hours(1))),
        ]),
    )
    .await;

    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("Makefile"), "X ?= 1.2  # upd: pypi thing\n").unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "[cooldown.ecosystem]\npypi = \"7d\"\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--check", "--no-color", "--output", "text", "."],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a cooldown-only skip is steady state, not pending work:\n{stdout}"
    );
    assert!(stdout.contains("Skipped thing"), "{stdout}");
    assert!(stdout.contains("1.3.0"), "{stdout}");
    assert!(
        stdout.contains("cooldown 7d"),
        "the renderer resolved the entry's ecosystem, not the file's:\n{stdout}"
    );
}

/// Three runs, because two of them assert different things. Run two is the
/// steady state after a successful write: no update entry, **no warning of any
/// kind**, and a byte-identical file under `--apply`. The warning half is the
/// load-bearing one. Reverse the gate order - downgrade check before the
/// equality exit - and the second run still writes nothing while
/// emitting `downgrade_warning` on every invocation forever, so an assertion
/// about bytes alone passes against exactly the defect this test exists to
/// catch. Run three is `--check`, which is what a CI job runs.
///
/// The lookup count rides along on the same fixture. `CachedRegistry` keys on
/// the bare package name (`src/cache.rs:233-240`) and `UPD_CACHE_DIR` makes the
/// store per-fixture, so the count across three runs is exact rather than
/// approximate. A 404 is not retried, so nothing else can inflate it either.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_run_is_silent_and_reuses_the_cached_lookup() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(&makefile, "BAO ?= 2.6.1  # upd: pypi openbao-cli\n").unwrap();

    let first = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(json_of(&first)["summary"]["updates_total"], 1);
    let written = "BAO ?= 2.7.0  # upd: pypi openbao-cli\n";
    assert_eq!(std::fs::read_to_string(&makefile).unwrap(), written);

    let second = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    let second_report = json_of(&second);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        second_report["summary"]["updates_total"], 0,
        "{second_report}"
    );
    let file = file_named(&second_report, "Makefile");
    // `warnings` is not `skip_serializing_if`'d (`src/output.rs:67`), so the key
    // is always present and this reads the empty vector rather than a missing
    // field. `downgrade_warning` would land here.
    assert!(
        file["warnings"].as_array().unwrap().is_empty(),
        "a steady-state run is silent, not merely non-writing: {second_report}"
    );
    assert_eq!(std::fs::read_to_string(&makefile).unwrap(), written);

    let third = run(
        &dir,
        &mock.uri(),
        &["update", "--check", "--output", "json", "."],
    );
    assert_eq!(
        third.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert_eq!(json_of(&third)["summary"]["updates_total"], 0);

    let requests = mock.received_requests().await.expect("recording is on");
    let lookups = requests
        .iter()
        .filter(|r| r.url.path() == "/pypi/openbao-cli/json")
        .count();
    assert_eq!(
        lookups,
        1,
        "runs two and three answered from disk; {} requests in total",
        requests.len()
    );
}

/// The steady state after a hold-back, which is the one a CI job lives in.
/// Current is now `1.3`; the walk-down still answers `1.3.0`, which
/// precision-matches back to `1.3` and takes the equality exit before
/// `held_back` is ever pushed. An implementation that pushes `held_back` first
/// makes `--check` exit 1 on every subsequent run, forever.
///
/// The `--check` before the write is the negative control, and without it the
/// exit-0 assertion at the end proves nothing: a `--check` that exited 0
/// unconditionally would pass. Before the write it must exit **1**, because an
/// `updated` and a `held_back` entry each count as pending work
/// (`has_checkable_manifest_changes`, `src/main.rs:677-685`) - which is also
/// where this differs from the cooldown *skip*, deliberately excluded from the
/// same predicate.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_run_after_a_cooldown_hold_back_is_quiet() {
    let mock = MockServer::start().await;
    mount_pypi_body(
        &mock,
        "thing",
        pypi_releases_at(&[
            ("1.2", ago(Duration::days(400))),
            ("1.3.0", ago(Duration::days(31))),
            ("1.3.1", ago(Duration::hours(1))),
        ]),
    )
    .await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(&makefile, "X ?= 1.2  # upd: pypi thing\n").unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "[cooldown.ecosystem]\npypi = \"7d\"\n",
    )
    .unwrap();

    let before = run(
        &dir,
        &mock.uri(),
        &["update", "--check", "--output", "json", "."],
    );
    let before_report = json_of(&before);
    assert_eq!(
        before.status.code(),
        Some(1),
        "an updated and a held_back entry are both pending work: {before_report}"
    );

    let first = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "X ?= 1.3  # upd: pypi thing\n"
    );

    let second = run(
        &dir,
        &mock.uri(),
        &["update", "--check", "--output", "json", "."],
    );
    let report = json_of(&second);
    assert_eq!(second.status.code(), Some(0), "{report}");
    assert_eq!(report["summary"]["updates_total"], 0, "{report}");
    assert!(
        report["summary"].get("held_back").is_none(),
        "held_back is omitted when zero, and must be zero here: {report}"
    );

    // Both vectors are `skip_serializing_if = "Vec::is_empty"`
    // (`src/output.rs:62-65`), so a missing key is an empty vector. The
    // equality exit happens before either record is pushed
    // (`src/updater/requirements.rs:472-482` is the precedent), and `warnings`
    // is always serialized (`:67`) so its emptiness reads directly.
    let file = file_named(&report, "Makefile");
    assert!(file.get("held_back").is_none(), "{report}");
    assert!(file.get("skipped_by_cooldown").is_none(), "{report}");
    assert!(
        file["warnings"].as_array().unwrap().is_empty(),
        "the equality exit is silent, and a downgrade_warning here would repeat forever: {report}"
    );
    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "X ?= 1.3  # upd: pypi thing\n"
    );
}

/// `downgrade_warning` is the shared helper every updater uses
/// (`src/updater/mod.rs:64-66`), and its exact wording has unit coverage; this
/// asserts the CLI consequences: a warning and not an error, no write, and exit
/// 0 rather than 1 or 2.
#[tokio::test(flavor = "multi_thread")]
async fn a_downgrade_is_refused_with_a_warning_through_the_cli() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "thing", &["1.0.0"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(&makefile, "X ?= 3.0.0  # upd: pypi thing\n").unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = json_of(&output);
    let file = file_named(&report, "Makefile");
    let warnings = file["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{report}");
    let warning = warnings[0].as_str().unwrap();
    assert!(
        warning.contains("thing") && warning.contains("1.0.0") && warning.contains("3.0.0"),
        "{warning}"
    );
    assert!(
        file["errors"].as_array().unwrap().is_empty(),
        "a refusal is not an error: {file}"
    );
    assert_eq!(report["summary"]["updates_total"], 0, "{report}");
    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "X ?= 3.0.0  # upd: pypi thing\n"
    );
}

/// A failed lookup is per line, not per file. The failing
/// annotation is deliberately FIRST, because an implementation that abandons
/// the file on the first error leaves the second line at 1.0.0 and fails on the
/// written bytes.
#[tokio::test(flavor = "multi_thread")]
async fn one_failing_lookup_does_not_stop_the_lines_below_it() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "present", &["1.0.0", "1.1.0"]).await;
    // `absent` is deliberately an unmounted npm package. Unlike PyPI, npm has
    // no system extra-index fallback such as `/etc/pip.conf`, so the intentional
    // miss cannot escape the explicit mock registry after its 404.

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(
        &makefile,
        "A ?= 1.0.0  # upd: npm absent\nB ?= 1.0.0  # upd: pypi present\n",
    )
    .unwrap();

    let uri = mock.uri();
    let output = run_env(
        &dir,
        &[
            ("NPM_REGISTRY", uri.as_str()),
            ("UV_INDEX_URL", uri.as_str()),
        ],
        &["update", "--apply", "--output", "json", "."],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = json_of(&output);
    assert_eq!(report["summary"]["errors"], 1, "{report}");
    assert_eq!(report["summary"]["updates_total"], 1, "{report}");
    let file = file_named(&report, "Makefile");
    assert!(
        file["errors"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("absent:"),
        "the error names the package it belongs to: {file}"
    );
    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "A ?= 1.0.0  # upd: npm absent\nB ?= 1.1.0  # upd: pypi present\n"
    );
}

/// The Go pseudo-version guard sits before the lookup. `GOPROXY` points at a
/// mock with nothing mounted, so `received_requests()`
/// being empty is direct evidence that no lookup was attempted; a guard placed
/// after the lookup would leave an error entry behind instead of a warning.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_pinned_go_line_is_refused_before_any_registry_call() {
    let mock = MockServer::start().await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    let original = "NET ?= v0.0.0-20240101000000-abcdef123456  # upd: go golang.org/x/net\n";
    std::fs::write(&makefile, original).unwrap();

    let uri = mock.uri();
    let output = run_env(
        &dir,
        &[("GOPROXY", uri.as_str())],
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = json_of(&output);
    let file = file_named(&report, "Makefile");
    let warnings = file["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{report}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .contains("commit-pinned Go version"),
        "{warnings:?}"
    );
    assert!(
        file["errors"].as_array().unwrap().is_empty(),
        "the guard runs before the lookup that would have failed: {file}"
    );
    assert!(
        mock.received_requests()
            .await
            .expect("recording is on")
            .is_empty(),
        "no registry was contacted"
    );
    assert_eq!(std::fs::read_to_string(&makefile).unwrap(), original);
}

/// The current token decides which registry method runs.
/// One prerelease line and one stable line in the same file, each with an
/// answer the other's rule would reject.
#[tokio::test(flavor = "multi_thread")]
async fn a_prerelease_line_tracks_prereleases_and_a_stable_line_does_not() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "pre-tool", &["1.0.0rc1", "1.0.0rc2"]).await;
    mount_pypi(&mock, "stable-tool", &["1.0.0", "2.0.0rc1"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    std::fs::write(
        &makefile,
        "A ?= 1.0.0rc1  # upd: pypi pre-tool\nB ?= 1.0.0  # upd: pypi stable-tool\n",
    )
    .unwrap();

    let output = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "A ?= 1.0.0rc2  # upd: pypi pre-tool\nB ?= 1.0.0  # upd: pypi stable-tool\n",
        "a prerelease current token opts into the prerelease track; a stable one does not"
    );
    assert_eq!(json_of(&output)["summary"]["updates_total"], 1);
}

/// The built-in name set does not bypass `.gitignore`. The walker
/// is built with `require_git(false)` (`src/updater/mod.rs:855-860`), so a
/// `.gitignore` with no `.git` directory beside it is still honoured.
#[tokio::test(flavor = "multi_thread")]
async fn a_gitignored_makefile_is_only_reached_with_no_ignore() {
    let mock = MockServer::start().await;
    mount_pypi(&mock, "openbao-cli", &["2.6.1", "2.7.0"]).await;

    let dir = TempDir::new().unwrap();
    let makefile = dir.path().join("Makefile");
    let original = "BAO ?= 2.6.1  # upd: pypi openbao-cli\n";
    std::fs::write(&makefile, original).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "Makefile\n").unwrap();

    let ignored = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--output", "json", "."],
    );
    assert!(
        ignored.status.success(),
        "{}",
        String::from_utf8_lossy(&ignored.stderr)
    );
    assert_eq!(
        json_of(&ignored)["summary"]["files_scanned"],
        0,
        "the annotated name set does not override .gitignore"
    );
    assert_eq!(std::fs::read_to_string(&makefile).unwrap(), original);

    let forced = run(
        &dir,
        &mock.uri(),
        &["update", "--apply", "--no-ignore", "--output", "json", "."],
    );
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert_eq!(json_of(&forced)["summary"]["files_scanned"], 1);
    assert_eq!(
        std::fs::read_to_string(&makefile).unwrap(),
        "BAO ?= 2.7.0  # upd: pypi openbao-cli\n"
    );
}

/// `Lang::Annotated` is in `build_audit_packages`' skip chain
/// (`src/main.rs:2615-2664`), which is an `if` chain the compiler does not
/// force. Without it the annotated dependency falls through to the `Ecosystem`
/// match below and hits `unreachable!("filtered above")`, so this test fails as
/// a panic rather than an assertion. The `requirements.txt` beside it is the
/// positive control: audit still runs and still checks one package.
#[tokio::test(flavor = "multi_thread")]
async fn an_annotated_file_contributes_nothing_to_audit() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"results":[{"vulns":[]}]}"#))
        .mount(&mock)
        .await;

    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("Makefile"),
        "BAO ?= 2.6.1  # upd: pypi openbao-cli\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("requirements.txt"), "ruff==0.1.0\n").unwrap();

    let uri = mock.uri();
    let output = run_env(
        &dir,
        &[("OSV_API_URL", uri.as_str())],
        &["audit", "--output", "json", "."],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = json_of(&output);
    assert_eq!(
        report["summary"]["packages_checked"], 1,
        "only requirements.txt contributes an audit package: {report}"
    );
    assert_eq!(report["summary"]["vulnerabilities"], 0, "{report}");
    assert_eq!(report["summary"]["errors"], 0, "{report}");
}
