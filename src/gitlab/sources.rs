//! Where a lockfile fetches code from. A lock job runs repository code, so
//! the lockfile it returns is checked against the one it started from: the
//! relock may pick other releases from the places the project already
//! fetches from, but it never adds a place.
//!
//! Every string in the parsed lockfile is classified, not a list of known
//! fields, so a location field this module does not name is still caught:
//! anything that looks like a location and cannot be placed is refused.

use std::collections::BTreeSet;

use serde_json::Value;
use url::Url;

use crate::lockfile::LockfileType;

/// A place a lockfile fetches from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Location {
    /// A registry or artifact download, compared by origin (scheme, host,
    /// port): releases come and go on a registry the project already uses.
    Download(String),
    /// Where code itself lives (a git repository, a local path, a direct
    /// archive), compared in full without its revision: a shared host such
    /// as a public forge says nothing about whose code it is.
    Source(String),
}

/// The lockfile formats whose locations can be read.
#[derive(Debug, Clone, Copy)]
enum Format {
    Uv,
    Npm,
    Cargo,
}

/// Refuses `result` when it fetches from a place `base` does not, or when
/// upd cannot tell where a lockfile of this kind fetches from.
pub fn check(kind: LockfileType, base: &str, result: &str) -> Result<(), String> {
    let name = kind.filename();
    let known = locations(kind, base).map_err(|error| format!("the original {name}: {error}"))?;
    let found =
        locations(kind, result).map_err(|error| format!("the regenerated {name}: {error}"))?;
    match found.difference(&known).next() {
        None => Ok(()),
        Some(Location::Download(origin)) => Err(format!(
            "the regenerated {name} downloads from {origin}, which the original never does"
        )),
        Some(Location::Source(source)) => Err(format!(
            "the regenerated {name} takes code from {source}, which the original never does"
        )),
    }
}

/// Whether upd can check where a lockfile of `kind` fetches from.
pub fn is_checkable(kind: LockfileType) -> bool {
    format(kind).is_some()
}

fn format(kind: LockfileType) -> Option<Format> {
    match kind {
        LockfileType::UvLock => Some(Format::Uv),
        LockfileType::PackageLockJson | LockfileType::NpmShrinkwrap => Some(Format::Npm),
        LockfileType::CargoLock => Some(Format::Cargo),
        LockfileType::PoetryLock
        | LockfileType::YarnLock
        | LockfileType::PnpmLock
        | LockfileType::BunLock
        | LockfileType::BunLockb
        | LockfileType::GoSum
        | LockfileType::GemfileLock
        | LockfileType::PackagesLockJson
        | LockfileType::TerraformLock => None,
    }
}

fn locations(kind: LockfileType, text: &str) -> Result<BTreeSet<Location>, String> {
    let format = format(kind)
        .ok_or_else(|| format!("upd cannot check where {} fetches from", kind.filename()))?;
    let document: Value = match format {
        Format::Uv | Format::Cargo => {
            toml::from_str(text).map_err(|error| format!("not valid TOML: {error}"))?
        }
        Format::Npm => {
            serde_json::from_str(text).map_err(|error| format!("not valid JSON: {error}"))?
        }
    };
    let mut found = BTreeSet::new();
    walk(format, &document, &mut Vec::new(), &mut found)?;
    Ok(found)
}

/// Collects the location of every string under `value`, map keys included;
/// `keys` is the path of map keys leading to it, array positions left out.
fn walk(
    format: Format,
    value: &Value,
    keys: &mut Vec<String>,
    found: &mut BTreeSet<Location>,
) -> Result<(), String> {
    match value {
        Value::Object(fields) => {
            for (key, field) in fields {
                found.extend(scheme_location(key)?);
                keys.push(key.clone());
                let walked = walk(format, field, keys, found);
                keys.pop();
                walked?;
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(format, item, keys, found)?;
            }
        }
        Value::String(text) => found.extend(
            classify(format, keys, text)
                .map_err(|error| format!("{error} at {}", keys.join(".")))?,
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

/// The location `text` names where `keys` places it, if any.
fn classify(format: Format, keys: &[String], text: &str) -> Result<Option<Location>, String> {
    let key = keys.last().map(String::as_str);
    let parent = keys.len().checked_sub(2).map(|index| keys[index].as_str());
    match format {
        Format::Uv => match key {
            Some("git") => Ok(Some(Location::Source(without_revision(text)))),
            Some("path" | "directory" | "editable" | "virtual") => {
                Ok(Some(Location::Source(text.to_string())))
            }
            // An sdist or wheel is an artifact on an index; any other URL is
            // a direct reference to an archive of someone's code.
            Some("url") if matches!(parent, Some("sdist" | "wheels")) => {
                Ok(Some(Location::Download(origin(text)?)))
            }
            Some("url") => Ok(Some(Location::Source(text.to_string()))),
            // An index is a URL or a local directory of distributions.
            Some("registry" | "index") => match scheme_location(text)? {
                Some(location) => Ok(Some(location)),
                None => Ok(Some(Location::Source(text.to_string()))),
            },
            _ => scheme_location(text),
        },
        Format::Npm => {
            if is_npm_funding(keys) {
                return Ok(None);
            }
            match (key, scheme_location(text)?) {
                (_, Some(location)) => Ok(Some(location)),
                // A linked package resolves to a relative directory.
                (Some("resolved"), None) => Ok(Some(Location::Source(text.to_string()))),
                (_, None) => Ok(None),
            }
        }
        Format::Cargo => match key {
            Some("source") => scheme_location(text)?
                .map(Some)
                .ok_or_else(|| format!("unrecognised source '{text}'")),
            // `name version (source)`, the source present when it is needed
            // to tell two packages apart.
            Some("dependencies") => match text.split_once(" (") {
                Some((_, source)) => {
                    let source = source
                        .strip_suffix(')')
                        .ok_or_else(|| format!("unrecognised dependency '{text}'"))?;
                    scheme_location(source)?
                        .map(Some)
                        .ok_or_else(|| format!("unrecognised dependency source '{source}'"))
                }
                None => scheme_location(text),
            },
            _ => scheme_location(text),
        },
    }
}

/// A `funding` entry of a package-lock package: where to donate, which no
/// install fetches from.
fn is_npm_funding(keys: &[String]) -> bool {
    match keys {
        [packages, _, funding] => packages == "packages" && funding == "funding",
        [packages, _, funding, field] => {
            packages == "packages"
                && funding == "funding"
                && matches!(field.as_str(), "url" | "type")
        }
        _ => false,
    }
}

/// The location a value names by its scheme, whatever field holds it;
/// `None` for a value that names no location. A value that looks like a
/// location but has no known scheme is refused rather than ignored.
fn scheme_location(text: &str) -> Result<Option<Location>, String> {
    const GIT: [&str; 7] = [
        "git+",
        "git://",
        "git@",
        "ssh://",
        "github:",
        "gitlab:",
        "bitbucket:",
    ];
    const LOCAL: [&str; 4] = ["file:", "link:", "path+", "portal:"];
    if let Some(index) = text
        .strip_prefix("registry+")
        .or_else(|| text.strip_prefix("sparse+"))
    {
        return Ok(Some(Location::Download(origin(index)?)));
    }
    if GIT.iter().any(|prefix| text.starts_with(prefix)) || text.starts_with("gist:") {
        return Ok(Some(Location::Source(without_revision(text))));
    }
    if LOCAL.iter().any(|prefix| text.starts_with(prefix)) {
        return Ok(Some(Location::Source(text.to_string())));
    }
    if text.starts_with("http://") || text.starts_with("https://") {
        return Ok(Some(Location::Download(origin(text)?)));
    }
    if text.contains("://") {
        return Err(format!("unrecognised location '{text}'"));
    }
    Ok(None)
}

/// The scheme, host and port of an HTTP(S) URL.
fn origin(text: &str) -> Result<String, String> {
    let url = Url::parse(text).map_err(|error| format!("unreadable URL '{text}': {error}"))?;
    let (scheme, Some(host), Some(port)) =
        (url.scheme(), url.host_str(), url.port_or_known_default())
    else {
        return Err(format!("unrecognised URL '{text}'"));
    };
    if !matches!(scheme, "http" | "https") {
        return Err(format!("unrecognised URL '{text}'"));
    }
    Ok(format!("{scheme}://{host}:{port}"))
}

/// A repository location without the branch, tag or commit it pins.
fn without_revision(text: &str) -> String {
    let end = text.find(['#', '?']).unwrap_or(text.len());
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UV_BASE: &str = r#"version = 1
requires-python = ">=3.12"

[[package]]
name = "demo"
version = "0.1.0"
source = { editable = "." }
dependencies = [{ name = "requests" }, { name = "tool" }]

[[package]]
name = "requests"
version = "2.32.3"
source = { registry = "https://pypi.org/simple" }
sdist = { url = "https://files.pythonhosted.org/packages/r/requests-2.32.3.tar.gz", hash = "sha256:aa", size = 1 }
wheels = [{ url = "https://files.pythonhosted.org/packages/r/requests-2.32.3-py3-none-any.whl", hash = "sha256:bb", size = 1 }]

[[package]]
name = "tool"
version = "1.0.0"
source = { git = "https://github.com/acme/tool?rev=v1#1111111" }
"#;

    #[test]
    fn a_relock_within_the_known_places_is_accepted() {
        assert_eq!(check(LockfileType::UvLock, UV_BASE, UV_BASE), Ok(()));
        // A newer release from the same index, and the same repository at
        // another revision.
        let relocked = UV_BASE
            .replace("2.32.3", "2.32.4")
            .replace("?rev=v1#1111111", "?rev=v2#2222222");
        assert_eq!(check(LockfileType::UvLock, UV_BASE, &relocked), Ok(()));
    }

    #[test]
    fn a_uv_lock_that_adds_a_place_is_refused() {
        for (what, from, to, names) in [
            (
                "another index",
                r#"source = { registry = "https://pypi.org/simple" }"#,
                r#"source = { registry = "https://pypi.example.test/simple" }"#,
                "https://pypi.example.test:443",
            ),
            (
                "another repository on the same forge",
                "https://github.com/acme/tool?rev=v1#1111111",
                "https://github.com/other/tool?rev=v1#1111111",
                "https://github.com/other/tool",
            ),
            // The index's artifact host, moved into a direct reference:
            // same origin, but a direct archive is compared in full.
            (
                "a direct archive on a known download host",
                r#"source = { git = "https://github.com/acme/tool?rev=v1#1111111" }"#,
                r#"source = { url = "https://files.pythonhosted.org/packages/t/tool.tar.gz" }"#,
                "https://files.pythonhosted.org/packages/t/tool.tar.gz",
            ),
            // The forge hosts a known repository, which does not make it a
            // download host.
            (
                "a download from a host only known for a repository",
                "https://files.pythonhosted.org/packages/r/requests-2.32.3-py3-none-any.whl",
                "https://github.com/acme/tool/releases/tool-py3-none-any.whl",
                "https://github.com:443",
            ),
            (
                "another local directory",
                r#"source = { editable = "." }"#,
                r#"source = { editable = "../elsewhere" }"#,
                "../elsewhere",
            ),
            (
                "a location in a field upd does not name",
                "requires-python",
                "mirror = \"s3://bucket/simple\"\nrequires-python",
                "s3://bucket/simple",
            ),
        ] {
            let result = UV_BASE.replacen(from, to, 1);
            assert_ne!(result, UV_BASE, "{what}: the edit applies");
            let error = check(LockfileType::UvLock, UV_BASE, &result)
                .expect_err(&format!("{what} was accepted"));
            assert!(error.contains(names), "{what}: {error}");
        }
    }

    const NPM_BASE: &str = r#"{
  "name": "demo",
  "lockfileVersion": 3,
  "packages": {
    "": {"name": "demo", "dependencies": {"left-pad": "^1.3.0", "shared": "file:../shared"}},
    "node_modules/left-pad": {
      "version": "1.3.0",
      "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
      "integrity": "sha512-aa",
      "funding": {"type": "github", "url": "https://github.com/sponsors/left-pad"}
    },
    "node_modules/shared": {"resolved": "../shared", "link": true},
    "node_modules/tool": {
      "version": "1.0.0",
      "resolved": "git+ssh://git@github.com/acme/tool.git#1111111"
    }
  }
}"#;

    #[test]
    fn a_package_lock_relock_within_the_known_places_is_accepted() {
        let relocked = NPM_BASE
            .replace("1.3.0", "1.3.1")
            .replace("#1111111", "#2222222")
            .replace("sponsors/left-pad", "sponsors/someone-else")
            .replace(
                "https://github.com/sponsors",
                "https://funding.example.test",
            );
        assert_eq!(
            check(LockfileType::PackageLockJson, NPM_BASE, &relocked),
            Ok(())
        );
    }

    #[test]
    fn a_package_lock_that_adds_a_place_is_refused() {
        for (what, from, to, names) in [
            (
                "another registry",
                "https://registry.npmjs.org/left-pad",
                "https://registry.example.test/left-pad",
                "https://registry.example.test:443",
            ),
            (
                "another repository",
                "git+ssh://git@github.com/acme/tool.git",
                "git+ssh://git@github.com/other/tool.git",
                "git+ssh://git@github.com/other/tool.git",
            ),
            (
                "another linked directory",
                r#""resolved": "../shared""#,
                r#""resolved": "../../elsewhere""#,
                "../../elsewhere",
            ),
            // A donation link carries no weight once it names a download.
            (
                "the funding host as a download",
                "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
                "https://github.com/sponsors/left-pad.tgz",
                "https://github.com:443",
            ),
            (
                "a dependency spec naming a new repository",
                r#""left-pad": "^1.3.0""#,
                r#""left-pad": "github:other/left-pad""#,
                "github:other/left-pad",
            ),
        ] {
            let result = NPM_BASE.replacen(from, to, 1);
            assert_ne!(result, NPM_BASE, "{what}: the edit applies");
            let error = check(LockfileType::PackageLockJson, NPM_BASE, &result)
                .expect_err(&format!("{what} was accepted"));
            assert!(error.contains(names), "{what}: {error}");
        }
    }

    const CARGO_BASE: &str = r#"version = 4

[[package]]
name = "demo"
version = "0.1.0"
dependencies = [
 "serde",
 "tool 1.0.0 (git+https://github.com/acme/tool?branch=main#1111111)",
]

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aa"

[[package]]
name = "tool"
version = "1.0.0"
source = "git+https://github.com/acme/tool?branch=main#1111111"
"#;

    #[test]
    fn a_cargo_relock_within_the_known_places_is_accepted() {
        let relocked = CARGO_BASE
            .replace("1.0.228", "1.0.229")
            .replace("#1111111", "#2222222");
        assert_eq!(
            check(LockfileType::CargoLock, CARGO_BASE, &relocked),
            Ok(())
        );
    }

    #[test]
    fn a_cargo_lock_that_adds_a_place_is_refused() {
        for (what, from, to, names) in [
            (
                "another registry",
                "registry+https://github.com/rust-lang/crates.io-index",
                "sparse+https://crates.example.test/index/",
                "https://crates.example.test:443",
            ),
            (
                "another repository in a source",
                r#"source = "git+https://github.com/acme/tool"#,
                r#"source = "git+https://github.com/other/tool"#,
                "git+https://github.com/other/tool",
            ),
            (
                "another repository in a dependency entry",
                "tool 1.0.0 (git+https://github.com/acme/tool",
                "tool 1.0.0 (git+https://github.com/other/tool",
                "git+https://github.com/other/tool",
            ),
            (
                "a source upd cannot place",
                "git+https://github.com/acme/tool?branch=main#1111111\"\n",
                "hg+https://hg.example.test/tool\"\n",
                "hg+https://hg.example.test/tool",
            ),
        ] {
            let result = CARGO_BASE.replacen(from, to, 1);
            assert_ne!(result, CARGO_BASE, "{what}: the edit applies");
            let error = check(LockfileType::CargoLock, CARGO_BASE, &result)
                .expect_err(&format!("{what} was accepted"));
            assert!(error.contains(names), "{what}: {error}");
        }
    }

    #[test]
    fn a_lockfile_upd_cannot_read_is_refused() {
        assert!(check(LockfileType::GoSum, "", "").is_err());
        assert!(!is_checkable(LockfileType::GoSum));
        assert!(is_checkable(LockfileType::UvLock));
        assert!(check(LockfileType::CargoLock, CARGO_BASE, "[[package").is_err());
        assert!(check(LockfileType::CargoLock, "[[package", CARGO_BASE).is_err());
    }
}
