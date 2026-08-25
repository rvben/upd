//! Integration tests for `--only-bump` and `--max-bump` filter flags.
//!
//! `--only-bump <LEVEL>[,<LEVEL>...]` restricts updates to those whose bump level
//! exactly matches one of the listed levels (e.g. `--only-bump minor,patch` skips major).
//!
//! `--max-bump <LEVEL>` applies a ceiling: only updates at or below that level are
//! included (e.g. `--max-bump minor` allows patch and minor, but not major).
//!
//! The two flags are mutually exclusive.

use std::fs;
use std::path::Path;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run_with_env(args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(upd_bin());
    cmd.args(args)
        .current_dir(cwd)
        .env("UPD_CACHE_DIR", cwd.join(".cache").to_str().unwrap());
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

// ── CLI parsing (unit-level, no binary I/O) ────────────────────────────────

#[test]
fn cli_only_bump_accepts_single_level() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--only-bump", "minor"]).unwrap();
    assert_eq!(cli.only_bump, vec![BumpLevel::Minor]);
    assert!(cli.max_bump.is_none());
}

#[test]
fn cli_only_bump_accepts_comma_separated() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--only-bump", "minor,patch"]).unwrap();
    assert_eq!(cli.only_bump, vec![BumpLevel::Minor, BumpLevel::Patch]);
}

#[test]
fn cli_only_bump_accepts_repeated() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--only-bump", "major", "--only-bump", "patch"]).unwrap();
    assert_eq!(cli.only_bump, vec![BumpLevel::Major, BumpLevel::Patch]);
}

#[test]
fn cli_max_bump_major_parses() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--max-bump", "major"]).unwrap();
    assert_eq!(cli.max_bump, Some(BumpLevel::Major));
    assert!(cli.only_bump.is_empty());
}

#[test]
fn cli_max_bump_minor_parses() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--max-bump", "minor"]).unwrap();
    assert_eq!(cli.max_bump, Some(BumpLevel::Minor));
}

#[test]
fn cli_max_bump_patch_parses() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--max-bump", "patch"]).unwrap();
    assert_eq!(cli.max_bump, Some(BumpLevel::Patch));
}

#[test]
fn cli_only_bump_and_max_bump_conflict() {
    use clap::Parser;
    use upd::cli::Cli;
    let result = Cli::try_parse_from(["upd", "--only-bump", "minor", "--max-bump", "minor"]);
    assert!(
        result.is_err(),
        "--only-bump and --max-bump must be mutually exclusive; got Ok"
    );
}

#[test]
fn cli_old_bump_flag_rejected() {
    use clap::Parser;
    use upd::cli::Cli;
    let result = Cli::try_parse_from(["upd", "--bump", "minor"]);
    assert!(
        result.is_err(),
        "--bump must not exist; it was renamed to --only-bump"
    );
}

// ── Filtering behaviour tests (via subprocess with mock registry) ──────────

/// `--max-bump minor` must skip a major update and treat the workspace as clean
/// (exit 0 under --check), because the only available update is major.
#[tokio::test]
async fn max_bump_minor_skips_major_update() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 2.0.0 - a major bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-2.0.0.tar.gz">requests-2.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    // --max-bump minor: the major bump from 1.0.0→2.0.0 must be excluded, so
    // --check sees no pending updates and must exit 0.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", "--max-bump", "minor", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "--max-bump minor must skip a major bump; --check should exit 0; stderr: {stderr}"
    );
}

/// `--max-bump minor` must allow a minor update (exit 1 under --check because
/// a pending minor update exists and is within the ceiling).
#[tokio::test]
async fn max_bump_minor_allows_minor_update() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 1.1.0 - a minor bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-1.1.0.tar.gz">requests-1.1.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    // --max-bump minor: a minor bump is within the ceiling, so --check must exit 1.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", "--max-bump", "minor", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 1,
        "--max-bump minor must include a minor bump; --check should exit 1; stderr: {stderr}"
    );
}

/// `--only-bump minor` skips a major bump so the workspace looks clean.
#[tokio::test]
async fn only_bump_minor_skips_major_update() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 2.0.0 - a major bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-2.0.0.tar.gz">requests-2.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    // --only-bump minor: only include exact minor bumps; the available update is
    // major so --check must see no pending updates and exit 0.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", "--only-bump", "minor", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "--only-bump minor must skip a major bump; --check should exit 0; stderr: {stderr}"
    );
}

/// `--apply --max-bump minor` must NOT write a major bump to disk. The bump
/// ceiling is a write-time guard, not merely a reporting filter: a capped-out
/// update must leave the file byte-for-byte unchanged.
#[tokio::test]
async fn apply_max_bump_minor_does_not_write_major() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 2.0.0 - a major bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-2.0.0.tar.gz">requests-2.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let manifest = tmp.path().join("requirements.txt");
    fs::write(&manifest, "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    let (_stdout, stderr, code) = run_with_env(
        &["--apply", "--no-cache", "--max-bump", "minor", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    let after = fs::read_to_string(&manifest).unwrap();
    assert_eq!(
        after, "requests==1.0.0\n",
        "--apply --max-bump minor must not write the major bump 1.0.0→2.0.0; file was modified to: {after:?}; stderr: {stderr}"
    );
    assert_eq!(
        code, 0,
        "clean apply (nothing within cap) should exit 0; stderr: {stderr}"
    );
}

/// `--apply --max-bump minor` MUST write an in-cap minor bump (guards against
/// the gate over-filtering and skipping allowed updates).
#[tokio::test]
async fn apply_max_bump_minor_writes_minor() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 1.1.0 - a minor bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-1.1.0.tar.gz">requests-1.1.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let manifest = tmp.path().join("requirements.txt");
    fs::write(&manifest, "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    let (_stdout, stderr, _code) = run_with_env(
        &["--apply", "--no-cache", "--max-bump", "minor", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    let after = fs::read_to_string(&manifest).unwrap();
    assert_eq!(
        after, "requests==1.1.0\n",
        "--apply --max-bump minor must write the in-cap minor bump 1.0.0→1.1.0; file is: {after:?}; stderr: {stderr}"
    );
}

/// A capped update must not be reported as an up-to-date dependency.
///
/// The ceiling decides what gets WRITTEN. Folding a capped update into the
/// up-to-date tally answers "is anything waiting for me?" with a confident no,
/// which is how an action four majors behind can sit in a repository whose
/// weekly check has printed a green tick every time.
///
/// The exit code deliberately stays 0: `--max-bump minor` in CI means "fail on
/// what I would take", and a major is not that. Only the text changes.
#[tokio::test]
async fn a_capped_update_is_not_reported_as_up_to_date() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 2.0.0, a major bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-2.0.0.tar.gz">requests-2.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    let (stdout, stderr, code) = run_with_env(
        &[
            "--check",
            "--no-cache",
            "--max-bump",
            "minor",
            "-o",
            "text",
            &path_str,
        ],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert!(
        !stdout.contains("all dependencies up to date"),
        "a dependency with a 2.0.0 release waiting is not up to date; stdout: {stdout}"
    );
    assert!(
        stdout.contains("held back by the bump ceiling"),
        "the run must say why 2.0.0 was not taken; stdout: {stdout}"
    );
    assert!(
        stdout.contains("requests") && stdout.contains("2.0.0"),
        "the held-back update must be named with the version waiting; stdout: {stdout}"
    );
    assert_eq!(
        code, 0,
        "a capped update must not fail the gate the ceiling exists to relax; stderr: {stderr}"
    );
}

/// The negative control for the test above: with nothing above the ceiling, the
/// up-to-date tick is still printed and no held-back line appears. Without this,
/// an implementation that never says "up to date" would pass.
#[tokio::test]
async fn a_genuinely_current_dependency_still_reports_up_to_date() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise exactly the pinned version: nothing is waiting.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-1.0.0.tar.gz">requests-1.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    let (stdout, stderr, code) = run_with_env(
        &[
            "--check",
            "--no-cache",
            "--max-bump",
            "minor",
            "-o",
            "text",
            &path_str,
        ],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert!(
        stdout.contains("all dependencies up to date"),
        "nothing is waiting, so the tick belongs here; stdout: {stdout}"
    );
    assert!(
        !stdout.contains("held back"),
        "nothing was held back; stdout: {stdout}"
    );
    assert_eq!(code, 0, "stderr: {stderr}");
}

/// A capped update is its own field in the JSON report, disjoint from `skipped`
/// and absent from the up-to-date accounting, so a machine reader can act on it.
#[tokio::test]
async fn a_capped_update_is_reported_in_json() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-2.0.0.tar.gz">requests-2.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    let (stdout, stderr, _code) = run_with_env(
        &[
            "--check",
            "--no-cache",
            "--max-bump",
            "minor",
            "-o",
            "json",
            &path_str,
        ],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}; stderr: {stderr}"));

    assert_eq!(
        report["summary"]["capped"], 1,
        "summary must count the capped update; report: {report}"
    );
    assert_eq!(
        report["summary"]["updates_total"], 0,
        "a capped update was not written, so it is not an update; report: {report}"
    );

    let capped = &report["files"][0]["capped"];
    assert_eq!(capped[0]["package"], "requests", "report: {report}");
    assert_eq!(capped[0]["current"], "1.0.0", "report: {report}");
    assert_eq!(capped[0]["available"], "2.0.0", "report: {report}");
    assert_eq!(
        capped[0]["bump"], "major",
        "naming the bump says what raising the ceiling would let through; report: {report}"
    );
}

/// A comparator spec whose anchor is not full semver (`>=1.0`) goes through
/// the range module, which records the spec verbatim, operator and all. The
/// report classifier therefore sees `>=1.0`, not a bare version. The ceiling
/// gated on the spec's lower bound; the level reported has to be that same
/// level, or the reader is told a patch ceiling would let this through when
/// raising it to minor is what it takes.
#[tokio::test]
async fn a_capped_comparator_range_reports_the_level_the_ceiling_gated_on() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/examplepkg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "examplepkg",
            "dist-tags": { "latest": "1.5.0" },
            "versions": {
                "1.0.0": { "name": "examplepkg", "version": "1.0.0" },
                "1.5.0": { "name": "examplepkg", "version": "1.5.0" }
            }
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("package.json"),
        r#"{"name": "t", "version": "1.0.0", "dependencies": {"examplepkg": ">=1.0"}}"#,
    )
    .unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    let (stdout, stderr, _code) = run_with_env(
        &[
            "--check",
            "--no-cache",
            "--max-bump",
            "patch",
            "-o",
            "json",
            &path_str,
        ],
        tmp.path(),
        &[("NPM_REGISTRY", &server.uri())],
    );

    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}; stderr: {stderr}"));

    let capped = &report["files"][0]["capped"];
    assert_eq!(capped[0]["package"], "examplepkg", "report: {report}");
    assert_eq!(
        capped[0]["bump"], "minor",
        "the ceiling compared 1.0.0 with 1.5.0, so the report must say minor; report: {report}"
    );
}

/// `--max-bump patch` must skip both a minor and a major update.
#[tokio::test]
async fn max_bump_patch_skips_minor_and_major_updates() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 1.1.0 - a minor bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-1.1.0.tar.gz">requests-1.1.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    // --max-bump patch: a minor bump is above the ceiling; --check exits 0.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", "--max-bump", "patch", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "--max-bump patch must skip a minor bump; --check should exit 0; stderr: {stderr}"
    );
}

/// A PEP 440 specifier is a set of clauses with no order, and setuptools and
/// pip write the ceiling first: `botocore<1.35.0,>=1.34.0` says what
/// `botocore>=1.34.0,<1.35.0` says. The clause naming the release installed
/// today, and the one an update rewrites, is the lower bound wherever the
/// author put it, so both orderings of one requirement must answer identically.
///
/// Reading the first clause instead compares the latest release against the
/// ceiling, where it reads as a downgrade: the package is warned about as
/// already ahead of the registry and never updated, so a repo pinned that way
/// never moves and its own output agrees. The forward ordering is the control that
/// proves the rig can see the update at all, and the ceiling left in place
/// afterwards proves the rewrite landed on the floor rather than the cap.
#[tokio::test]
async fn a_specifier_reads_its_floor_from_the_lower_bound() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    for (file, ceiling_first, forward) in [
        ("requirements.txt", "<2.0,>=1.0", ">=1.0,<2.0"),
        ("pyproject.toml", "<2.0,>=1.0", ">=1.0,<2.0"),
    ] {
        for spec in [ceiling_first, forward] {
            let server = MockServer::start().await;
            let html = r#"<!DOCTYPE html><html><body>
<a href="examplepkg-1.5.0.tar.gz">examplepkg-1.5.0.tar.gz</a>
</body></html>"#;
            Mock::given(method("GET"))
                .and(path_regex(r"^/simple/examplepkg/?$"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
                .mount(&server)
                .await;

            let tmp = tempfile::tempdir().unwrap();
            let body = if file == "pyproject.toml" {
                format!(
                    "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = [\"examplepkg{spec}\"]\n"
                )
            } else {
                format!("examplepkg{spec}\n")
            };
            fs::write(tmp.path().join(file), body).unwrap();

            let (stdout, stderr, code) = run_with_env(
                &["update", "--apply", "--format", "json", "--no-cache", "."],
                tmp.path(),
                &[("UV_INDEX_URL", &server.uri())],
            );

            let who = format!("{file} {spec:?}");
            assert_eq!(code, 0, "{who}: stdout: {stdout}\nstderr: {stderr}");

            let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
            let entry = json["files"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|f| f["updates"].as_array().cloned().unwrap_or_default())
                .find(|u| u["package"] == "examplepkg")
                .unwrap_or_else(|| panic!("{who}: no update in {json}"));
            assert_eq!(entry["current"], "1.0", "{who}: {entry}");
            assert_eq!(entry["latest"], "1.5", "{who}: {entry}");
            assert_eq!(entry["bump"], "minor", "{who}: {entry}");

            let after = fs::read_to_string(tmp.path().join(file)).unwrap();
            let line = after
                .lines()
                .find(|l| l.contains("examplepkg"))
                .unwrap_or_else(|| panic!("{who}: package gone from {after}"));
            assert!(
                line.contains(">=1.5"),
                "{who}: the floor is what moves: {line}"
            );
            assert!(
                line.contains("<2.0"),
                "{who}: the ceiling is not the update's business: {line}"
            );
        }
    }
}

/// Everything a requirement line carries besides its floor stays put: the
/// clauses the update did not move, an extras group, an environment marker and
/// a trailing comment. Splicing the new version over the floor's own span is
/// what keeps them; rebuilding the line from the package name and the new
/// version would take the marker and the comment with it, and rebuilding the
/// specifier from its leading operator would take the other clauses.
#[tokio::test]
async fn a_rewritten_requirement_line_keeps_everything_but_its_floor() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    for name in ["markerpkg", "extraspkg", "excludepkg"] {
        let html = format!(
            "<!DOCTYPE html><html><body>\n<a href=\"{name}-1.5.0.tar.gz\">{name}-1.5.0.tar.gz</a>\n</body></html>"
        );
        Mock::given(method("GET"))
            .and(path_regex(format!(r"^/simple/{name}/?$")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(html.into_bytes(), "text/html"))
            .mount(&server)
            .await;
    }

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("requirements.txt"),
        "markerpkg<2.0,>=1.0 ; python_version >= \"3.8\"\n\
         extraspkg[extra]<2.0,>=1.0\n\
         excludepkg !=1.2,>=1.0,<2.0  # keep me\n",
    )
    .unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["update", "--apply", "--format", "json", "--no-cache", "."],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let after = fs::read_to_string(tmp.path().join("requirements.txt")).unwrap();
    assert_eq!(
        after,
        "markerpkg<2.0,>=1.5 ; python_version >= \"3.8\"\n\
         extraspkg[extra]<2.0,>=1.5\n\
         excludepkg !=1.2,>=1.5,<2.0  # keep me\n",
        "only the floor may move"
    );
}

/// A Cargo requirement carries the same clause set, and the same floor rule:
/// `dupcrate = ">=1.0, <2.0"` is floored at `1.0` and capped below `2.0`
/// whichever clause was typed first. An update moves the floor and leaves every
/// other clause where the author wrote it, so the ceiling still stands
/// afterwards. Rebuilding the requirement from its leading operator instead
/// writes the new version out as the whole requirement, which drops the ceiling
/// silently: `--apply` exits 0 and nothing in the output says a bound was lost.
///
/// The registry offers a release above the ceiling as well, so the version that
/// lands is also the proof the ceiling was read: `1.9.0` is the newest release
/// the requirement admits, and `2.5.0` is the one it does not.
#[tokio::test]
async fn a_cargo_requirement_keeps_the_ceiling_its_floor_moves_under() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    for spec in [">=1.0, <2.0", "<2.0, >=1.0"] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/crates/dupcrate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "crate": {"max_stable_version": "2.5.0", "max_version": "2.5.0"},
                "versions": [
                    {"num": "2.5.0", "yanked": false, "created_at": "2024-01-01T00:00:00Z"},
                    {"num": "1.9.0", "yanked": false, "created_at": "2024-01-01T00:00:00Z"},
                    {"num": "1.0.0", "yanked": false, "created_at": "2024-01-01T00:00:00Z"},
                ]
            })))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            format!(
                "[package]\nname = \"t\"\nversion = \"0.1.0\"\n\n[dependencies]\ndupcrate = \"{spec}\"\n"
            ),
        )
        .unwrap();

        let (stdout, stderr, code) = run_with_env(
            &[
                "update",
                "--apply",
                "--no-lock",
                "--format",
                "json",
                "--no-cache",
                ".",
            ],
            tmp.path(),
            &[("CARGO_REGISTRIES_CRATES_IO_INDEX", &server.uri())],
        );

        let who = format!("Cargo.toml {spec:?}");
        assert_eq!(code, 0, "{who}: stdout: {stdout}\nstderr: {stderr}");

        let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let entry = json["files"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|f| f["updates"].as_array().cloned().unwrap_or_default())
            .find(|u| u["package"] == "dupcrate")
            .unwrap_or_else(|| panic!("{who}: no update in {json}"));
        assert_eq!(entry["current"], "1.0", "{who}: {entry}");
        assert_eq!(entry["latest"], "1.9", "{who}: {entry}");
        assert_eq!(entry["bump"], "minor", "{who}: {entry}");

        let after = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        let line = after
            .lines()
            .find(|l| l.starts_with("dupcrate"))
            .unwrap_or_else(|| panic!("{who}: package gone from {after}"));
        assert!(
            line.contains(">=1.9"),
            "{who}: the floor is what moves: {line}"
        );
        assert!(
            line.contains("<2.0"),
            "{who}: the ceiling the author wrote must survive the rewrite: {line}"
        );
    }
}
