//! What a lock job may change. The job runs repository code, so its patch
//! is applied to the commit it started from in a scratch index, never to a
//! checkout, and the resulting tree is compared with that commit entry by
//! entry before anything is published.
//!
//! An entry is accepted when it is exactly the planned edit to that path
//! (prepared by a job that ran no repository code), or when it modifies an
//! existing lockfile in place, under the scanned paths, without adding a
//! place the lockfile fetches code from. Everything else is refused:
//! creations, deletions, renames, mode and type changes, binary patches,
//! other files, and a planned manifest the lock job edited differently. A
//! planned edit the lock job left out stays at the base content, as a relock
//! that rolled the change back.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::Error;
use super::git::Git;
use super::sources;
use crate::lockfile::LockfileType;

/// File modes a lockfile or manifest may have.
const REGULAR: [&str; 2] = ["100644", "100755"];

/// One changed path as `git diff-tree --raw` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    old_mode: String,
    new_mode: String,
    old_blob: String,
    new_blob: String,
    status: String,
}

/// Checks the lock job's `result` patch against `base`, the commit both
/// patches start from, and `planned`, the planned edits. Returns the tree
/// the result records, or refuses with every problem found.
pub(super) async fn check(
    git: &Git,
    base: &str,
    planned: &[u8],
    result: &[u8],
    paths: &[String],
) -> Result<String, Error> {
    if !is_object_id(base) {
        return Err(Error::Refused(format!(
            "The commit a lock job starts from must be a full object id, not '{base}'"
        )));
    }
    let scratch = tempfile::Builder::new()
        .prefix("upd-patch-")
        .tempdir()
        .map_err(|error| Error::Io(format!("cannot create a work directory: {error}")))?;
    let planned_tree = apply(git, scratch.path(), "planned", base, planned)
        .await
        .map_err(|error| Error::Io(format!("the planned edits do not apply: {error}")))?;

    let mut problems = binary_paths(git, scratch.path(), result)
        .await?
        .into_iter()
        .map(|path| format!("{path}: a binary patch"))
        .collect::<Vec<_>>();
    let result_tree = apply(git, scratch.path(), "result", base, result)
        .await
        .map_err(|error| {
            Error::Refused(format!(
                "The lock job's patch does not apply to the commit it started from; nothing was published ({error})"
            ))
        })?;

    let planned = entries(git, base, &planned_tree).await?;
    for (path, entry) in entries(git, base, &result_tree).await? {
        if planned.get(&path) == Some(&entry) {
            continue;
        }
        if let Err(problem) =
            check_entry(git, &path, &entry, planned.contains_key(&path), paths).await?
        {
            problems.push(format!("{path}: {problem}"));
        }
    }
    if problems.is_empty() {
        return Ok(result_tree);
    }
    Err(Error::Refused(format!(
        "The lock job changed what lock mode does not publish; nothing was published:\n- {}",
        problems.join("\n- ")
    )))
}

/// Why `entry`, which is not a planned edit, cannot be published; `Ok(Ok)`
/// when it is a lockfile modified within the rules.
async fn check_entry(
    git: &Git,
    path: &str,
    entry: &Entry,
    planned: bool,
    paths: &[String],
) -> Result<Result<(), String>, Error> {
    let change = match entry.status.as_str() {
        "M" if entry.old_mode != entry.new_mode => Some("changes the file mode"),
        "M" => None,
        "A" => Some("creates a file"),
        "D" => Some("deletes a file"),
        "T" => Some("changes the file type"),
        _ => Some("changes the path in a way lock mode does not publish"),
    };
    if let Some(change) = change {
        return Ok(Err(change.to_string()));
    }
    if !REGULAR.contains(&entry.new_mode.as_str()) {
        return Ok(Err(format!(
            "is not a regular file (mode {})",
            entry.new_mode
        )));
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some(kind) = LockfileType::from_filename(name) else {
        return Ok(Err(if planned {
            "differs from the planned edit"
        } else {
            "is neither a planned edit nor a lockfile"
        }
        .to_string()));
    };
    if !in_scope(path, paths) {
        return Ok(Err("is outside the scanned paths".to_string()));
    }
    let (Some(base), Some(result)) = (
        text(git, &entry.old_blob).await?,
        text(git, &entry.new_blob).await?,
    ) else {
        return Ok(Err("is not UTF-8 text".to_string()));
    };
    Ok(sources::check(kind, &base, &result))
}

/// Applies `patch` to `base` in a scratch index and names the tree it
/// records. An empty patch records the base tree.
async fn apply(
    git: &Git,
    scratch: &Path,
    name: &str,
    base: &str,
    patch: &[u8],
) -> Result<String, Error> {
    let index = scratch.join(format!("{name}.index"));
    let file = scratch.join(format!("{name}.patch"));
    fs::write(&file, patch)
        .map_err(|error| Error::Io(format!("cannot write {}: {error}", file.display())))?;
    git.bytes_with_index(&index, ["read-tree", base]).await?;
    if !patch.is_empty() {
        git.bytes_with_index(
            &index,
            [
                "apply".as_ref(),
                "--cached".as_ref(),
                "--whitespace=nowarn".as_ref(),
                file.as_os_str(),
            ],
        )
        .await?;
    }
    let tree = git.bytes_with_index(&index, ["write-tree"]).await?;
    Ok(String::from_utf8_lossy(&tree).trim_end().to_string())
}

/// Paths `patch` changes with a binary patch, which lock mode never
/// publishes.
async fn binary_paths(git: &Git, scratch: &Path, patch: &[u8]) -> Result<Vec<String>, Error> {
    if patch.is_empty() {
        return Ok(Vec::new());
    }
    let file = scratch.join("numstat.patch");
    fs::write(&file, patch)
        .map_err(|error| Error::Io(format!("cannot write {}: {error}", file.display())))?;
    let numstat = git
        .bytes([
            "apply".as_ref(),
            "--numstat".as_ref(),
            "-z".as_ref(),
            file.as_os_str(),
        ])
        .await
        .map_err(|error| {
            Error::Refused(format!(
                "The lock job's patch cannot be read; nothing was published ({error})"
            ))
        })?;
    let numstat = String::from_utf8_lossy(&numstat);
    Ok(numstat
        .split('\0')
        .filter_map(|record| record.strip_prefix("-\t-\t"))
        .map(str::to_string)
        .collect())
}

/// Every path `tree` changes relative to `base`, without rename detection.
async fn entries(git: &Git, base: &str, tree: &str) -> Result<BTreeMap<String, Entry>, Error> {
    let raw = git
        .bytes([
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--no-abbrev",
            "--raw",
            base,
            tree,
        ])
        .await?;
    let raw = String::from_utf8(raw).map_err(|_| {
        Error::Refused("The lock job's result names a path that is not UTF-8".to_string())
    })?;
    let mut fields = raw.split('\0');
    let mut entries = BTreeMap::new();
    while let Some(meta) = fields.next().filter(|meta| !meta.is_empty()) {
        let path = fields
            .next()
            .ok_or_else(|| Error::Io(format!("unexpected git diff-tree output: {meta}")))?;
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_blob, new_blob, status] = parts[..] else {
            return Err(Error::Io(format!(
                "unexpected git diff-tree output: {meta}"
            )));
        };
        entries.insert(
            path.to_string(),
            Entry {
                old_mode: old_mode.to_string(),
                new_mode: new_mode.to_string(),
                old_blob: old_blob.to_string(),
                new_blob: new_blob.to_string(),
                status: status.to_string(),
            },
        );
    }
    Ok(entries)
}

/// The blob's content, or `None` when it is not UTF-8.
async fn text(git: &Git, blob: &str) -> Result<Option<String>, Error> {
    Ok(String::from_utf8(git.bytes(["cat-file", "blob", blob]).await?).ok())
}

/// Whether `text` is a full SHA-1 or SHA-256 object id in lowercase hex.
pub(super) fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64)
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `path` lies under one of the scanned `paths`, each relative to
/// the repository root.
fn in_scope(path: &str, paths: &[String]) -> bool {
    paths.iter().any(|scanned| {
        let scanned = scanned.trim_start_matches("./").trim_end_matches('/');
        scanned.is_empty()
            || scanned == "."
            || path == scanned
            || path
                .strip_prefix(scanned)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The fixtures commit symlinks, so these tests run on Unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const PYPROJECT: &str = "[project]\nname = \"demo\"\ndependencies = [\"requests==2.32.3\"]\n";
    const PYPROJECT_PLANNED: &str =
        "[project]\nname = \"demo\"\ndependencies = [\"requests==2.32.4\"]\n";

    const UV_LOCK: &str = r#"version = 1
requires-python = ">=3.12"

[[package]]
name = "demo"
version = "0.1.0"
source = { editable = "." }
dependencies = [{ name = "requests" }]

[[package]]
name = "requests"
version = "2.32.3"
source = { registry = "https://pypi.org/simple" }
sdist = { url = "https://files.pythonhosted.org/packages/r/requests-2.32.3.tar.gz", hash = "sha256:aa", size = 1 }
"#;

    /// The lockfile a relock of the planned edit writes.
    fn relocked() -> String {
        UV_LOCK
            .replace("2.32.3", "2.32.4")
            .replace("sha256:aa", "sha256:cc")
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        dir: PathBuf,
        git: Git,
        base: String,
    }

    /// One change to the fixture's working tree.
    enum Edit<'a> {
        Write(&'a str, &'a [u8]),
        Remove(&'a str),
        Executable(&'a str),
        Symlink(&'a str, &'a str),
    }

    use Edit::{Executable, Remove, Symlink, Write};
    use std::path::PathBuf;

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_path_buf();
            let git = Git::new(&path, "").unwrap();
            git.run(["init", "--quiet", "--initial-branch=main"])
                .await
                .unwrap();
            git.run(["config", "commit.gpgsign", "false"])
                .await
                .unwrap();
            for (file, content) in [
                ("pyproject.toml", PYPROJECT),
                ("uv.lock", UV_LOCK),
                ("go.sum", "example.test/mod v1.0.0 h1:aa=\n"),
                ("README.md", "demo\n"),
                ("sub/pyproject.toml", PYPROJECT),
                ("sub/uv.lock", UV_LOCK),
            ] {
                let file = path.join(file);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, content).unwrap();
            }
            fs::create_dir_all(path.join("sub/link")).unwrap();
            std::os::unix::fs::symlink("../../uv.lock", path.join("sub/link/uv.lock")).unwrap();
            git.run(["add", "--all"]).await.unwrap();
            git.commit("base", "upd", "upd@example.test").await.unwrap();
            let base = git.read(["rev-parse", "HEAD"]).await.unwrap();
            Self {
                _dir: dir,
                dir: path,
                git,
                base,
            }
        }

        /// The patch `edits` make to the base commit, as the lock job
        /// records it, and the tree they produce. The working tree is back
        /// at the base afterwards.
        async fn diff(&self, edits: &[Edit<'_>], extra: &[&str]) -> (Vec<u8>, String) {
            for edit in edits {
                match edit {
                    Write(file, content) => {
                        let file = self.dir.join(file);
                        fs::create_dir_all(file.parent().unwrap()).unwrap();
                        fs::write(file, content).unwrap();
                    }
                    Remove(file) => fs::remove_file(self.dir.join(file)).unwrap(),
                    Executable(_) | Symlink(..) => {}
                }
            }
            for edit in edits {
                if let Symlink(file, target) = edit {
                    fs::remove_file(self.dir.join(file)).unwrap();
                    std::os::unix::fs::symlink(target, self.dir.join(file)).unwrap();
                }
            }
            self.git.run(["add", "--all"]).await.unwrap();
            for edit in edits {
                if let Executable(file) = edit {
                    self.git
                        .run(["update-index", "--chmod=+x", file])
                        .await
                        .unwrap();
                }
            }
            let mut args = vec!["diff", "--cached", "--binary", "--full-index"];
            args.extend(extra);
            args.push("HEAD");
            let patch = self.git.bytes(&args).await.unwrap();
            let tree = self.git.read(["write-tree"]).await.unwrap();
            self.git.run(["reset", "--quiet", "--hard"]).await.unwrap();
            self.git.run(["clean", "-fdq"]).await.unwrap();
            (patch, tree)
        }

        async fn planned(&self) -> Vec<u8> {
            self.diff(
                &[Write("pyproject.toml", PYPROJECT_PLANNED.as_bytes())],
                &["--no-renames"],
            )
            .await
            .0
        }

        async fn check(
            &self,
            planned: &[u8],
            result: &[u8],
            paths: &[&str],
        ) -> Result<String, Error> {
            let paths: Vec<String> = paths.iter().map(ToString::to_string).collect();
            check(&self.git, &self.base, planned, result, &paths).await
        }

        /// The refusal checking `edits` against the planned edit produces.
        async fn refusal(&self, edits: &[Edit<'_>]) -> String {
            let planned = self.planned().await;
            let (result, _) = self.diff(edits, &["--no-renames"]).await;
            match self.check(&planned, &result, &["."]).await {
                Err(Error::Refused(message)) => message,
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_relock_of_the_planned_edit_is_accepted_as_its_tree() {
        let fixture = Fixture::new().await;
        let planned = fixture.planned().await;
        let relocked = relocked();
        let (result, tree) = fixture
            .diff(
                &[
                    Write("pyproject.toml", PYPROJECT_PLANNED.as_bytes()),
                    Write("uv.lock", relocked.as_bytes()),
                    Write("sub/uv.lock", relocked.as_bytes()),
                ],
                &["--no-renames"],
            )
            .await;
        assert_eq!(
            fixture.check(&planned, &result, &["."]).await.unwrap(),
            tree
        );
    }

    #[tokio::test]
    async fn a_planned_edit_left_at_the_base_content_is_a_rollback() {
        let fixture = Fixture::new().await;
        let planned = fixture.planned().await;
        let base_tree = fixture
            .git
            .read(["rev-parse", "HEAD^{tree}"])
            .await
            .unwrap();
        assert_eq!(
            fixture.check(&planned, b"", &["."]).await.unwrap(),
            base_tree
        );
    }

    #[tokio::test]
    async fn a_planned_manifest_the_lock_job_edited_differently_is_refused() {
        let fixture = Fixture::new().await;
        let altered = PYPROJECT_PLANNED.replace("]\n", ", \"evil==1\"]\n");
        let message = fixture
            .refusal(&[Write("pyproject.toml", altered.as_bytes())])
            .await;
        assert!(
            message.contains("pyproject.toml: differs from the planned edit"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_manifest_outside_the_plan_is_refused() {
        let fixture = Fixture::new().await;
        let message = fixture
            .refusal(&[Write("sub/pyproject.toml", PYPROJECT_PLANNED.as_bytes())])
            .await;
        assert!(
            message.contains("sub/pyproject.toml: is neither a planned edit nor a lockfile"),
            "{message}"
        );
        assert!(!message.contains("- pyproject.toml"), "{message}");
    }

    #[tokio::test]
    async fn creating_deleting_or_renaming_a_lockfile_is_refused() {
        let fixture = Fixture::new().await;
        let message = fixture
            .refusal(&[Write("docs/uv.lock", UV_LOCK.as_bytes())])
            .await;
        assert!(
            message.contains("docs/uv.lock: creates a file"),
            "{message}"
        );

        let message = fixture.refusal(&[Remove("sub/uv.lock")]).await;
        assert!(message.contains("sub/uv.lock: deletes a file"), "{message}");

        // A patch with rename headers moves the file just the same.
        let planned = fixture.planned().await;
        let (result, _) = fixture
            .diff(
                &[
                    Remove("sub/uv.lock"),
                    Write("lib/uv.lock", UV_LOCK.as_bytes()),
                ],
                &["--find-renames"],
            )
            .await;
        assert!(String::from_utf8_lossy(&result).contains("rename from sub/uv.lock"));
        let Err(Error::Refused(message)) = fixture.check(&planned, &result, &["."]).await else {
            panic!("a rename was accepted");
        };
        assert!(message.contains("sub/uv.lock: deletes a file"), "{message}");
        assert!(message.contains("lib/uv.lock: creates a file"), "{message}");
    }

    #[tokio::test]
    async fn a_mode_or_type_change_is_refused() {
        let fixture = Fixture::new().await;
        let message = fixture.refusal(&[Executable("uv.lock")]).await;
        assert!(
            message.contains("uv.lock: changes the file mode"),
            "{message}"
        );

        let message = fixture.refusal(&[Symlink("uv.lock", "/etc/passwd")]).await;
        assert!(
            message.contains("uv.lock: changes the file type"),
            "{message}"
        );

        // A lockfile that is already a symlink keeps its type when retargeted.
        let message = fixture
            .refusal(&[Symlink("sub/link/uv.lock", "/etc/passwd")])
            .await;
        assert!(
            message.contains("sub/link/uv.lock: is not a regular file (mode 120000)"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_binary_or_non_utf8_lockfile_is_refused() {
        let fixture = Fixture::new().await;
        let message = fixture
            .refusal(&[Write("uv.lock", b"version = 1\n\0\n")])
            .await;
        assert!(message.contains("uv.lock: a binary patch"), "{message}");

        let message = fixture
            .refusal(&[Write("uv.lock", b"version = 1\n# caf\xe9\n")])
            .await;
        assert!(message.contains("uv.lock: is not UTF-8 text"), "{message}");
    }

    #[tokio::test]
    async fn a_lockfile_taking_code_from_a_new_place_is_refused() {
        let fixture = Fixture::new().await;
        let evil = UV_LOCK.replace(
            "source = { registry = \"https://pypi.org/simple\" }",
            "source = { git = \"https://example.test/evil/requests?rev=v1#2222222\" }",
        );
        let message = fixture.refusal(&[Write("uv.lock", evil.as_bytes())]).await;
        assert!(
            message.contains("uv.lock: the regenerated uv.lock"),
            "{message}"
        );
        assert!(message.contains("example.test/evil/requests"), "{message}");
    }

    #[tokio::test]
    async fn a_lockfile_upd_cannot_read_is_refused() {
        let fixture = Fixture::new().await;
        let message = fixture
            .refusal(&[Write("go.sum", b"example.test/mod v1.0.1 h1:bb=\n")])
            .await;
        assert!(
            message.contains("go.sum: upd cannot check where go.sum fetches from"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_lockfile_outside_the_scanned_paths_is_refused() {
        let fixture = Fixture::new().await;
        let relocked = relocked();
        let (result, tree) = fixture
            .diff(
                &[
                    Write("uv.lock", relocked.as_bytes()),
                    Write("sub/uv.lock", relocked.as_bytes()),
                ],
                &["--no-renames"],
            )
            .await;
        let Err(Error::Refused(message)) = fixture.check(b"", &result, &["sub/"]).await else {
            panic!("a lockfile outside the scanned paths was accepted");
        };
        assert!(
            message.contains("- uv.lock: is outside the scanned paths"),
            "{message}"
        );
        assert!(!message.contains("sub/uv.lock"), "{message}");
        assert_eq!(
            fixture.check(b"", &result, &["./sub", "."]).await.unwrap(),
            tree
        );
    }

    #[test]
    fn a_scanned_path_covers_itself_and_what_lies_below_it() {
        let paths = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        for scanned in [&["."][..], &[""], &["./"]] {
            assert!(in_scope("uv.lock", &paths(scanned)));
            assert!(in_scope("a/b/uv.lock", &paths(scanned)));
        }
        for scanned in ["sub", "sub/", "./sub"] {
            assert!(in_scope("sub/uv.lock", &paths(&[scanned])), "{scanned}");
            assert!(
                in_scope("sub/deep/uv.lock", &paths(&[scanned])),
                "{scanned}"
            );
            assert!(!in_scope("subway/uv.lock", &paths(&[scanned])), "{scanned}");
            assert!(!in_scope("uv.lock", &paths(&[scanned])), "{scanned}");
        }
        assert!(in_scope("b/uv.lock", &paths(&["a", "b"])));
        assert!(!in_scope("uv.lock", &paths(&[])));
    }

    #[tokio::test]
    async fn every_problem_is_listed_in_one_refusal() {
        let fixture = Fixture::new().await;
        let message = fixture
            .refusal(&[Write("README.md", b"changed\n"), Remove("sub/uv.lock")])
            .await;
        assert!(
            message.starts_with("The lock job changed what lock mode does not publish"),
            "{message}"
        );
        assert!(
            message.contains("- README.md: is neither a planned edit nor a lockfile"),
            "{message}"
        );
        assert!(
            message.contains("- sub/uv.lock: deletes a file"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_patch_that_does_not_apply_or_parse_is_refused() {
        let fixture = Fixture::new().await;
        let relocked = relocked();
        let (result, _) = fixture
            .diff(&[Write("uv.lock", relocked.as_bytes())], &["--no-renames"])
            .await;
        let moved = String::from_utf8(result)
            .unwrap()
            .replace("\n name = \"requests\"\n", "\n name = \"requestz\"\n");
        assert!(moved.contains("requestz"), "{moved}");
        let Err(Error::Refused(message)) = fixture.check(b"", moved.as_bytes(), &["."]).await
        else {
            panic!("a patch that does not apply was accepted");
        };
        assert!(
            message.contains("does not apply to the commit it started from"),
            "{message}"
        );

        let Err(Error::Refused(message)) = fixture.check(b"", b"not a patch\n", &["."]).await
        else {
            panic!("text that is not a patch was accepted");
        };
        assert!(message.contains("cannot be read"), "{message}");
    }

    #[tokio::test]
    async fn the_base_must_be_a_full_object_id() {
        let fixture = Fixture::new().await;
        for base in [
            "HEAD",
            "main",
            &fixture.base[..12],
            &fixture.base.to_uppercase(),
        ] {
            let Err(Error::Refused(message)) =
                check(&fixture.git, base, b"", b"", &[".".to_string()]).await
            else {
                panic!("{base} was accepted as a base");
            };
            assert!(message.contains("must be a full object id"), "{message}");
        }
        assert!(is_object_id(&fixture.base));
        assert!(is_object_id(&"a".repeat(64)));
    }
}
