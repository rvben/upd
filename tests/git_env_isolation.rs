//! A suite running inside a linked worktree's git hook must leave that
//! repository alone. See `tests/isolated/mod.rs`.

#![cfg(unix)]

mod isolated;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_git(dir: &Path, args: &[&str]) {
    let output = isolated::command("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn shared_config(repo: &Path) -> String {
    fs::read_to_string(repo.join(".git/config")).unwrap()
}

/// A repository with a linked worktree, and the environment a git hook
/// running in that worktree receives.
fn repository_with_hook_env() -> (tempfile::TempDir, PathBuf, Vec<(&'static str, PathBuf)>) {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let worktree = root.path().join("worktree");
    fs::create_dir(&repo).unwrap();
    run_git(&repo, &["init", "--quiet"]);
    run_git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "x",
        ],
    );
    run_git(
        &repo,
        &["worktree", "add", "--quiet", worktree.to_str().unwrap()],
    );
    let admin = repo.join(".git/worktrees/worktree");
    let env = vec![
        ("GIT_DIR", admin.clone()),
        ("GIT_WORK_TREE", worktree),
        ("GIT_INDEX_FILE", admin.join("index")),
    ];
    (root, repo, env)
}

const PROBE_DIR: &str = "UPD_ISOLATED_PROBE_DIR";

#[test]
fn a_hook_environment_does_not_reach_the_hook_repository() {
    let (root, repo, hook_env) = repository_with_hook_env();
    let before = shared_config(&repo);

    // Control: plain git under the hook environment re-initializes the hook's
    // repository, proving this test can observe the damage.
    let raw = root.path().join("raw");
    fs::create_dir(&raw).unwrap();
    let status = Command::new("git")
        .current_dir(&raw)
        .args(["init", "--quiet"])
        .envs(hook_env.clone())
        .status()
        .unwrap();
    assert!(status.success());
    let corrupted = shared_config(&repo);
    assert!(
        corrupted.contains("worktree = "),
        "control did not corrupt:\n{corrupted}"
    );
    assert!(!raw.join(".git").exists());
    fs::write(repo.join(".git/config"), &before).unwrap();

    // The real path: a test process that inherits the hook environment, as
    // every test does under the pre-push hook. Re-run this binary with only
    // the probe selected.
    let isolated = root.path().join("isolated");
    fs::create_dir(&isolated).unwrap();
    let probe = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "probe_init", "--ignored", "--test-threads=1"])
        .env(PROBE_DIR, &isolated)
        .envs(hook_env)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&probe.stdout);
    assert!(
        probe.status.success() && stdout.contains("1 passed"),
        "probe did not run:\n{stdout}\n{}",
        String::from_utf8_lossy(&probe.stderr)
    );

    assert_eq!(shared_config(&repo), before);
    assert!(
        isolated.join(".git").is_dir(),
        "git init did not create the fixture repository"
    );
}

/// Run only by the test above, in a child process holding a hook environment.
/// It goes through a script, as the workflow suites do, so the environment has
/// to be gone before bash starts rather than only from direct git calls.
#[test]
#[ignore = "spawned by a_hook_environment_does_not_reach_the_hook_repository"]
fn probe_init() {
    let dir = std::env::var_os(PROBE_DIR).expect("run only as a probe");
    let status = isolated::command("bash")
        .current_dir(&dir)
        .args(["-c", "git init --quiet"])
        .status()
        .expect("bash starts");
    assert!(status.success());
}

#[test]
fn every_repository_variable_git_knows_is_removed() {
    let output = Command::new("git")
        .args(["rev-parse", "--local-env-vars"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let missing: Vec<_> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|name| !isolated::REPOSITORY_ENV.contains(name))
        .map(str::to_owned)
        .collect();
    assert!(missing.is_empty(), "add to REPOSITORY_ENV: {missing:?}");
}

/// Any other way of starting git or a script would inherit a hook's
/// environment. Spawning `upd` itself is fine: it strips these variables from
/// its own children.
#[test]
fn suites_spawn_git_and_scripts_only_through_the_isolated_module() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let exempt = [
        tests.join("git_env_isolation.rs"),
        tests.join("isolated/mod.rs"),
    ];
    let forbidden = [
        concat!("Command::new(", "\"git\")"),
        concat!("Command::new(", "\"bash\")"),
        concat!("Command::new(", "\"sh\")"),
    ];
    let mut offenders = Vec::new();
    let mut pending = vec![tests.clone()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") && !exempt.contains(&path) {
                let source = fs::read_to_string(&path).unwrap();
                if forbidden.iter().any(|call| source.contains(call)) {
                    offenders.push(path.strip_prefix(&tests).unwrap().display().to_string());
                }
            }
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "spawn through tests/isolated/mod.rs instead: {offenders:?}"
    );
}
