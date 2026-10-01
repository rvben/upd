//! `update --lock` relocks the project upd selected even when the caller's
//! environment carries a uv project redirection. This file holds a single
//! test because it sets process-wide environment variables, which only one
//! test binary with one test can do without racing a sibling.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use upd::lockfile::{LockfileType, RegenOutcome, regenerate_lockfile};

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

fn uv_lock(dir: &Path) -> Output {
    Command::new("uv")
        .arg("lock")
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("uv not found on PATH: {e}"))
}

fn stale_uv_projects(root: &Path) -> (PathBuf, PathBuf) {
    let selected = root.join("selected");
    let other = root.join("other");
    for (dir, name) in [(&selected, "selected"), (&other, "other")] {
        write_offline_uv_project(dir, name, "0.1.0");
        let output = uv_lock(dir);
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
fn relock_ignores_inherited_project_redirection() {
    for variable in ["UV_PROJECT", "UV_WORKING_DIR"] {
        let temp = tempfile::tempdir().unwrap();
        let (selected, other) = stale_uv_projects(temp.path());
        let other_before = fs::read_to_string(other.join("uv.lock")).unwrap();

        // SAFETY: this test binary runs this one test, so no other thread
        // reads the environment concurrently.
        unsafe { std::env::set_var(variable, &other) };
        let relock = regenerate_lockfile(
            &selected.join("pyproject.toml"),
            LockfileType::UvLock,
            &[],
            None,
            false,
        );
        unsafe { std::env::remove_var(variable) };

        assert!(
            matches!(relock.outcome, RegenOutcome::Ok(_)),
            "{variable}: {:?}",
            relock.outcome.error_message()
        );
        assert_eq!(
            fs::read_to_string(other.join("uv.lock")).unwrap(),
            other_before,
            "{variable} redirected the relock into another project's lockfile"
        );
        assert!(
            lock_records_version(&selected, "0.2.0"),
            "{variable}: the selected lockfile was not relocked"
        );
    }
}
