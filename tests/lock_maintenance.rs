#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn fake_tool(dir: &Path, name: &str, script: &str) {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let path = bin.join(name);
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_upd"))
        .args(args)
        .current_dir(dir)
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap()
}

#[test]
fn dry_run_lists_lockfiles_without_running_package_managers() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("pyproject.toml"),
        "[project]\nname = 'demo'\nversion = '0.1.0'\n",
    )
    .unwrap();
    fs::write(temp.path().join("uv.lock"), "version = 1\n").unwrap();
    let output = run(temp.path(), &["lock-refresh", ".", "-o", "json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report[0]["status"], "planned");
    assert_eq!(
        fs::read_to_string(temp.path().join("uv.lock")).unwrap(),
        "version = 1\n"
    );
}

#[test]
fn npm_refresh_reports_transitive_changes_without_editing_the_manifest() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("package.json"),
        "{\"name\":\"demo\",\"dependencies\":{\"alpha\":\"^1.0.0\"}}\n",
    )
    .unwrap();
    fs::write(temp.path().join("package-lock.json"), r#"{"lockfileVersion":3,"packages":{"":{"name":"demo"},"node_modules/alpha":{"version":"1.0.0","resolved":"https://registry.npmjs.org/alpha/-/alpha-1.0.0.tgz"}}}"#).unwrap();
    fake_tool(
        temp.path(),
        "npm",
        r#"test "$1" = update
test "$2" = --package-lock-only
cat > package-lock.json <<'JSON'
{"lockfileVersion":3,"packages":{"":{"name":"demo"},"node_modules/alpha":{"version":"1.1.0","resolved":"https://registry.npmjs.org/alpha/-/alpha-1.1.0.tgz"}}}
JSON"#,
    );
    let output = run(temp.path(), &["lock-refresh", ".", "--apply", "-o", "json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report[0]["status"], "refreshed");
    assert_eq!(report[0]["changes"][0]["package"], "alpha");
    assert_eq!(report[0]["changes"][0]["bump"], "minor");
    assert!(
        fs::read_to_string(temp.path().join("package.json"))
            .unwrap()
            .contains("^1.0.0")
    );
}

#[test]
fn uv_refresh_rolls_back_when_an_ignored_package_moves() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("pyproject.toml"),
        "[project]\nname = 'demo'\nversion = '0.1.0'\n",
    )
    .unwrap();
    fs::write(temp.path().join(".updrc.toml"), "ignore = ['alpha']\n").unwrap();
    let before = "version = 1\n[[package]]\nname = 'alpha'\nversion = '1.0.0'\nsource = { registry = 'https://pypi.org/simple' }\n[[package]]\nname = 'beta'\nversion = '1.0.0'\nsource = { registry = 'https://pypi.org/simple' }\n";
    fs::write(temp.path().join("uv.lock"), before).unwrap();
    fake_tool(
        temp.path(),
        "uv",
        r#"test "$1" = lock
printf '%s\n' "$@" > uv-args.txt
cat > uv.lock <<'LOCK'
version = 1
[[package]]
name = 'alpha'
version = '1.1.0'
source = { registry = 'https://pypi.org/simple' }
[[package]]
name = 'beta'
version = '1.1.0'
source = { registry = 'https://pypi.org/simple' }
LOCK"#,
    );
    let output = run(temp.path(), &["lock-refresh", ".", "--apply", "-o", "json"]);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report[0]["status"], "failed");
    assert!(
        report[0]["error"]
            .as_str()
            .unwrap()
            .contains("protected package alpha moved")
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("uv.lock")).unwrap(),
        before
    );
    let args = fs::read_to_string(temp.path().join("uv-args.txt")).unwrap();
    assert!(args.contains("--upgrade-package\nbeta\n"));
    assert!(!args.contains("--upgrade-package\nalpha\n"));
}

#[test]
fn cargo_refresh_respects_the_bump_ceiling_and_restores_the_lockfile() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = 'demo'\nversion = '0.1.0'\n",
    )
    .unwrap();
    let before = "version = 4\n[[package]]\nname = 'alpha'\nversion = '1.0.0'\nsource = 'registry+https://github.com/rust-lang/crates.io-index'\n";
    fs::write(temp.path().join("Cargo.lock"), before).unwrap();
    fake_tool(
        temp.path(),
        "cargo",
        r#"test "$1" = update
cat > Cargo.lock <<'LOCK'
version = 4
[[package]]
name = 'alpha'
version = '2.0.0'
source = 'registry+https://github.com/rust-lang/crates.io-index'
LOCK"#,
    );
    let output = run(
        temp.path(),
        &[
            "lock-refresh",
            ".",
            "--apply",
            "--max-bump",
            "minor",
            "-o",
            "json",
        ],
    );
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report[0]["error"]
            .as_str()
            .unwrap()
            .contains("exceeds --max-bump")
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("Cargo.lock")).unwrap(),
        before
    );
}

#[test]
fn cooldown_refuses_refresh_before_invoking_the_tool() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("package.json"), "{\"name\":\"demo\"}\n").unwrap();
    let before = "{\"lockfileVersion\":3,\"packages\":{}}\n";
    fs::write(temp.path().join("package-lock.json"), before).unwrap();
    fs::write(
        temp.path().join(".updrc.toml"),
        "[cooldown]\ndefault = '7d'\n",
    )
    .unwrap();
    let output = run(temp.path(), &["lock-refresh", ".", "--apply", "-o", "json"]);
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(temp.path().join("package-lock.json")).unwrap(),
        before
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report[0]["error"].as_str().unwrap().contains("cooldown"));
}

#[test]
fn invalid_output_fields_fail_before_running_a_package_manager() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("package.json"), "{\"name\":\"demo\"}\n").unwrap();
    fs::write(
        temp.path().join("package-lock.json"),
        "{\"lockfileVersion\":3,\"packages\":{}}\n",
    )
    .unwrap();
    fake_tool(temp.path(), "npm", "touch was-run");
    let output = run(
        temp.path(),
        &[
            "lock-refresh",
            ".",
            "--apply",
            "-o",
            "json",
            "--fields",
            "secret",
        ],
    );
    assert!(!output.status.success());
    assert!(!temp.path().join("was-run").exists());
}

/// Writes an offline uv project (no dependencies, no index, isolated cache)
/// at `version`, so a real `uv lock` runs without the network.
fn write_offline_uv_project(dir: &Path, name: &str, version: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("pyproject.toml"),
        format!(
            "[project]\nname = \"{name}\"\nversion = \"{version}\"\nrequires-python = \">=3.8\"\ndependencies = []\n\n\
             [tool.uv]\nno-index = true\npython-downloads = \"never\"\ncache-dir = \"{cache}\"\n",
            cache = dir.join("uv-cache").display(),
        ),
    )
    .unwrap();
}

fn uv_lock_in(dir: &Path, envs: &[(&str, &Path)]) -> Output {
    let mut command = Command::new("uv");
    command.arg("lock").current_dir(dir);
    command
        .env_remove("UV_PROJECT")
        .env_remove("UV_WORKING_DIR");
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .output()
        .unwrap_or_else(|e| panic!("uv not found on PATH: {e}"))
}

/// Two locked projects whose manifests have both moved past their lockfiles,
/// so a `uv lock` in either one rewrites that project's uv.lock.
fn stale_uv_projects(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let selected = root.join("selected");
    let other = root.join("other");
    for (dir, name) in [(&selected, "selected"), (&other, "other")] {
        write_offline_uv_project(dir, name, "0.1.0");
        let output = uv_lock_in(dir, &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        write_offline_uv_project(dir, name, "0.2.0");
    }
    (selected, other)
}

fn lock_records_version(dir: &Path, version: &str) -> bool {
    fs::read_to_string(dir.join("uv.lock"))
        .unwrap()
        .contains(&format!("version = \"{version}\""))
}

#[test]
fn uv_refresh_ignores_inherited_project_redirection() {
    for variable in ["UV_PROJECT", "UV_WORKING_DIR"] {
        let temp = tempfile::tempdir().unwrap();
        let (selected, other) = stale_uv_projects(temp.path());

        // Negative control: the variable really redirects a plain uv lock.
        let control = tempfile::tempdir().unwrap();
        let (control_selected, control_other) = stale_uv_projects(control.path());
        let output = uv_lock_in(&control_selected, &[(variable, &control_other)]);
        assert!(output.status.success());
        assert!(lock_records_version(&control_other, "0.2.0"));
        assert!(lock_records_version(&control_selected, "0.1.0"));

        let other_before = fs::read_to_string(other.join("uv.lock")).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_upd"))
            .args(["lock-refresh", ".", "--apply", "-o", "json"])
            .current_dir(&selected)
            .env(variable, &other)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{variable}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(other.join("uv.lock")).unwrap(),
            other_before,
            "{variable} redirected the refresh into another project's lockfile"
        );
        assert!(
            lock_records_version(&selected, "0.2.0"),
            "{variable}: the selected lockfile was not refreshed"
        );
    }
}
