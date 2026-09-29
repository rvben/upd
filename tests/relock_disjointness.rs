//! A relock stays disjoint from the manifest edits that did not trigger it:
//! moving one direct dependency across a major version must not disturb a
//! transitive dependency that is locked at an older, but still compatible,
//! version reached through some other direct dependency. This is what lets
//! two independent update lanes (a major-only lane and an ordinary lane)
//! each carry only the lockfile lines their own manifest edit requires,
//! instead of the two lanes' diffs colliding on a package neither one
//! touched.
//!
//! Each format is driven through the same entry point production uses,
//! `upd::lockfile::regenerate_lockfile`, against a registry that never
//! leaves the machine: a `directory` source replacement for Cargo, a
//! `--find-links` wheel directory for uv, and a `wiremock` server on the
//! loopback interface for npm. Every fixture proves two things: the relock
//! actually ran against the registry (the changed dependency lands on its
//! new version, a positive control), and the registry could have moved the
//! unrelated dependency too (the tool's own full-upgrade command, run
//! directly, does move it, a negative control) so the disjointness is not
//! vacuous.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;
use upd::lockfile::{LockfileType, RegenOutcome, Relock, regenerate_lockfile};

/// Fails with the tool's own message when a relock did not succeed, instead
/// of a bare assertion that only names which outcome variant showed up.
fn assert_relock_ok(relock: &Relock) {
    assert!(
        matches!(relock.outcome, RegenOutcome::Ok(_)),
        "{}",
        relock
            .outcome
            .error_message()
            .unwrap_or_else(|| "relock did not report an error".to_string())
    );
}

/// Reads the locked version of `name` out of a TOML lockfile that holds it
/// as one of a `[[package]]` array, the shape both `Cargo.lock` and
/// `uv.lock` use.
fn toml_lock_version(path: &Path, name: &str) -> String {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let doc: toml::Table = text
        .parse()
        .unwrap_or_else(|e| panic!("{} is not valid TOML: {e}", path.display()));
    let packages = doc
        .get("package")
        .and_then(|value| value.as_array())
        .unwrap_or_else(|| panic!("{} has no [[package]] entries", path.display()));
    packages
        .iter()
        .find(|package| package.get("name").and_then(|v| v.as_str()) == Some(name))
        .and_then(|package| package.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("{name} not found in {}", path.display()))
        .to_string()
}

// ---------------------------------------------------------------------
// Cargo: a `directory` source replacement holding hand-written crates.
// ---------------------------------------------------------------------

/// Writes a vendored crate at `<vendor_dir>/<name>-<version>/`, the layout
/// `cargo vendor` produces and a `[source] directory` replacement reads
/// directly, with no real registry involved.
fn write_vendored_crate(vendor_dir: &Path, name: &str, version: &str, deps: &[(&str, &str)]) {
    let crate_dir = vendor_dir.join(format!("{name}-{version}"));
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    let mut manifest =
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n");
    if !deps.is_empty() {
        manifest.push_str("\n[dependencies]\n");
        for (dep_name, requirement) in deps {
            manifest.push_str(&format!("{dep_name} = \"{requirement}\"\n"));
        }
    }
    fs::write(crate_dir.join("Cargo.toml"), manifest).unwrap();
    fs::write(crate_dir.join("src/lib.rs"), "").unwrap();
    // A directory source checks this checksum only against what a prior
    // `Cargo.lock` entry recorded for the same package; nothing here was
    // ever fetched from a real registry to check it against, so a
    // placeholder of the right shape is all `cargo` needs to accept it.
    let checksum = "0".repeat(64);
    fs::write(
        crate_dir.join(".cargo-checksum.json"),
        format!("{{\"files\":{{}},\"package\":\"{checksum}\"}}"),
    )
    .unwrap();
}

/// Writes the project crate depending on `alpha` (at `alpha_requirement`)
/// and `beta`, plus the `.cargo/config.toml` that replaces crates.io with
/// the local `vendor/` directory and turns off network access outright.
fn write_cargo_project(root: &Path, alpha_requirement: &str) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nalpha = \"{alpha_requirement}\"\nbeta = \"1.0.0\"\n"
        ),
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "").unwrap();
    fs::create_dir_all(root.join(".cargo")).unwrap();
    fs::write(
        root.join(".cargo/config.toml"),
        "[source.crates-io]\n\
         replace-with = \"vendor\"\n\n\
         [source.vendor]\n\
         directory = \"vendor\"\n\n\
         [net]\n\
         offline = true\n",
    )
    .unwrap();
}

fn run_cargo(dir: &Path, args: &[&str]) -> Output {
    Command::new("cargo")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("cargo not found on PATH: {e}"))
}

fn cargo_lock_version(dir: &Path, name: &str) -> String {
    toml_lock_version(&dir.join("Cargo.lock"), name)
}

#[test]
fn cargo_relock_keeps_an_unrelated_transitive_at_its_locked_version() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let vendor = root.join("vendor");
    write_vendored_crate(&vendor, "alpha", "1.0.0", &[]);
    write_vendored_crate(&vendor, "alpha", "2.0.0", &[]);
    write_vendored_crate(&vendor, "beta", "1.0.0", &[("tau", "1.0.0")]);
    // Only tau 1.0.0 exists so far: the "before" lock is produced honestly,
    // not carved out of a registry that already offered the newer version.
    write_vendored_crate(&vendor, "tau", "1.0.0", &[]);
    write_cargo_project(root, "1.0.0");

    let before = run_cargo(root, &["generate-lockfile"]);
    assert!(
        before.status.success(),
        "{}",
        String::from_utf8_lossy(&before.stderr)
    );
    assert_eq!(cargo_lock_version(root, "alpha"), "1.0.0");
    assert_eq!(cargo_lock_version(root, "tau"), "1.0.0");

    // tau 1.1.0 becomes available and admissible through beta's caret
    // requirement, but nothing has relocked yet.
    write_vendored_crate(&vendor, "tau", "1.1.0", &[]);

    // Move alpha across its major, as upd's major lane would.
    let manifest = root.join("Cargo.toml");
    let updated = fs::read_to_string(&manifest)
        .unwrap()
        .replace("alpha = \"1.0.0\"", "alpha = \"2.0.0\"");
    fs::write(&manifest, updated).unwrap();

    let relock = regenerate_lockfile(
        &manifest,
        LockfileType::CargoLock,
        &["alpha".to_string()],
        None,
        false,
    );
    assert_relock_ok(&relock);

    assert_eq!(
        cargo_lock_version(root, "alpha"),
        "2.0.0",
        "positive control: the relock must have moved the changed dependency"
    );
    assert_eq!(
        cargo_lock_version(root, "tau"),
        "1.0.0",
        "tau is an unrelated transitive dependency and must stay at its locked version"
    );

    // Negative control: cargo's own full upgrade does move tau, proving
    // 1.1.0 was reachable and the property above is not vacuous.
    let negative = run_cargo(root, &["update"]);
    assert!(
        negative.status.success(),
        "{}",
        String::from_utf8_lossy(&negative.stderr)
    );
    assert_eq!(cargo_lock_version(root, "tau"), "1.1.0");
}

// ---------------------------------------------------------------------
// npm: a wiremock server standing in for the npm registry protocol.
// ---------------------------------------------------------------------

/// Packs `<name>@<version>` with the given `dependencies` object into a
/// gzipped tarball at `<out_dir>/<name>-<version>.tgz`, the same archive
/// shape `npm pack` produces.
fn npm_tarball(
    out_dir: &Path,
    name: &str,
    version: &str,
    dependencies: &serde_json::Value,
) -> Vec<u8> {
    let staging = out_dir.join(format!("{name}-{version}-src"));
    let package_dir = staging.join("package");
    fs::create_dir_all(&package_dir).unwrap();
    let package_json = serde_json::json!({
        "name": name,
        "version": version,
        "dependencies": dependencies,
    });
    fs::write(
        package_dir.join("package.json"),
        serde_json::to_vec(&package_json).unwrap(),
    )
    .unwrap();
    let tgz_path = out_dir.join(format!("{name}-{version}.tgz"));
    let output = Command::new("tar")
        .arg("-czf")
        .arg(&tgz_path)
        .arg("-C")
        .arg(&staging)
        .arg("package")
        .output()
        .unwrap_or_else(|e| panic!("tar not found on PATH: {e}"));
    assert!(
        output.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read(&tgz_path).unwrap()
}

/// Mounts `GET /<name>` (the packument) and `GET /<name>/-/<name>-<version>.tgz`
/// (the tarball) for every `(version, dependencies)` pair, in the shape the
/// npm registry protocol uses. Neither `integrity` nor `shasum` is set: npm
/// resolves and writes a `package-lock.json` fine without them against a
/// private registry, and leaving them out avoids pulling in a hashing crate
/// just for test fixtures.
async fn mount_npm_package(
    server: &wiremock::MockServer,
    work_dir: &Path,
    name: &str,
    versions: &[(&str, serde_json::Value)],
) {
    let mut versions_json = serde_json::Map::new();
    let mut latest = "";
    for (version, dependencies) in versions {
        let tarball = npm_tarball(work_dir, name, version, dependencies);
        let tarball_url = format!("{}/{name}/-/{name}-{version}.tgz", server.uri());
        versions_json.insert(
            (*version).to_string(),
            serde_json::json!({
                "name": name,
                "version": version,
                "dependencies": dependencies,
                "dist": { "tarball": tarball_url },
            }),
        );
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/{name}/-/{name}-{version}.tgz"
            )))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(tarball))
            .mount(server)
            .await;
        latest = version;
    }
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/{name}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": name,
                "dist-tags": { "latest": latest },
                "versions": versions_json,
            })),
        )
        .mount(server)
        .await;
}

/// Writes `package.json` (depending on `alpha` at `alpha_requirement` and
/// `beta` at `1.0.0`) and an `.npmrc` pointing npm at the mock registry with
/// an isolated cache and no audit/fund/update chatter.
fn write_npm_project(root: &Path, registry_uri: &str, alpha_requirement: &str) {
    fs::write(
        root.join("package.json"),
        format!(
            "{{\n  \"name\": \"demo\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {{\n    \
             \"alpha\": \"{alpha_requirement}\",\n    \"beta\": \"1.0.0\"\n  }}\n}}\n"
        ),
    )
    .unwrap();
    let cache_dir = root.join("npm-cache");
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(
        root.join(".npmrc"),
        format!(
            "registry={registry_uri}/\ncache={}\naudit=false\nfund=false\nupdate-notifier=false\n",
            cache_dir.display()
        ),
    )
    .unwrap();
}

fn run_npm(dir: &Path, args: &[&str]) -> Output {
    Command::new("npm")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("npm not found on PATH: {e}"))
}

fn npm_lock_version(dir: &Path, name: &str) -> String {
    let path = dir.join("package-lock.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let doc: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
    doc["packages"][format!("node_modules/{name}")]["version"]
        .as_str()
        .unwrap_or_else(|| panic!("{name} not found in {}", path.display()))
        .to_string()
}

#[tokio::test]
async fn npm_relock_keeps_an_unrelated_transitive_at_its_locked_version() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let registry_work = root.join("registry-src");
    fs::create_dir_all(&registry_work).unwrap();

    let server = wiremock::MockServer::start().await;
    mount_npm_package(
        &server,
        &registry_work,
        "alpha",
        &[("1.0.0", serde_json::json!({}))],
    )
    .await;
    mount_npm_package(
        &server,
        &registry_work,
        "beta",
        &[("1.0.0", serde_json::json!({ "tau": "^1.0.0" }))],
    )
    .await;
    // Only tau 1.0.0 exists so far: the "before" lock is produced honestly.
    mount_npm_package(
        &server,
        &registry_work,
        "tau",
        &[("1.0.0", serde_json::json!({}))],
    )
    .await;

    write_npm_project(root, &server.uri(), "1.0.0");

    let before = run_npm(
        root,
        &["install", "--package-lock-only", "--ignore-scripts"],
    );
    assert!(
        before.status.success(),
        "{}",
        String::from_utf8_lossy(&before.stderr)
    );
    assert_eq!(npm_lock_version(root, "alpha"), "1.0.0");
    assert_eq!(npm_lock_version(root, "tau"), "1.0.0");

    // Relax the registry: alpha 2.0.0 and tau 1.1.0 both become available
    // and admissible, but nothing has relocked yet. `reset` clears the
    // earlier mocks so the new, wider packuments are the only ones a
    // request can match (wiremock otherwise prefers the first-mounted mock
    // at equal priority, which would keep serving the narrower one).
    server.reset().await;
    mount_npm_package(
        &server,
        &registry_work,
        "alpha",
        &[
            ("1.0.0", serde_json::json!({})),
            ("2.0.0", serde_json::json!({})),
        ],
    )
    .await;
    mount_npm_package(
        &server,
        &registry_work,
        "beta",
        &[("1.0.0", serde_json::json!({ "tau": "^1.0.0" }))],
    )
    .await;
    mount_npm_package(
        &server,
        &registry_work,
        "tau",
        &[
            ("1.0.0", serde_json::json!({})),
            ("1.1.0", serde_json::json!({})),
        ],
    )
    .await;

    // Move alpha across its major, as upd's major lane would.
    let manifest = root.join("package.json");
    let updated = fs::read_to_string(&manifest)
        .unwrap()
        .replace("\"alpha\": \"1.0.0\"", "\"alpha\": \"2.0.0\"");
    fs::write(&manifest, updated).unwrap();

    let relock = regenerate_lockfile(
        &manifest,
        LockfileType::PackageLockJson,
        &["alpha".to_string()],
        None,
        false,
    );
    assert_relock_ok(&relock);

    assert_eq!(
        npm_lock_version(root, "alpha"),
        "2.0.0",
        "positive control: the relock must have moved the changed dependency"
    );
    assert_eq!(
        npm_lock_version(root, "tau"),
        "1.0.0",
        "tau is an unrelated transitive dependency and must stay at its locked version"
    );

    // Negative control: npm's own full update does move tau, proving 1.1.0
    // was reachable and the property above is not vacuous.
    let negative = run_npm(root, &["update", "--package-lock-only"]);
    assert!(
        negative.status.success(),
        "{}",
        String::from_utf8_lossy(&negative.stderr)
    );
    assert_eq!(npm_lock_version(root, "tau"), "1.1.0");
}

// ---------------------------------------------------------------------
// uv: a `--find-links` directory of hand-built wheels.
// ---------------------------------------------------------------------

/// Writes a minimal pure-Python wheel `<name>-<version>-py3-none-any.whl`
/// into `wheel_dir` with the given `Requires-Dist` entries, using the
/// system `zip`. `uv lock` reads metadata straight out of the archive and
/// never imports the module, so the package body can stay a comment.
fn write_wheel(wheel_dir: &Path, name: &str, version: &str, requires: &[&str]) {
    let staging = wheel_dir.join(format!("{name}-{version}-src"));
    let dist_info_name = format!("{name}-{version}.dist-info");
    let dist_info = staging.join(&dist_info_name);
    let package_dir = staging.join(name);
    fs::create_dir_all(&dist_info).unwrap();
    fs::create_dir_all(&package_dir).unwrap();
    fs::write(package_dir.join("__init__.py"), format!("# {name}\n")).unwrap();

    let mut metadata = format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n");
    for requirement in requires {
        metadata.push_str(&format!("Requires-Dist: {requirement}\n"));
    }
    fs::write(dist_info.join("METADATA"), metadata).unwrap();
    fs::write(
        dist_info.join("WHEEL"),
        "Wheel-Version: 1.0\nGenerator: relock-disjointness-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    )
    .unwrap();
    // A real RECORD carries a hash and size per file; uv only needs the
    // entries to exist, and a blank hash/size is valid for a file whose
    // integrity the writer declines to record.
    fs::write(
        dist_info.join("RECORD"),
        format!(
            "{name}/__init__.py,,\n{dist_info_name}/METADATA,,\n{dist_info_name}/WHEEL,,\n{dist_info_name}/RECORD,,\n"
        ),
    )
    .unwrap();

    let wheel_path = wheel_dir.join(format!("{name}-{version}-py3-none-any.whl"));
    let output = Command::new("zip")
        .arg("-rq")
        .arg(&wheel_path)
        .arg(name)
        .arg(&dist_info_name)
        .current_dir(&staging)
        .output()
        .unwrap_or_else(|e| panic!("zip not found on PATH: {e}"));
    assert!(
        output.status.success(),
        "zip failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Writes `pyproject.toml` (depending on `alpha` at `alpha_requirement` and
/// `beta` at `1.0.0`), configured entirely through `[tool.uv]` so the plain
/// `uv lock` production runs needs no environment variables at all: no
/// index, an isolated cache, and no attempt to download a Python.
fn write_uv_project(root: &Path, wheel_dir: &Path, alpha_requirement: &str) {
    fs::write(
        root.join("pyproject.toml"),
        format!(
            "[project]\n\
             name = \"demo\"\n\
             version = \"0.1.0\"\n\
             requires-python = \">=3.8\"\n\
             dependencies = [\n  \"alpha=={alpha_requirement}\",\n  \"beta==1.0.0\",\n]\n\n\
             [tool.uv]\n\
             no-index = true\n\
             python-downloads = \"never\"\n\
             find-links = [\"{wheels}\"]\n\
             cache-dir = \"{cache}\"\n",
            wheels = wheel_dir.display(),
            cache = root.join("uv-cache").display(),
        ),
    )
    .unwrap();
}

fn run_uv(dir: &Path, args: &[&str]) -> Output {
    Command::new("uv")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("uv not found on PATH: {e}"))
}

fn uv_lock_version(dir: &Path, name: &str) -> String {
    toml_lock_version(&dir.join("uv.lock"), name)
}

#[test]
fn uv_relock_keeps_an_unrelated_transitive_at_its_locked_version() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let wheels = root.join("wheels");
    fs::create_dir_all(&wheels).unwrap();
    write_wheel(&wheels, "alpha", "1.0.0", &[]);
    write_wheel(&wheels, "alpha", "2.0.0", &[]);
    write_wheel(&wheels, "beta", "1.0.0", &["tau>=1.0.0,<2.0.0"]);
    // Only tau 1.0.0 exists so far: the "before" lock is produced honestly.
    write_wheel(&wheels, "tau", "1.0.0", &[]);
    write_uv_project(root, &wheels, "1.0.0");

    let before = run_uv(root, &["lock"]);
    assert!(
        before.status.success(),
        "{}",
        String::from_utf8_lossy(&before.stderr)
    );
    assert_eq!(uv_lock_version(root, "alpha"), "1.0.0");
    assert_eq!(uv_lock_version(root, "tau"), "1.0.0");

    // tau 1.1.0 becomes available and admissible through beta's range, but
    // nothing has relocked yet.
    write_wheel(&wheels, "tau", "1.1.0", &[]);

    // Move alpha across its major, as upd's major lane would.
    let manifest = root.join("pyproject.toml");
    let updated = fs::read_to_string(&manifest)
        .unwrap()
        .replace("alpha==1.0.0", "alpha==2.0.0");
    fs::write(&manifest, updated).unwrap();

    let relock = regenerate_lockfile(
        &manifest,
        LockfileType::UvLock,
        &["alpha".to_string()],
        None,
        false,
    );
    assert_relock_ok(&relock);

    assert_eq!(
        uv_lock_version(root, "alpha"),
        "2.0.0",
        "positive control: the relock must have moved the changed dependency"
    );
    assert_eq!(
        uv_lock_version(root, "tau"),
        "1.0.0",
        "tau is an unrelated transitive dependency and must stay at its locked version"
    );

    // Negative control: uv's own full upgrade does move tau, proving 1.1.0
    // was reachable and the property above is not vacuous.
    let negative = run_uv(root, &["lock", "--upgrade"]);
    assert!(
        negative.status.success(),
        "{}",
        String::from_utf8_lossy(&negative.stderr)
    );
    assert_eq!(uv_lock_version(root, "tau"), "1.1.0");
}
