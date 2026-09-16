//! A cooldown keeps the lockfile refresh as young as the manifest edit that
//! triggered it. The manifest only ever names releases older than the
//! cooldown, but the package manager that relocks it resolves on its own, so
//! without a gate of its own it takes whatever was published an hour ago.
//!
//! Tools with a native release-age setting are handed one: npm `--before`,
//! uv `--exclude-newer` (with the lock's existing entries exempted so nothing
//! is downgraded). Cargo has none, so upd moves a too-young crate back with
//! `cargo update --precise`. Everything else is checked after the refresh,
//! and a locked release inside the cooldown is reported in
//! `lockfile_cooldown[]`.
//!
//! The fake tools are POSIX shell scripts, so this file is unix-only.
#![cfg(unix)]

mod common;

use chrono::{DateTime, Duration, SubsecRound, Utc};
use common::{run_on_a_terminal, upd_bin};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Environment a developer machine may carry that changes what uv or npm
/// would be told; removed so every run starts from the same configuration.
const SCRUBBED_ENV: &[&str] = &[
    "UV_EXCLUDE_NEWER",
    "UV_EXCLUDE_NEWER_PACKAGE",
    "UV_CONFIG_FILE",
    "NPM_CONFIG_BEFORE",
    "npm_config_before",
];

fn run_with_env(args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(upd_bin());
    cmd.args(args).current_dir(cwd).env("NO_COLOR", "1");
    for key in SCRUBBED_ENV {
        cmd.env_remove(key);
    }
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

fn write_fake_tool(bin_dir: &Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = bin_dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_with(bin_dir: &Path) -> String {
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn days_ago(days: i64) -> DateTime<Utc> {
    Utc::now() - Duration::days(days)
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// A scratch layout: `<root>/bin` for fake tools, `<root>/project` as the
/// working directory, `<root>/xdg` as an empty user configuration home and
/// `<root>/tool.log` for whatever the fakes record.
struct Fixture {
    _tmp: tempfile::TempDir,
    bin: PathBuf,
    project: PathBuf,
    xdg: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let project = tmp.path().join("project");
        let xdg = tmp.path().join("xdg");
        for dir in [&bin, &project, &xdg] {
            fs::create_dir(dir).unwrap();
        }
        let log = tmp.path().join("tool.log");
        Self {
            _tmp: tmp,
            bin,
            project,
            xdg,
            log,
        }
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.project.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    /// A file outside the project, for fakes to copy from.
    fn stash(&self, name: &str, content: &str) -> PathBuf {
        let path = self.bin.parent().unwrap().join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn base_env(&self) -> Vec<(String, String)> {
        vec![
            ("PATH".to_string(), path_with(&self.bin)),
            ("FAKE_LOG".to_string(), self.log.display().to_string()),
            (
                "XDG_CONFIG_HOME".to_string(),
                self.xdg.display().to_string(),
            ),
            ("HOME".to_string(), self.xdg.display().to_string()),
        ]
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> (String, String, i32) {
        let mut env = self.base_env();
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        run_with_env(args, &self.project, &env)
    }

    fn logged(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// The value following `flag` in a logged argv line, for flags passed either
/// as `--flag=value` or as `--flag value`.
fn flag_value<'a>(line: &'a str, flag: &str) -> Option<&'a str> {
    let mut words = line.split(' ');
    while let Some(word) = words.next() {
        if let Some(value) = word.strip_prefix(&format!("{flag}=")) {
            return Some(value);
        }
        if word == flag {
            return words.next();
        }
    }
    None
}

fn assert_near_cutoff(value: &str, cooldown: Duration, context: &str) {
    let at = DateTime::parse_from_rfc3339(value)
        .unwrap_or_else(|e| panic!("{value} is not an RFC 3339 time ({e}): {context}"))
        .with_timezone(&Utc);
    let expected = Utc::now() - cooldown;
    let drift = (at - expected).num_seconds().abs();
    assert!(
        drift < 600,
        "{value} is {drift}s away from now minus the cooldown: {context}"
    );
}

fn json(stdout: &str, stderr: &str) -> serde_json::Value {
    serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("{e}\nstdout: {stdout}\nstderr: {stderr}"))
}

// ---------------------------------------------------------------- npm

/// A registry document carrying publish times, which cooldown selection reads.
fn npm_document(name: &str, releases: &[(&str, DateTime<Utc>)]) -> serde_json::Value {
    let latest = releases.last().unwrap().0;
    let versions: serde_json::Map<String, serde_json::Value> = releases
        .iter()
        .map(|(v, _)| {
            (
                v.to_string(),
                serde_json::json!({"name": name, "version": v}),
            )
        })
        .collect();
    let time: serde_json::Map<String, serde_json::Value> = releases
        .iter()
        .map(|(v, at)| (v.to_string(), serde_json::json!(iso(*at))))
        .collect();
    serde_json::json!({
        "name": name,
        "dist-tags": {"latest": latest},
        "versions": versions,
        "time": time,
    })
}

async fn mount_npm(server: &wiremock::MockServer, name: &str, releases: &[(&str, DateTime<Utc>)]) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/{name}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(npm_document(name, releases)),
        )
        .mount(server)
        .await;
}

/// An `npm` that answers `config list --json` with the file named by
/// `NPM_CONFIG_JSON`, records every other invocation and writes a lockfile.
const NPM_RECORDING: &str = "#!/bin/sh
case \"$1\" in
  --version) echo '10.9.0'; exit 0 ;;
  config) cat \"$NPM_CONFIG_JSON\"; exit $? ;;
esac
echo \"$*\" >> \"$FAKE_LOG\"
printf '{\"lockfileVersion\": 3, \"packages\": {}}\\n' > package-lock.json
exit 0
";

const NPM_LOCK: &str = "{\"lockfileVersion\": 3, \"packages\": {}}\n";

async fn npm_project(config_json: &str) -> (Fixture, wiremock::MockServer, PathBuf) {
    let server = wiremock::MockServer::start().await;
    mount_npm(
        &server,
        "examplepkg",
        &[("1.0.0", days_ago(400)), ("1.1.0", days_ago(30))],
    )
    .await;
    let fx = Fixture::new();
    write_fake_tool(&fx.bin, "npm", NPM_RECORDING);
    fx.write(
        "package.json",
        r#"{"name": "t", "version": "1.0.0", "dependencies": {"examplepkg": "1.0.0"}}"#,
    );
    fx.write("package-lock.json", NPM_LOCK);
    let config = fx.stash("npm-config.json", config_json);
    (fx, server, config)
}

#[tokio::test]
async fn an_npm_refresh_under_a_cooldown_passes_before() {
    let (fx, server, config) = npm_project(r#"{"before": null, "min-release-age": null}"#).await;

    let (stdout, stderr, code) = fx.run(
        &[
            "--apply",
            "--lock",
            "--min-age",
            "7d",
            "--no-cache",
            "--format",
            "json",
            ".",
        ],
        &[
            ("NPM_REGISTRY", &server.uri()),
            ("NPM_CONFIG_JSON", config.to_str().unwrap()),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let installs: Vec<String> = fx
        .logged()
        .into_iter()
        .filter(|l| l.starts_with("install"))
        .collect();
    assert_eq!(installs.len(), 1, "{installs:?}\nstderr: {stderr}");
    let before = flag_value(&installs[0], "--before")
        .unwrap_or_else(|| panic!("the refresh must be gated: {installs:?}"));
    assert_near_cutoff(before, Duration::days(7), &installs[0]);
    assert!(
        installs[0].contains("--package-lock-only"),
        "the gate joins the refresh command rather than replacing it: {installs:?}"
    );
}

/// The project's own stricter `before` is kept: the flag upd passes overrides
/// `.npmrc`, so passing the run's cutoff would loosen it.
#[tokio::test]
async fn an_npm_refresh_keeps_a_stricter_project_before() {
    let (fx, server, config) =
        npm_project(r#"{"before": "2024-06-01T00:00:00.000Z", "min-release-age": null}"#).await;

    let (stdout, stderr, code) = fx.run(
        &[
            "--apply",
            "--lock",
            "--min-age",
            "7d",
            "--no-cache",
            "--format",
            "json",
            ".",
        ],
        &[
            ("NPM_REGISTRY", &server.uri()),
            ("NPM_CONFIG_JSON", config.to_str().unwrap()),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let installs: Vec<String> = fx
        .logged()
        .into_iter()
        .filter(|l| l.starts_with("install"))
        .collect();
    assert_eq!(installs.len(), 1, "{installs:?}");
    assert_eq!(
        flag_value(&installs[0], "--before"),
        Some("2024-06-01T00:00:00Z"),
        "{installs:?}"
    );
}

/// A project `min-release-age` stricter than the run's cooldown wins the same way.
#[tokio::test]
async fn an_npm_refresh_keeps_a_stricter_project_min_release_age() {
    let (fx, server, config) = npm_project(r#"{"before": null, "min-release-age": 30}"#).await;
    mount_npm(
        &server,
        "examplepkg",
        &[("1.0.0", days_ago(400)), ("1.1.0", days_ago(60))],
    )
    .await;

    let (stdout, stderr, code) = fx.run(
        &[
            "--apply",
            "--lock",
            "--min-age",
            "7d",
            "--no-cache",
            "--format",
            "json",
            ".",
        ],
        &[
            ("NPM_REGISTRY", &server.uri()),
            ("NPM_CONFIG_JSON", config.to_str().unwrap()),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let installs: Vec<String> = fx
        .logged()
        .into_iter()
        .filter(|l| l.starts_with("install"))
        .collect();
    let before = flag_value(&installs[0], "--before").expect("gated");
    assert_near_cutoff(before, Duration::days(30), &installs[0]);
}

/// Control: without a cooldown the refresh command is exactly what it was.
#[tokio::test]
async fn an_npm_refresh_without_a_cooldown_is_not_gated() {
    let (fx, server, config) = npm_project(r#"{"before": null, "min-release-age": null}"#).await;

    let (stdout, stderr, code) = fx.run(
        &["--apply", "--lock", "--no-cache", "--format", "json", "."],
        &[
            ("NPM_REGISTRY", &server.uri()),
            ("NPM_CONFIG_JSON", config.to_str().unwrap()),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fx.logged(),
        vec!["install --package-lock-only --ignore-scripts".to_string()],
        "stderr: {stderr}"
    );
}

/// An interactive session refreshes lockfiles itself, and gates them the same way.
#[tokio::test]
async fn an_interactive_npm_refresh_passes_before() {
    let (fx, server, config) = npm_project(r#"{"before": null, "min-release-age": null}"#).await;
    let mut env = fx.base_env();
    env.push(("NPM_REGISTRY".to_string(), server.uri()));
    env.push(("NPM_CONFIG_JSON".to_string(), config.display().to_string()));
    env.push(("NO_COLOR".to_string(), "1".to_string()));
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let (code, output) = run_on_a_terminal(
        &[
            "--interactive",
            "--lock",
            "--min-age",
            "7d",
            "--no-cache",
            ".",
        ],
        &fx.project,
        &env,
        "y\n",
    );

    assert_eq!(code, 0, "{output}");
    let installs: Vec<String> = fx
        .logged()
        .into_iter()
        .filter(|l| l.starts_with("install"))
        .collect();
    assert_eq!(installs.len(), 1, "{installs:?}\n{output}");
    let before = flag_value(&installs[0], "--before")
        .unwrap_or_else(|| panic!("the interactive refresh must be gated: {installs:?}"));
    assert_near_cutoff(before, Duration::days(7), &installs[0]);
}

/// A lock-only `--package` floor relocks through the same command, so it is
/// gated too.
#[tokio::test]
async fn an_npm_floor_relock_passes_before() {
    let server = wiremock::MockServer::start().await;
    mount_npm(
        &server,
        "examplepkg",
        &[("1.2.0", days_ago(400)), ("1.3.0", days_ago(30))],
    )
    .await;
    let fx = Fixture::new();
    write_fake_tool(&fx.bin, "npm", NPM_RECORDING);
    let config = fx.stash(
        "npm-config.json",
        r#"{"before": null, "min-release-age": null}"#,
    );
    fx.write(
        "package.json",
        r#"{"name": "t", "version": "1.0.0", "dependencies": {"host": "1.0.0"}}"#,
    );
    fx.write(
        "package-lock.json",
        r#"{
  "name": "t", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
  "packages": {
    "": { "name": "t", "version": "1.0.0" },
    "node_modules/host": { "version": "1.0.0" },
    "node_modules/host/node_modules/examplepkg": { "version": "1.2.0" }
  }
}"#,
    );

    let (stdout, stderr, code) = fx.run(
        &[
            "update",
            "--package",
            "examplepkg",
            "--apply",
            "--min-age",
            "7d",
            "--no-cache",
            "--format",
            "json",
            ".",
        ],
        &[
            ("NPM_REGISTRY", &server.uri()),
            ("NPM_CONFIG_JSON", config.to_str().unwrap()),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let manifest = fs::read_to_string(fx.project.join("package.json")).unwrap();
    assert!(
        manifest.contains("overrides"),
        "the floor was written: {manifest}"
    );
    let installs: Vec<String> = fx
        .logged()
        .into_iter()
        .filter(|l| l.starts_with("install"))
        .collect();
    assert_eq!(
        installs.len(),
        1,
        "{installs:?}\nstdout: {stdout}\nstderr: {stderr}"
    );
    let before = flag_value(&installs[0], "--before")
        .unwrap_or_else(|| panic!("the floor relock must be gated: {installs:?}"));
    assert_near_cutoff(before, Duration::days(7), &installs[0]);
}

// ---------------------------------------------------------------- uv

const PYPROJECT: &str =
    "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = [\"requests==2.31.0\"]\n";

async fn mount_pypi(server: &wiremock::MockServer, name: &str, releases: &[(&str, DateTime<Utc>)]) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/simple/{name}/")))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(server)
        .await;
    let releases: serde_json::Map<String, serde_json::Value> = releases
        .iter()
        .map(|(v, at)| {
            (
                v.to_string(),
                serde_json::json!([{"yanked": false, "upload_time_iso_8601": iso(*at)}]),
            )
        })
        .collect();
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/pypi/{name}/json")))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"releases": releases})),
        )
        .mount(server)
        .await;
}

fn uv_entry(index: &str, name: &str, version: &str, uploaded: DateTime<Utc>) -> String {
    format!(
        "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = {{ registry = \"{index}\" }}\nsdist = {{ url = \"https://files.example/{name}-{version}.tar.gz\", hash = \"sha256:00\", size = 1, upload-time = \"{}\" }}\nwheels = [\n    {{ url = \"https://files.example/{name}-{version}-py3-none-any.whl\", hash = \"sha256:00\", size = 1, upload-time = \"{}\" }},\n]\n",
        iso(uploaded - Duration::minutes(5)),
        iso(uploaded),
    )
}

/// A `uv` that supports `--exclude-newer-package` and records every `lock`.
///
/// A gated `lock` writes `FAKE_LOCK` with its cutoff recorded under
/// `[options]`, as uv does, or fails the way an unsatisfiable resolution does
/// when `FAKE_REFUSE_GATE` is set. A plain `lock` resolves afresh, writing
/// `FAKE_FRESH_LOCK`, when `uv.lock` records a cutoff the command no longer
/// carries (uv 0.9 discards such a lockfile) or when `FAKE_RERESOLVE` is set,
/// and otherwise keeps the lockfile it finds.
const UV_RECORDING: &str = r#"#!/bin/sh
case "$1" in
  --version) echo 'uv 0.9.0'; exit 0 ;;
esac
if [ "$1" = lock ] && [ "$2" = --help ]; then
  echo '      --exclude-newer-package <EXCLUDE_NEWER_PACKAGE>'
  exit 0
fi
echo "$*" >> "$FAKE_LOG"
case "$*" in
  *--exclude-newer*)
    if [ -n "$FAKE_REFUSE_GATE" ]; then
      printf 'partial-resolution = true\n' > uv.lock
      echo 'error: No solution found when resolving dependencies' >&2
      exit 1
    fi
    {
      head -n 1 "$FAKE_LOCK"
      printf '\n[options]\nexclude-newer = "2000-01-01T00:00:00Z"\n'
      case "$*" in
        *--exclude-newer-package*)
          printf '\n[options.exclude-newer-package]\nrequests = "2000-01-01T00:00:00Z"\n' ;;
      esac
      tail -n +2 "$FAKE_LOCK"
    } > uv.lock
    exit 0 ;;
esac
if [ -n "$FAKE_SEEN" ]; then
  cat uv.lock > "$FAKE_SEEN"
fi
if [ -n "$FAKE_RERESOLVE" ] || grep -q exclude-newer uv.lock; then
  cat "$FAKE_FRESH_LOCK" > uv.lock
fi
exit 0
"#;

struct UvRun {
    fx: Fixture,
    stdout: String,
    stderr: String,
    code: i32,
    /// When the locked `requests` release was uploaded, whose exemption
    /// keeps the gated pass from moving it.
    locked_upload: DateTime<Utc>,
}

async fn uv_refresh(extra: &[(&str, &str)]) -> UvRun {
    uv_refresh_locking(("2.32.0", days_ago(30)), false, extra).await
}

/// A `--lock` refresh whose gated pass locks `gated`, the `requests` release
/// and its upload time. The mock index is configured through the environment,
/// or with `declared` only in `[[tool.uv.index]]`.
async fn uv_refresh_locking(
    gated: (&str, DateTime<Utc>),
    declared: bool,
    extra: &[(&str, &str)],
) -> UvRun {
    let server = wiremock::MockServer::start().await;
    mount_pypi(
        &server,
        "requests",
        &[
            ("2.31.0", days_ago(2)),
            ("2.31.5", days_ago(3)),
            ("2.32.0", days_ago(30)),
            ("2.33.0", days_ago(1)),
        ],
    )
    .await;
    mount_pypi(&server, "idna", &[("3.10", days_ago(300))]).await;
    let fx = Fixture::new();
    write_fake_tool(&fx.bin, "uv", UV_RECORDING);
    let index = format!("{}/simple", server.uri());
    if declared {
        fx.write(
            "pyproject.toml",
            &format!("{PYPROJECT}\n[[tool.uv.index]]\nname = \"mock\"\nurl = \"{index}\"\n"),
        );
    } else {
        fx.write("pyproject.toml", PYPROJECT);
    }
    // Uploaded with a fractional second, so the exemption has to round up to
    // keep the file itself inside it.
    let locked_upload = days_ago(2).trunc_subsecs(0) + Duration::milliseconds(250);
    let lock_before = format!(
        "version = 1\n{}{}",
        uv_entry(&index, "requests", "2.31.0", locked_upload),
        uv_entry(&index, "idna", "3.10", days_ago(300)),
    );
    fx.write("uv.lock", &lock_before);
    let lock_after = format!(
        "version = 1\n{}{}",
        uv_entry(&index, "requests", gated.0, gated.1),
        uv_entry(&index, "idna", "3.10", days_ago(300)),
    );
    let after = fx.stash("uv.lock.after", &lock_after);
    // What resolving without a cutoff locks: a release inside the cooldown.
    let lock_fresh = format!(
        "version = 1\n{}{}",
        uv_entry(&index, "requests", "2.33.0", days_ago(1)),
        uv_entry(&index, "idna", "3.10", days_ago(300)),
    );
    let fresh = fx.stash("uv.lock.fresh", &lock_fresh);

    let mut env = vec![
        ("FAKE_LOCK", after.display().to_string()),
        ("FAKE_FRESH_LOCK", fresh.display().to_string()),
    ];
    if !declared {
        env.push(("UV_INDEX_URL", server.uri()));
    }
    env.extend(extra.iter().map(|(k, v)| (*k, v.to_string())));
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (stdout, stderr, code) = fx.run(
        &[
            "--apply",
            "--lock",
            "--min-age",
            "7d",
            "--no-cache",
            "--format",
            "json",
            ".",
        ],
        &env,
    );
    UvRun {
        fx,
        stdout,
        stderr,
        code,
        locked_upload,
    }
}

#[tokio::test]
async fn a_uv_refresh_under_a_cooldown_resolves_gated_then_removes_the_cutoff() {
    let run = uv_refresh(&[]).await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    let locks = run.fx.logged();
    assert_eq!(locks.len(), 2, "a gated pass, then a plain one: {locks:?}");

    let gated = &locks[0];
    let cutoff = flag_value(gated, "--exclude-newer")
        .unwrap_or_else(|| panic!("the first pass must be gated: {locks:?}"));
    assert_near_cutoff(cutoff, Duration::days(7), gated);
    let rounded_up = (run.locked_upload.trunc_subsecs(0) + Duration::seconds(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    assert_eq!(
        flag_value(gated, "--exclude-newer-package"),
        Some(format!("requests={rounded_up}").as_str()),
        "the locked release younger than the cutoff is exempt at its own upload time: {gated}"
    );
    assert_eq!(
        gated.matches("--exclude-newer-package").count(),
        1,
        "a locked release older than the cutoff needs no exemption: {gated}"
    );

    assert_eq!(locks[1], "lock", "the second pass is plain: {locks:?}");

    let lock = fs::read_to_string(run.fx.project.join("uv.lock")).unwrap();
    assert!(
        lock.contains("version = \"2.32.0\"") && !lock.contains("version = \"2.33.0\""),
        "the gated resolution is what stays locked: {lock}"
    );
    assert!(
        !lock.contains("exclude-newer"),
        "uv.lock must not keep the gate's cutoff, which `uv lock --locked` rejects: {lock}"
    );
    let report = json(&run.stdout, &run.stderr);
    assert!(report.get("lockfile_cooldown").is_none(), "{report}");
    assert!(report.get("warnings").is_none(), "{report}");
}

/// An exemption admits every release of its package up to the exempted
/// time, so a gated pass that used one can lock a release inside the
/// cooldown; what it introduced is read back.
#[tokio::test]
async fn a_uv_refresh_whose_exemption_admits_a_young_release_reports_it() {
    let run = uv_refresh_locking(("2.31.5", days_ago(3)), false, &[]).await;
    exempted_requests_reported(&run);
}

/// A uv.lock entry from an index the project declares is dated by that
/// index, as its dependencies are looked up there.
#[tokio::test]
async fn a_uv_entry_from_a_declared_index_is_checked_against_it() {
    let run = uv_refresh_locking(("2.31.5", days_ago(3)), true, &[]).await;
    exempted_requests_reported(&run);
}

/// The gated pass ran with an exemption, no warning was raised, and the
/// young requests 2.31.5 it admitted is reported.
fn exempted_requests_reported(run: &UvRun) {
    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    let locks = run.fx.logged();
    assert_eq!(locks.len(), 2, "{locks:?}");
    assert!(locks[0].contains("--exclude-newer-package"), "{locks:?}");
    let report = json(&run.stdout, &run.stderr);
    assert!(
        report.get("warnings").is_none(),
        "the gate held, so no warning: {report}"
    );
    let entries = report["lockfile_cooldown"]
        .as_array()
        .unwrap_or_else(|| panic!("the young release must be reported: {report}"));
    assert_eq!(entries.len(), 1, "{report}");
    assert_eq!(entries[0]["package"], "requests", "{report}");
    assert_eq!(entries[0]["version"], "2.31.5", "{report}");
}

/// The lockfile a young release reached, as `lockfile_cooldown[]` reports it.
fn young_requests_reported(report: &serde_json::Value) {
    let entries = report["lockfile_cooldown"]
        .as_array()
        .unwrap_or_else(|| panic!("the young release must be reported: {report}"));
    assert_eq!(entries.len(), 1, "{report}");
    assert_eq!(entries[0]["package"], "requests", "{report}");
    assert_eq!(entries[0]["version"], "2.33.0", "{report}");
}

/// A gated resolution that fails is not the end of the refresh: the lockfile
/// is put back, the plain refresh runs, and the run says the gate was dropped.
/// The refused pass leaves a partial `uv.lock` behind, so what the plain
/// refresh is given records whether it was put back.
#[tokio::test]
async fn a_uv_refresh_the_gate_cannot_satisfy_reruns_without_it_and_says_so() {
    let run = uv_refresh(&[
        ("FAKE_REFUSE_GATE", "1"),
        ("FAKE_RERESOLVE", "1"),
        ("FAKE_SEEN", "seen.lock"),
    ])
    .await;

    let seen = fs::read_to_string(run.fx.project.join("seen.lock"))
        .expect("the plain refresh records the lockfile it was given");
    assert!(
        !seen.contains("partial-resolution") && seen.contains("requests"),
        "the plain refresh runs on the lockfile put back, not what the refused pass left: {seen}"
    );

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    let locks = run.fx.logged();
    assert_eq!(locks.len(), 2, "{locks:?}");
    assert!(locks[0].contains("--exclude-newer"), "{locks:?}");
    assert_eq!(locks[1], "lock", "{locks:?}");
    assert!(
        fs::read_to_string(run.fx.project.join("uv.lock"))
            .unwrap()
            .contains("version = \"2.33.0\""),
        "the plain refresh's lockfile is kept"
    );
    let report = json(&run.stdout, &run.stderr);
    let warnings = report["warnings"].to_string();
    assert!(
        warnings.contains("uv.lock") && warnings.contains("without the 7d cooldown"),
        "the dropped gate must be named: {report}"
    );
    assert!(
        warnings.contains("No solution found"),
        "the gated failure is quoted: {report}"
    );
    young_requests_reported(&report);
}

/// A uv that resolves again once the cutoff is gone undoes the gate, so the
/// refresh is named as not keeping to it and what it locked is checked.
#[tokio::test]
async fn a_uv_that_resolves_again_without_the_cutoff_is_named_and_checked() {
    let run = uv_refresh(&[("FAKE_RERESOLVE", "1")]).await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    let locks = run.fx.logged();
    assert_eq!(locks.len(), 2, "{locks:?}");
    let report = json(&run.stdout, &run.stderr);
    let warnings = report["warnings"].to_string();
    assert!(
        warnings.contains("uv.lock")
            && warnings.contains("without the 7d cooldown")
            && warnings.contains("requests 2.32.0 to 2.33.0"),
        "the undone gate must be named with what moved: {report}"
    );
    young_requests_reported(&report);
}

// ---------------------------------------------------------------- poetry

fn poetry_lock(index: &str, entries: &[(&str, &str)]) -> String {
    let mut lock = String::new();
    for (name, version) in entries {
        lock.push_str(&format!(
            "[[package]]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"\"\noptional = false\npython-versions = \">=3.8\"\nfiles = []\n\n[package.source]\ntype = \"legacy\"\nurl = \"{index}\"\nreference = \"mock\"\n\n"
        ));
    }
    lock.push_str(
        "[metadata]\nlock-version = \"2.1\"\npython-versions = \">=3.12\"\ncontent-hash = \"0\"\n",
    );
    lock
}

const POETRY_RECORDING: &str = "#!/bin/sh
case \"$1\" in
  --version) echo 'Poetry (version 2.1.3)'; exit 0 ;;
esac
echo \"$*\" >> \"$FAKE_LOG\"
cat \"$FAKE_LOCK\" > poetry.lock
exit 0
";

/// Poetry has no release-age setting, so its refresh is checked afterwards:
/// the release it newly locked inside the cooldown is reported, the release
/// outside it is not.
async fn poetry_refresh(urllib3_published: DateTime<Utc>) -> serde_json::Value {
    let server = wiremock::MockServer::start().await;
    mount_pypi(
        &server,
        "requests",
        &[("2.31.0", days_ago(400)), ("2.32.0", days_ago(30))],
    )
    .await;
    mount_pypi(
        &server,
        "urllib3",
        &[("2.4.0", days_ago(300)), ("2.5.0", urllib3_published)],
    )
    .await;
    let fx = Fixture::new();
    write_fake_tool(&fx.bin, "poetry", POETRY_RECORDING);
    fx.write("pyproject.toml", PYPROJECT);
    let index = format!("{}/simple", server.uri());
    fx.write(
        "poetry.lock",
        &poetry_lock(&index, &[("requests", "2.31.0"), ("urllib3", "2.4.0")]),
    );
    let after = fx.stash(
        "poetry.lock.after",
        &poetry_lock(&index, &[("requests", "2.32.0"), ("urllib3", "2.5.0")]),
    );

    let (stdout, stderr, code) = fx.run(
        &[
            "--apply",
            "--lock",
            "--min-age",
            "7d",
            "--no-cache",
            "--format",
            "json",
            ".",
        ],
        &[
            ("UV_INDEX_URL", &server.uri()),
            ("FAKE_LOCK", after.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(fx.logged(), vec!["lock".to_string()], "stderr: {stderr}");
    json(&stdout, &stderr)
}

#[tokio::test]
async fn a_poetry_refresh_reports_a_release_locked_inside_the_cooldown() {
    let published = days_ago(1);
    let report = poetry_refresh(published).await;

    let entries = report["lockfile_cooldown"]
        .as_array()
        .unwrap_or_else(|| panic!("no lockfile_cooldown in {report}"));
    assert_eq!(
        entries.len(),
        1,
        "only the young release is reported: {report}"
    );
    let entry = &entries[0];
    assert!(
        entry["lockfile"].as_str().unwrap().ends_with("poetry.lock"),
        "{entry}"
    );
    assert_eq!(entry["package"], "urllib3", "{entry}");
    assert_eq!(entry["version"], "2.5.0", "{entry}");
    assert_eq!(entry["cooldown"], "7d", "{entry}");
    let reported = DateTime::parse_from_rfc3339(entry["published_at"].as_str().unwrap()).unwrap();
    assert_eq!(reported.timestamp(), published.timestamp(), "{entry}");
    assert!(
        report["summary"]["warnings"].as_u64().unwrap() >= 1,
        "a young locked release counts as a warning: {report}"
    );
    assert_eq!(report["summary"]["errors"], 0, "{report}");
}

#[tokio::test]
async fn a_poetry_refresh_outside_the_cooldown_reports_nothing() {
    let report = poetry_refresh(days_ago(10)).await;

    assert!(report.get("lockfile_cooldown").is_none(), "{report}");
    assert!(report.get("warnings").is_none(), "{report}");
}

// ---------------------------------------------------------------- cargo

const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

fn cargo_lock(entries: &[(&str, &str)]) -> String {
    let mut lock = String::from(
        "# This file is automatically @generated by Cargo.\nversion = 4\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\n",
    );
    for (name, version) in entries {
        lock.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"{CRATES_IO}\"\nchecksum = \"00\"\n"
        ));
    }
    lock
}

async fn mount_crate(
    server: &wiremock::MockServer,
    name: &str,
    releases: &[(&str, DateTime<Utc>)],
) {
    let versions: Vec<serde_json::Value> = releases
        .iter()
        .map(|(v, at)| serde_json::json!({"num": v, "yanked": false, "created_at": iso(*at)}))
        .collect();
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/api/v1/crates/{name}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "crate": {"max_stable_version": releases.last().unwrap().0},
                "versions": versions,
            })),
        )
        .mount(server)
        .await;
}

/// A `cargo` whose `update -p clap` locks the young 4.6.7 pair, and whose
/// `--precise 4.6.6` for either crate writes `FAKE_HELD_LOCK`, which moves
/// the pair back as real cargo does for crates that pin each other exactly.
/// With `FAKE_REFUSE_PRECISE` set, `--precise` fails instead; with
/// `FAKE_PRECISE_NOOP` set, it succeeds without touching the lockfile; with
/// `FAKE_LOCK_READONLY` set, it leaves the lockfile it wrote read-only; with
/// `FAKE_SECOND_HELD_LOCK` set, every `--precise` after the first writes that
/// lockfile instead, as cargo does when one hold moves a crate an earlier
/// hold moved. The floor a `--package clap_builder` run writes locks
/// `FAKE_FLOOR_LOCK`.
const CARGO_RECORDING: &str = "#!/bin/sh
case \"$1\" in
  --version) echo 'cargo 1.96.0 (00 2026-01-01)'; exit 0 ;;
esac
echo \"$*\" >> \"$FAKE_LOG\"
case \"$*\" in
  'update -p clap') cat \"$FAKE_FRESH_LOCK\" > Cargo.lock ;;
  'update -p clap_builder@4.5.0 --precise '*) cat \"$FAKE_FLOOR_LOCK\" > Cargo.lock ;;
  *--precise*)
    if [ -n \"$FAKE_REFUSE_PRECISE\" ]; then
      echo 'error: failed to select a version for the requirement' >&2
      exit 101
    fi
    if [ -z \"$FAKE_PRECISE_NOOP\" ]; then
      if [ -n \"$FAKE_SECOND_HELD_LOCK\" ] && [ -f .fake-held ]; then
        cat \"$FAKE_SECOND_HELD_LOCK\" > Cargo.lock
      else
        cat \"$FAKE_HELD_LOCK\" > Cargo.lock
      fi
      : > .fake-held
    fi
    if [ -n \"$FAKE_LOCK_READONLY\" ]; then
      chmod 444 Cargo.lock
    fi ;;
  *) echo \"unexpected cargo $*\" >&2; exit 101 ;;
esac
exit 0
";

const CLAP_BEFORE: &[(&str, &str)] = &[("clap", "4.5.0"), ("clap_builder", "4.5.0")];
const CLAP_FRESH: &[(&str, &str)] = &[("clap", "4.6.7"), ("clap_builder", "4.6.7")];
const CLAP_HELD: &[(&str, &str)] = &[("clap", "4.6.6"), ("clap_builder", "4.6.6")];

struct CargoRun {
    fx: Fixture,
    stdout: String,
    stderr: String,
    code: i32,
}

async fn cargo_refresh(extra: &[(&str, &str)]) -> CargoRun {
    cargo_refresh_from("", CLAP_BEFORE, CLAP_HELD, extra).await
}

/// A `--lock` refresh of a project with `config` as `.updrc.toml`, whose
/// `Cargo.lock` starts at `before`, with `--precise` writing `held`.
async fn cargo_refresh_from(
    config: &str,
    before: &[(&str, &str)],
    held: &[(&str, &str)],
    extra: &[(&str, &str)],
) -> CargoRun {
    cargo_refresh_locking(config, before, CLAP_FRESH, held, None, extra).await
}

/// A `--lock` refresh whose refresh locks `fresh`, with the first `--precise`
/// writing `held` and every later one `second`, when given.
async fn cargo_refresh_locking(
    config: &str,
    before: &[(&str, &str)],
    fresh: &[(&str, &str)],
    held: &[(&str, &str)],
    second: Option<&[(&str, &str)]>,
    extra: &[(&str, &str)],
) -> CargoRun {
    let server = wiremock::MockServer::start().await;
    let releases = [
        ("4.5.0", days_ago(400)),
        ("4.6.6", days_ago(10)),
        ("4.6.7", days_ago(1)),
    ];
    mount_crate(&server, "clap", &releases).await;
    mount_crate(&server, "clap_builder", &releases).await;
    mount_crate(
        &server,
        "anstream",
        &[("0.6.8", days_ago(30)), ("0.6.9", days_ago(1))],
    )
    .await;
    let fx = Fixture::new();
    write_fake_tool(&fx.bin, "cargo", CARGO_RECORDING);
    fx.write(
        "Cargo.toml",
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nclap = \"4.5.0\"\n",
    );
    fx.write(".updrc.toml", config);
    fx.write("Cargo.lock", &cargo_lock(before));
    let fresh = fx.stash("fresh.lock", &cargo_lock(fresh));
    let held = fx.stash("held.lock", &cargo_lock(held));
    let second = second.map(|entries| fx.stash("second.lock", &cargo_lock(entries)));

    let index = format!("sparse+{}/index/", server.uri());
    let mut env = vec![
        ("CARGO_REGISTRIES_CRATES_IO_INDEX", index),
        ("FAKE_FRESH_LOCK", fresh.display().to_string()),
        ("FAKE_HELD_LOCK", held.display().to_string()),
    ];
    if let Some(second) = &second {
        env.push(("FAKE_SECOND_HELD_LOCK", second.display().to_string()));
    }
    env.extend(extra.iter().map(|(k, v)| (*k, v.to_string())));
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (stdout, stderr, code) = fx.run(
        &[
            "--apply",
            "--lock",
            "--min-age",
            "7d",
            "--no-cache",
            "--format",
            "json",
            ".",
        ],
        &env,
    );
    CargoRun {
        fx,
        stdout,
        stderr,
        code,
    }
}

#[tokio::test]
async fn a_cargo_refresh_holds_a_crate_published_inside_the_cooldown() {
    let run = cargo_refresh(&[]).await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        fs::read_to_string(run.fx.project.join("Cargo.toml"))
            .unwrap()
            .lines()
            .find(|l| l.starts_with("clap"))
            .unwrap(),
        "clap = \"4.6.6\"",
        "the manifest takes the newest release outside the cooldown"
    );
    assert_eq!(
        run.fx.logged(),
        vec![
            "update -p clap".to_string(),
            "update -p clap@4.6.7 --precise 4.6.6".to_string(),
        ],
        "stderr: {}",
        run.stderr
    );
    let lock = fs::read_to_string(run.fx.project.join("Cargo.lock")).unwrap();
    assert!(
        !lock.contains("4.6.7") && lock.contains("version = \"4.6.6\""),
        "{lock}"
    );
    let report = json(&run.stdout, &run.stderr);
    assert!(report.get("lockfile_cooldown").is_none(), "{report}");
    let holds = report["lockfile_holds"]
        .as_array()
        .unwrap_or_else(|| panic!("the hold must be reported: {report}"));
    assert_eq!(holds.len(), 1, "{report}");
    assert_eq!(holds[0]["lockfile"], "Cargo.lock", "{report}");
    assert_eq!(holds[0]["package"], "clap", "{report}");
    assert_eq!(holds[0]["from"], "4.6.7", "{report}");
    assert_eq!(holds[0]["to"], "4.6.6", "{report}");
    assert_eq!(holds[0]["cooldown"], "7d", "{report}");
    let published: DateTime<Utc> = holds[0]["published_at"]
        .as_str()
        .unwrap_or_else(|| panic!("the hold must date the release it moved back from: {report}"))
        .parse()
        .unwrap_or_else(|e| panic!("{e}: {report}"));
    let age = Utc::now() - published;
    assert!(
        age > Duration::hours(12) && age < Duration::days(2),
        "the date is 4.6.7's, the release the refresh locked, not 4.6.6's: {report}"
    );
}

/// When cargo refuses to move a crate back, the lockfile it wrote stays and
/// every young crate in it is reported with what went wrong.
#[tokio::test]
async fn a_cargo_crate_that_cannot_be_held_is_reported() {
    let run = cargo_refresh(&[("FAKE_REFUSE_PRECISE", "1")]).await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    let lock = fs::read_to_string(run.fx.project.join("Cargo.lock")).unwrap();
    assert!(
        lock.contains("version = \"4.6.7\""),
        "a refused hold leaves the refreshed lockfile as cargo wrote it: {lock}"
    );
    let report = json(&run.stdout, &run.stderr);
    assert!(report.get("lockfile_holds").is_none(), "{report}");
    let entries = report["lockfile_cooldown"]
        .as_array()
        .unwrap_or_else(|| panic!("no lockfile_cooldown in {report}"));
    let mut packages: Vec<&str> = entries
        .iter()
        .map(|e| e["package"].as_str().unwrap())
        .collect();
    packages.sort_unstable();
    assert_eq!(packages, ["clap", "clap_builder"], "{report}");
    for entry in entries {
        assert_eq!(entry["version"], "4.6.7", "{entry}");
        assert!(
            entry["note"]
                .as_str()
                .is_some_and(|n| n.contains("4.6.6") && n.contains("failed to select a version")),
            "the note names the release tried and cargo's refusal: {entry}"
        );
    }
}

/// Each young crate the refresh introduced, with the note saying why it was
/// not held; asserts nothing was reported as held.
fn unheld(run: &CargoRun) -> Vec<(String, String, String)> {
    let report = json(&run.stdout, &run.stderr);
    assert!(report.get("lockfile_holds").is_none(), "{report}");
    let mut entries: Vec<(String, String, String)> = report["lockfile_cooldown"]
        .as_array()
        .unwrap_or_else(|| panic!("no lockfile_cooldown in {report}"))
        .iter()
        .map(|e| {
            (
                e["package"].as_str().unwrap().to_string(),
                e["version"].as_str().unwrap().to_string(),
                e["note"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    entries.sort();
    entries
}

/// A `--precise` that exits 0 without moving the crate is not a hold.
#[tokio::test]
async fn a_cargo_precise_that_leaves_the_crate_in_place_is_not_reported_as_a_hold() {
    let run = cargo_refresh(&[("FAKE_PRECISE_NOOP", "1")]).await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    let entries = unheld(&run);
    assert_eq!(
        entries
            .iter()
            .map(|(p, v, _)| (p.as_str(), v.as_str()))
            .collect::<Vec<_>>(),
        [("clap", "4.6.7"), ("clap_builder", "4.6.7")],
        "{entries:?}"
    );
    for (_, _, note) in &entries {
        assert_eq!(note, "cargo did not move it to 4.6.6", "{entries:?}");
    }
}

/// Holding a crate back can move a crate pinned to it below what the
/// lockfile held before the run; that hold is undone rather than trading a
/// young release for a downgrade.
#[tokio::test]
async fn a_cargo_hold_that_would_downgrade_a_companion_is_undone() {
    let run = cargo_refresh_from(
        "",
        &[("clap", "4.5.0"), ("clap_builder", "4.6.6")],
        &[("clap", "4.6.6"), ("clap_builder", "4.6.5")],
        &[],
    )
    .await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    let lock = fs::read_to_string(run.fx.project.join("Cargo.lock")).unwrap();
    assert!(
        !lock.contains("4.6.5") && lock.contains("version = \"4.6.7\""),
        "the refused hold is put back: {lock}"
    );
    let entries = unheld(&run);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[0].0, "clap", "{entries:?}");
    assert_eq!(
        entries[0].2,
        "holding it at 4.6.6 would move clap_builder to 4.6.5, below the 4.6.6 locked before the run",
        "{entries:?}"
    );
    assert_eq!(entries[1].0, "clap_builder", "{entries:?}");
    assert_eq!(
        entries[1].2, "cargo did not move it to 4.6.6",
        "the companion's own attempt says why it was not held: {entries:?}"
    );
}

/// Cargo can carry out a hold by moving a crate an earlier hold moved. The
/// run reports what the lockfile it leaves behind carries, so the earlier
/// hold is not reported as still holding that crate, and the release it went
/// back to is read back like any other the refresh left behind.
#[tokio::test]
async fn a_hold_a_later_hold_moved_away_is_not_reported_as_held() {
    let run = cargo_refresh_locking(
        "",
        &[
            ("anstream", "0.6.8"),
            ("clap", "4.5.0"),
            ("clap_builder", "4.5.0"),
        ],
        &[
            ("anstream", "0.6.9"),
            ("clap", "4.6.7"),
            ("clap_builder", "4.6.7"),
        ],
        &[
            ("anstream", "0.6.8"),
            ("clap", "4.6.7"),
            ("clap_builder", "4.6.7"),
        ],
        Some(&[
            ("anstream", "0.6.9"),
            ("clap", "4.6.6"),
            ("clap_builder", "4.6.6"),
        ]),
        &[],
    )
    .await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        run.fx.logged(),
        vec![
            "update -p clap".to_string(),
            "update -p anstream@0.6.9 --precise 0.6.8".to_string(),
            "update -p clap@4.6.7 --precise 4.6.6".to_string(),
        ],
        "stderr: {}",
        run.stderr
    );
    let report = json(&run.stdout, &run.stderr);
    let holds = report["lockfile_holds"]
        .as_array()
        .unwrap_or_else(|| panic!("the hold that stands must be reported: {report}"));
    assert_eq!(
        holds.len(),
        1,
        "only the hold the lockfile carries: {report}"
    );
    assert_eq!(holds[0]["package"], "clap", "{report}");
    assert_eq!(holds[0]["to"], "4.6.6", "{report}");
    let cooldown = report["lockfile_cooldown"]
        .as_array()
        .unwrap_or_else(|| panic!("the release it went back to is read back: {report}"));
    assert_eq!(cooldown.len(), 1, "{report}");
    assert_eq!(cooldown[0]["package"], "anstream", "{report}");
    assert_eq!(cooldown[0]["version"], "0.6.9", "{report}");
    assert_eq!(
        cooldown[0]["note"], "a later hold moved it away from 0.6.8",
        "{report}"
    );
}

/// A pin names its crate the way the configuration spells it, which upd
/// reads for the crate whose name matches once case and separators are
/// normalized. A pin written `clap-builder` is therefore a floor for
/// `clap_builder`, and no hold takes it below.
#[tokio::test]
async fn a_cargo_hold_keeps_to_a_pin_the_lockfile_spells_differently() {
    let run = cargo_refresh_from(
        "[pin]\nclap-builder = \"4.6.7\"\n",
        CLAP_BEFORE,
        CLAP_HELD,
        &[],
    )
    .await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    let lock = fs::read_to_string(run.fx.project.join("Cargo.lock")).unwrap();
    assert!(
        lock.contains("version = \"4.6.7\"") && !lock.contains("version = \"4.6.6\""),
        "the hold that would move the pinned crate below its pin is put back: {lock}"
    );
    assert_eq!(
        unheld(&run),
        [
            (
                "clap".to_string(),
                "4.6.7".to_string(),
                "holding it at 4.6.6 would move clap_builder below 4.6.7, the version floor the run chose"
                    .to_string()
            ),
            (
                "clap_builder".to_string(),
                "4.6.7".to_string(),
                "it is the version floor the run chose".to_string()
            ),
        ]
    );
}

/// A refused hold whose lockfile cannot be put back leaves that hold on
/// disk, so the run fails naming the lockfile instead of carrying on.
#[tokio::test]
async fn a_cargo_lockfile_that_cannot_be_restored_after_a_refused_hold_fails_the_run() {
    let run = cargo_refresh_from(
        "",
        &[("clap", "4.5.0"), ("clap_builder", "4.6.6")],
        &[("clap", "4.6.6"), ("clap_builder", "4.6.5")],
        &[("FAKE_LOCK_READONLY", "1")],
    )
    .await;

    assert_eq!(
        run.code, 2,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        run.fx.logged(),
        vec![
            "update -p clap".to_string(),
            "update -p clap@4.6.7 --precise 4.6.6".to_string(),
        ],
        "no further hold is tried on a lockfile in an unknown state: {}",
        run.stderr
    );
    let report = json(&run.stdout, &run.stderr);
    assert_eq!(report["summary"]["errors"], 1, "{report}");
    let file = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "Cargo.toml")
        .unwrap_or_else(|| panic!("no Cargo.toml in {report}"));
    let message = file["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with(
            "Cargo.lock could not be restored after holding clap at 4.6.6 was refused ("
        ) && message.contains("would move clap_builder to 4.6.5"),
        "{report}"
    );
}

/// An exact version pinned in the configuration is a floor like one the run
/// wrote, so no hold takes a crate below it.
#[tokio::test]
async fn a_cargo_hold_never_moves_a_crate_below_its_configured_pin() {
    let run = cargo_refresh_from(
        "[pin]\nclap_builder = \"4.6.7\"\n",
        CLAP_BEFORE,
        CLAP_HELD,
        &[],
    )
    .await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        run.fx.logged(),
        vec![
            "update -p clap".to_string(),
            "update -p clap@4.6.7 --precise 4.6.6".to_string(),
        ],
        "the pinned crate itself is never tried: {}",
        run.stderr
    );
    let lock = fs::read_to_string(run.fx.project.join("Cargo.lock")).unwrap();
    assert!(
        !lock.contains("4.6.6"),
        "the refused hold is put back: {lock}"
    );
    assert_eq!(
        unheld(&run),
        [
            (
                "clap".to_string(),
                "4.6.7".to_string(),
                "holding it at 4.6.6 would move clap_builder below 4.6.7, the version floor the run chose"
                    .to_string()
            ),
            (
                "clap_builder".to_string(),
                "4.6.7".to_string(),
                "it is the version floor the run chose".to_string()
            ),
        ]
    );
}

/// An `update --package` floor for each of `packages` in a project whose
/// `Cargo.lock` starts at `before`, with `config` as `.updrc.toml`. The
/// clap_builder floor's `--precise` writes `floor`, and any other `--precise`
/// writes `held`.
async fn cargo_floor(
    config: &str,
    packages: &[&str],
    before: &[(&str, &str)],
    floor: &[(&str, &str)],
    held: &[(&str, &str)],
) -> CargoRun {
    let server = wiremock::MockServer::start().await;
    let releases = [
        ("4.5.0", days_ago(400)),
        ("4.6.6", days_ago(10)),
        ("4.6.7", days_ago(1)),
    ];
    mount_crate(&server, "clap", &releases).await;
    mount_crate(&server, "clap_builder", &releases).await;
    mount_crate(
        &server,
        "anstream",
        &[("0.6.8", days_ago(30)), ("0.6.9", days_ago(1))],
    )
    .await;
    let fx = Fixture::new();
    write_fake_tool(&fx.bin, "cargo", CARGO_RECORDING);
    fx.write(
        "Cargo.toml",
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nclap = \"4.5.0\"\n",
    );
    fx.write(".updrc.toml", config);
    fx.write("Cargo.lock", &cargo_lock(before));
    let floor = fx.stash("floor.lock", &cargo_lock(floor));
    let held = fx.stash("held.lock", &cargo_lock(held));

    let index = format!("sparse+{}/index/", server.uri());
    let mut args = vec!["update"];
    for package in packages {
        args.extend(["--package", package]);
    }
    args.extend([
        "--apply",
        "--min-age",
        "7d",
        "--no-cache",
        "--format",
        "json",
        ".",
    ]);
    let (stdout, stderr, code) = fx.run(
        &args,
        &[
            ("CARGO_REGISTRIES_CRATES_IO_INDEX", &index),
            ("FAKE_FLOOR_LOCK", held_path(&floor)),
            ("FAKE_HELD_LOCK", held_path(&held)),
        ],
    );
    CargoRun {
        fx,
        stdout,
        stderr,
        code,
    }
}

fn held_path(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A Cargo floor is written with `cargo update --precise`, which can lock a
/// companion crate published inside the cooldown; that crate is held back
/// like one a `--lock` refresh introduced.
#[tokio::test]
async fn a_cargo_floor_that_locks_a_young_companion_holds_it() {
    let run = cargo_floor(
        "",
        &["clap_builder"],
        CLAP_BEFORE,
        &[
            ("clap", "4.5.0"),
            ("clap_builder", "4.6.6"),
            ("anstream", "0.6.9"),
        ],
        &[
            ("clap", "4.5.0"),
            ("clap_builder", "4.6.6"),
            ("anstream", "0.6.8"),
        ],
    )
    .await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        run.fx.logged(),
        vec![
            "update -p clap_builder@4.5.0 --precise 4.6.6".to_string(),
            "update -p anstream@0.6.9 --precise 0.6.8".to_string(),
        ],
        "stderr: {}",
        run.stderr
    );
    let lock = fs::read_to_string(run.fx.project.join("Cargo.lock")).unwrap();
    assert!(
        lock.contains("version = \"0.6.8\"") && lock.contains("version = \"4.6.6\""),
        "{lock}"
    );
    let report = json(&run.stdout, &run.stderr);
    assert!(report.get("lockfile_cooldown").is_none(), "{report}");
    let holds = report["lockfile_holds"]
        .as_array()
        .unwrap_or_else(|| panic!("the hold must be reported: {report}"));
    assert_eq!(holds.len(), 1, "{report}");
    assert_eq!(holds[0]["package"], "anstream", "{report}");
    assert_eq!(holds[0]["from"], "0.6.9", "{report}");
    assert_eq!(holds[0]["to"], "0.6.8", "{report}");
}

/// A floor pinned inside the cooldown is what the run chose to lock, so no
/// hold moves it: not directly, and not by holding a companion that drags
/// it back.
#[tokio::test]
async fn a_cargo_floor_inside_the_cooldown_is_never_held_back() {
    let run = cargo_floor(
        "[pin]\nclap_builder = \"4.6.7\"\n",
        &["clap_builder"],
        CLAP_BEFORE,
        &[
            ("clap", "4.5.0"),
            ("clap_builder", "4.6.7"),
            ("anstream", "0.6.9"),
        ],
        &[
            ("clap", "4.5.0"),
            ("clap_builder", "4.6.6"),
            ("anstream", "0.6.8"),
        ],
    )
    .await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        run.fx.logged(),
        vec![
            "update -p clap_builder@4.5.0 --precise 4.6.7".to_string(),
            "update -p anstream@0.6.9 --precise 0.6.8".to_string(),
        ],
        "the floor itself is never tried: {}",
        run.stderr
    );
    let lock = fs::read_to_string(run.fx.project.join("Cargo.lock")).unwrap();
    assert!(
        lock.contains("version = \"4.6.7\"") && lock.contains("version = \"0.6.9\""),
        "the refused hold is put back: {lock}"
    );
    assert_eq!(
        unheld(&run),
        [
            (
                "anstream".to_string(),
                "0.6.9".to_string(),
                "holding it at 0.6.8 would move clap_builder below 4.6.7, the version floor the run chose"
                    .to_string()
            ),
            (
                "clap_builder".to_string(),
                "4.6.7".to_string(),
                "it is the version floor the run chose".to_string()
            ),
        ]
    );
}

/// A floor an earlier floor's update already reached is still a floor the
/// run chose, so no hold takes it back below.
#[tokio::test]
async fn a_cargo_floor_another_floor_already_reached_is_never_held_back() {
    let run = cargo_floor(
        "[pin]\nanstream = \"0.6.9\"\n",
        &["clap_builder", "anstream"],
        &[
            ("clap", "4.5.0"),
            ("clap_builder", "4.5.0"),
            ("anstream", "0.6.8"),
        ],
        &[
            ("clap", "4.5.0"),
            ("clap_builder", "4.6.6"),
            ("anstream", "0.6.9"),
        ],
        &[
            ("clap", "4.5.0"),
            ("clap_builder", "4.6.6"),
            ("anstream", "0.6.8"),
        ],
    )
    .await;

    assert_eq!(
        run.code, 0,
        "stdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    assert_eq!(
        run.fx.logged(),
        vec!["update -p clap_builder@4.5.0 --precise 4.6.6".to_string()],
        "anstream is never tried: {}",
        run.stderr
    );
    let lock = fs::read_to_string(run.fx.project.join("Cargo.lock")).unwrap();
    assert!(lock.contains("version = \"0.6.9\""), "{lock}");
    assert_eq!(
        unheld(&run),
        [(
            "anstream".to_string(),
            "0.6.9".to_string(),
            "it is the version floor the run chose".to_string()
        )]
    );
}
