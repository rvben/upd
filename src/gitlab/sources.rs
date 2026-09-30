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
    /// A file a registry serves, compared by origin (scheme, host, port):
    /// every release has its own file, and a registry the project already
    /// uses serves its new releases from the same host.
    Download(String),
    /// A registry or index, compared by its full URL without credentials:
    /// one host can serve several registries, each its own place.
    Registry(String),
    /// Where code itself lives (a git repository, a local path, a direct
    /// archive), compared in full without its revision: a shared host such
    /// as a public forge says nothing about whose code it is.
    Source(String),
    /// Whatever registry npm is configured with when it installs: a package
    /// it installs without a `resolved` URL comes from there, which need not
    /// be any registry the lockfile names.
    ConfiguredRegistry,
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
        Some(Location::Registry(registry)) => Err(format!(
            "the regenerated {name} resolves from the registry {registry}, which the original never does"
        )),
        Some(Location::Source(source)) => Err(format!(
            "the regenerated {name} takes code from {source}, which the original never does"
        )),
        Some(Location::ConfiguredRegistry) => Err(format!(
            "the regenerated {name} leaves a package to the registry npm is configured with, which the original never does"
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
    if matches!(format, Format::Npm) && has_unresolved_npm_package(&document) {
        found.insert(Location::ConfiguredRegistry);
    }
    Ok(found)
}

/// Whether npm installs some package of a package-lock without a
/// `resolved` URL, and so from the registry it is configured with. A
/// bundled package ships inside another, a workspace is a directory of the
/// project, a linked package resolves to its directory, and a package
/// whose version names a repository or path is classified by that version
/// instead.
fn has_unresolved_npm_package(document: &Value) -> bool {
    let installed =
        |path: &str| path.starts_with("node_modules/") || path.contains("/node_modules/");
    document
        .get("packages")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .any(|(path, entry)| installed(path) && is_unresolved(entry))
        || has_unresolved_legacy_package(document.get("dependencies"))
}

/// The same for a legacy lockfile's `dependencies` tree, nested ones
/// included.
fn has_unresolved_legacy_package(dependencies: Option<&Value>) -> bool {
    dependencies
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .any(|(_, entry)| {
            is_unresolved(entry) || has_unresolved_legacy_package(entry.get("dependencies"))
        })
}

fn is_unresolved(entry: &Value) -> bool {
    let Value::Object(fields) = entry else {
        return false;
    };
    let flagged = |name: &str| fields.get(name) == Some(&Value::Bool(true));
    // npm reads a `resolved` that is not a string as none.
    !matches!(fields.get("resolved"), Some(Value::String(_)))
        && !flagged("inBundle")
        && !flagged("bundled")
        && match fields.get("version") {
            Some(Value::String(version)) => matches!(npm_spec(version), Ok(None)),
            _ => true,
        }
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
            Some("git") => Ok(Some(source(&without_revision(text)))),
            Some("path" | "directory" | "editable" | "virtual") => Ok(Some(source(text))),
            // An sdist or wheel is an artifact on an index; any other URL is
            // a direct reference to an archive of someone's code.
            Some("url") if matches!(parent, Some("sdist" | "wheels")) => {
                Ok(Some(Location::Download(origin(text)?)))
            }
            Some("url") => Ok(Some(source(text))),
            // An index is a URL or a local directory of distributions.
            Some("registry" | "index") if is_http(text) => Ok(Some(registry(text)?)),
            Some("registry" | "index") => match scheme_location(text)? {
                Some(location) => Ok(Some(location)),
                None => Ok(Some(source(text))),
            },
            _ => scheme_location(text),
        },
        Format::Npm => {
            if is_npm_funding(keys) {
                return Ok(None);
            }
            if is_npm_spec(keys) {
                return npm_spec(text);
            }
            if key == Some("resolved") && is_http(text) {
                return match npm_registry(text)? {
                    Some(registry) => Ok(Some(registry)),
                    None => Ok(Some(source(text))),
                };
            }
            match (key, scheme_location(text)?) {
                (_, Some(location)) => Ok(Some(location)),
                // A linked package resolves to a relative directory.
                (Some("resolved"), None) => Ok(Some(source(text))),
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

/// Whether `keys` places a value npm reads as a package spec: a package's
/// `version`, which a legacy lockfile installs from, or a spec in one of
/// its dependency maps, which npm resolves when the package it points at
/// does not satisfy it.
fn is_npm_spec(keys: &[String]) -> bool {
    const MAPS: [&str; 4] = [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ];
    if keys.first().is_some_and(|key| key == "packages") {
        return match keys {
            [_, _, field] => field == "version",
            [_, _, map, _] => MAPS.contains(&map.as_str()),
            _ => false,
        };
    }
    // A legacy entry sits under `dependencies.<name>`, nested entries
    // under `dependencies.<name>` again.
    let entry = match keys {
        [entry @ .., field] if field == "version" => entry,
        [entry @ .., requires, _] if requires == "requires" => entry,
        _ => return false,
    };
    !entry.is_empty()
        && entry
            .chunks(2)
            .all(|pair| matches!(pair, [dependencies, _] if dependencies == "dependencies"))
}

/// The place an npm package spec fetches from, read as npm reads it. A
/// version, range or tag names the configured registry, and so does an
/// `npm:` alias, which npm allows only for registry packages. `user/repo`
/// is a GitHub repository. Any other spec with a slash, or one that starts
/// like a path (`.`, `~/`) or names a tarball, is a local path.
fn npm_spec(text: &str) -> Result<Option<Location>, String> {
    if text.starts_with("npm:") {
        return Ok(None);
    }
    if let Some(location) = scheme_location(text)? {
        return Ok(Some(location));
    }
    if is_github_shorthand(text) {
        return Ok(Some(source(&without_revision(&format!("github:{text}")))));
    }
    let local = text.contains(['/', '\\'])
        || text.starts_with('.')
        || text.starts_with("~/")
        || [".tgz", ".tar.gz", ".tar"]
            .iter()
            .any(|suffix| text.ends_with(suffix));
    if local {
        return Ok(Some(source(text)));
    }
    if text.contains(':') {
        return Err(format!("unrecognised package spec '{text}'"));
    }
    Ok(None)
}

/// Whether `text` is npm's GitHub shorthand, `user/repo` with an optional
/// `#ref`.
fn is_github_shorthand(text: &str) -> bool {
    let path = text.split_once('#').map_or(text, |(path, _)| path);
    let Some((user, repo)) = path.split_once('/') else {
        return false;
    };
    let plain = |part: &str, forbidden: &[char]| {
        !part.is_empty()
            && !part
                .chars()
                .any(|c| c.is_whitespace() || forbidden.contains(&c))
    };
    !user.starts_with(['.', '-'])
        && plain(user, &['@', '%', '/', ':'])
        && plain(repo, &['@', '%', '/'])
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
        return Ok(Some(registry(index)?));
    }
    if GIT.iter().any(|prefix| text.starts_with(prefix)) || text.starts_with("gist:") {
        return Ok(Some(source(&without_revision(text))));
    }
    if LOCAL.iter().any(|prefix| text.starts_with(prefix)) {
        return Ok(Some(source(text)));
    }
    // Outside the fields that name a registry or a file it serves, a URL
    // is a direct archive of someone's code.
    if is_http(text) {
        return Ok(Some(source(text)));
    }
    if text.contains("://") {
        return Err(format!("unrecognised location '{text}'"));
    }
    Ok(None)
}

fn is_http(text: &str) -> bool {
    text.starts_with("http://") || text.starts_with("https://")
}

/// `text` as an HTTP(S) URL with a host.
fn http_url(text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|error| format!("unreadable URL '{text}': {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return Err(format!("unrecognised URL '{text}'"));
    }
    Ok(url)
}

/// The scheme, host and port of an HTTP(S) URL.
fn origin(text: &str) -> Result<String, String> {
    let url = http_url(text)?;
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return Err(format!("unrecognised URL '{text}'"));
    };
    Ok(format!("{}://{host}:{port}", url.scheme()))
}

/// The registry an HTTP(S) URL names, without credentials.
fn registry(text: &str) -> Result<Location, String> {
    let mut url = http_url(text)?;
    strip_credentials(&mut url);
    Ok(Location::Registry(url.to_string()))
}

/// The registry an npm package tarball comes from: the URL up to the
/// package name, for `<registry>/<name>/-/<name>-<version>.tgz`, with
/// `@<scope>/` before the name, and before the file name too on registries
/// that repeat it there. `None` for any other URL, which then names an
/// archive of its own.
fn npm_registry(text: &str) -> Result<Option<Location>, String> {
    let mut url = http_url(text)?;
    let segments: Vec<String> = url
        .path_segments()
        .map(|segments| segments.map(str::to_string).collect())
        .unwrap_or_default();
    let Some(dash) = segments.iter().rposition(|segment| segment == "-") else {
        return Ok(None);
    };
    let (name, tail) = (&segments[..dash], &segments[dash + 1..]);
    let (scope, base) = match name {
        [.., scope, base] if scope.starts_with('@') => (Some(scope), base),
        [.., base] => (None, base),
        [] => return Ok(None),
    };
    let file = match (tail, scope) {
        ([file], _) => file,
        ([file_scope, file], Some(scope)) if file_scope == scope => file,
        _ => return Ok(None),
    };
    if !(file.starts_with(&format!("{base}-")) && file.ends_with(".tgz")) {
        return Ok(None);
    }
    let start = dash - 1 - usize::from(scope.is_some());
    url.set_path(&segments[..start].join("/"));
    url.set_query(None);
    url.set_fragment(None);
    strip_credentials(&mut url);
    Ok(Some(Location::Registry(url.to_string())))
}

/// Where code lives, without the credentials a URL may carry: they choose
/// access, not the code, and must not be repeated in a refusal. An SSH user
/// such as `git@` is the account, not a secret, and stays; over HTTP a bare
/// user is how a token is passed.
fn source(text: &str) -> Location {
    match Url::parse(text) {
        Ok(mut url) if url.has_host() => {
            strip_credentials(&mut url);
            Location::Source(url.to_string())
        }
        _ => Location::Source(text.to_string()),
    }
}

/// Removes the password from `url`, and over HTTP the user too.
fn strip_credentials(url: &mut Url) {
    // Neither setter fails on a URL with a host.
    let _ = url.set_password(None);
    if url.scheme().ends_with("http") || url.scheme().ends_with("https") {
        let _ = url.set_username("");
    }
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
    fn every_format_without_a_reader_is_refused_even_unchanged() {
        let mut checkable = Vec::new();
        for kind in LockfileType::ALL {
            if is_checkable(kind) {
                checkable.push(kind.filename());
                continue;
            }
            let error = check(kind, "", "").unwrap_err();
            assert!(
                error.contains(&format!("cannot check where {}", kind.filename())),
                "{error}"
            );
        }
        assert_eq!(
            checkable,
            [
                "uv.lock",
                "package-lock.json",
                "npm-shrinkwrap.json",
                "Cargo.lock"
            ]
        );
    }

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
                "registry https://pypi.example.test/simple,",
            ),
            (
                "another index on the same host",
                r#"source = { registry = "https://pypi.org/simple" }"#,
                r#"source = { registry = "https://pypi.org/attacker/simple" }"#,
                "https://pypi.org/attacker/simple",
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

    #[test]
    fn credentials_do_not_make_a_place_and_are_never_repeated() {
        let base = UV_BASE
            .replace(
                "https://pypi.org/simple",
                "https://reader:index-secret@pypi.example.test/simple",
            )
            .replace(
                "https://github.com/acme/tool",
                "https://repo-secret@git.example.test/acme/tool",
            );
        let relocked = base
            .replace("reader:index-secret@", "")
            .replace("repo-secret@", "oauth2:other-secret@");
        assert_eq!(check(LockfileType::UvLock, &base, &relocked), Ok(()));

        for (what, from, to, names) in [
            (
                "another repository",
                "repo-secret@git.example.test/acme/tool",
                "repo-secret@git.example.test/other/tool",
                "https://git.example.test/other/tool",
            ),
            (
                "another index",
                "index-secret@pypi.example.test",
                "index-secret@evil.example.test",
                "registry https://evil.example.test/simple,",
            ),
        ] {
            let result = base.replacen(from, to, 1);
            assert_ne!(result, base, "{what}: the edit applies");
            let error = check(LockfileType::UvLock, &base, &result)
                .expect_err(&format!("{what} was accepted"));
            assert!(error.contains(names), "{what}: {error}");
            assert!(!error.contains("secret"), "{what}: {error}");
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
    },
    "node_modules/archived": {
      "version": "1.0.0",
      "resolved": "https://github.com/acme/archived/archive/v1.tar.gz"
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
                "registry https://registry.example.test/,",
            ),
            (
                "another registry on the same host",
                "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
                "https://registry.npmjs.org/mirror/left-pad/-/left-pad-1.3.0.tgz",
                "https://registry.npmjs.org/mirror",
            ),
            (
                "another archive on the same host",
                "https://github.com/acme/archived/archive/v1.tar.gz",
                "https://github.com/attacker/evil/archive/v1.tar.gz",
                "https://github.com/attacker/evil/archive/v1.tar.gz",
            ),
            (
                "a dependency spec naming an archive on a known host",
                r#""left-pad": "^1.3.0""#,
                r#""left-pad": "https://github.com/attacker/evil/archive/v1.tar.gz""#,
                "https://github.com/attacker/evil/archive/v1.tar.gz",
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
                "the funding link as an archive",
                "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
                "https://github.com/sponsors/left-pad.tgz",
                "takes code from https://github.com/sponsors/left-pad.tgz,",
            ),
            (
                "the funding link as a registry",
                "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
                "https://github.com/sponsors/left-pad/-/left-pad-1.3.0.tgz",
                "registry https://github.com/sponsors,",
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

    const NPM_REGISTRIES: &str = r#"{
  "lockfileVersion": 3,
  "packages": {
    "node_modules/@acme/core": {
      "version": "1.0.0",
      "resolved": "https://registry.npmjs.org/@acme/core/-/core-1.0.0.tgz"
    },
    "node_modules/@acme/ui": {
      "version": "1.0.0",
      "resolved": "https://reader:registry-secret@gitlab.example.test/api/v4/projects/7/packages/npm/@acme/ui/-/@acme/ui-1.0.0.tgz"
    },
    "node_modules/left-pad": {
      "version": "1.3.0",
      "resolved": "https://art.example.test/api/npm/npm-remote/left-pad/-/left-pad-1.3.0.tgz?download=1"
    }
  }
}"#;

    #[test]
    fn a_package_lock_relock_on_each_registry_layout_is_accepted() {
        let relocked = NPM_REGISTRIES
            .replace("1.0.0", "1.1.0")
            .replace("1.3.0", "1.3.1")
            .replace("reader:registry-secret@", "");
        assert_eq!(
            check(LockfileType::PackageLockJson, NPM_REGISTRIES, &relocked),
            Ok(())
        );
    }

    #[test]
    fn another_registry_on_a_shared_host_is_another_place() {
        for (what, from, to, names) in [
            (
                "another project's registry on the same GitLab",
                "projects/7/",
                "projects/8/",
                "registry https://gitlab.example.test/api/v4/projects/8/packages/npm,",
            ),
            (
                "another repository on the same artifact server",
                "npm-remote/left-pad",
                "npm-untrusted/left-pad",
                "registry https://art.example.test/api/npm/npm-untrusted,",
            ),
        ] {
            let result = NPM_REGISTRIES.replacen(from, to, 1);
            assert_ne!(result, NPM_REGISTRIES, "{what}: the edit applies");
            let error = check(LockfileType::PackageLockJson, NPM_REGISTRIES, &result)
                .expect_err(&format!("{what} was accepted"));
            assert!(error.contains(names), "{what}: {error}");
            assert!(!error.contains("secret"), "{what}: {error}");
        }
    }

    /// A URL that does not take a registry tarball's form names one archive,
    /// so even a new version of it is a new place: refused rather than
    /// admitted by its host.
    #[test]
    fn a_tarball_url_without_a_registry_form_is_compared_in_full() {
        for (resolved, bumped) in [
            (
                "https://npm.example.test/download/@acme/tool/1.0.0/abc",
                "https://npm.example.test/download/@acme/tool/1.0.1/abc",
            ),
            (
                "https://gitlab.example.test/acme/tool/-/raw-1.0.0.tgz",
                "https://gitlab.example.test/acme/tool/-/raw-1.0.1.tgz",
            ),
            (
                "https://registry.npmjs.org/tool/-/tool-1.0.0.zip",
                "https://registry.npmjs.org/tool/-/tool-1.0.1.zip",
            ),
            (
                "https://registry.npmjs.org/@acme/tool/-/@other/tool-1.0.0.tgz",
                "https://registry.npmjs.org/@acme/tool/-/@other/tool-1.0.1.tgz",
            ),
        ] {
            let lock = |url: &str| {
                format!(
                    r#"{{"lockfileVersion": 3, "packages": {{"node_modules/tool": {{"version": "1", "resolved": "{url}"}}}}}}"#
                )
            };
            let error = check(
                LockfileType::PackageLockJson,
                &lock(resolved),
                &lock(bumped),
            )
            .expect_err(&format!("{bumped} was accepted"));
            assert!(
                error.contains(&format!("takes code from {bumped},")),
                "{error}"
            );
        }
    }

    /// A legacy lockfile, as npm 6 wrote it: npm installs each entry from
    /// its `version` and resolves its `requires` specs.
    const NPM_LEGACY: &str = r#"{
  "name": "demo",
  "version": "1.0.0",
  "lockfileVersion": 1,
  "requires": true,
  "dependencies": {
    "is-even": {
      "version": "1.0.0",
      "resolved": "https://registry.npmjs.org/is-even/-/is-even-1.0.0.tgz",
      "requires": {"is-odd": "^0.1.2"},
      "dependencies": {
        "is-number": {
          "version": "3.0.0",
          "resolved": "https://registry.npmjs.org/is-number/-/is-number-3.0.0.tgz"
        }
      }
    },
    "is-odd": {
      "version": "0.1.2",
      "resolved": "https://registry.npmjs.org/is-odd/-/is-odd-0.1.2.tgz"
    },
    "tool": {"version": "acme/tool#1111111", "from": "acme/tool"}
  }
}"#;

    /// npm reads a package's `version` in a legacy lockfile, and every
    /// dependency spec, as a package spec: `user/repo` is a GitHub
    /// repository, and a spec with a slash or a tarball name a local path.
    #[test]
    fn a_package_spec_naming_a_new_place_is_refused() {
        let modern = |from: &str, to: &str| NPM_BASE.replacen(from, to, 1);
        let legacy = |from: &str, to: &str| NPM_LEGACY.replacen(from, to, 1);
        for (what, base, result, names) in [
            (
                "a legacy version naming a repository",
                NPM_LEGACY,
                legacy(
                    r#""version": "0.1.2""#,
                    r#""version": "attacker/payload#deadbeef""#,
                ),
                "takes code from github:attacker/payload,",
            ),
            (
                "a nested legacy version naming a repository",
                NPM_LEGACY,
                legacy(r#""version": "3.0.0""#, r#""version": "attacker/payload""#),
                "takes code from github:attacker/payload,",
            ),
            (
                "a legacy requires spec naming a repository",
                NPM_LEGACY,
                legacy(
                    r#""is-odd": "^0.1.2""#,
                    r#""is-odd": "attacker/payload#deadbeef""#,
                ),
                "takes code from github:attacker/payload,",
            ),
            (
                "a package's dependency spec naming a repository",
                NPM_BASE,
                modern(
                    r#""integrity": "sha512-aa","#,
                    r#""integrity": "sha512-aa", "dependencies": {"is-odd": "attacker/payload#deadbeef"},"#,
                ),
                "takes code from github:attacker/payload,",
            ),
            (
                "a version naming a repository",
                NPM_BASE,
                modern(r#""version": "1.3.0""#, r#""version": "attacker/payload""#),
                "takes code from github:attacker/payload,",
            ),
            (
                "a dependency spec naming a local tarball",
                NPM_BASE,
                modern(r#""left-pad": "^1.3.0""#, r#""left-pad": "payload.tgz""#),
                "takes code from payload.tgz,",
            ),
            (
                "a dependency spec naming a local directory",
                NPM_BASE,
                modern(
                    r#""left-pad": "^1.3.0""#,
                    r#""left-pad": "./vendor/payload""#,
                ),
                "takes code from ./vendor/payload,",
            ),
            (
                "a dependency spec naming a hidden directory",
                NPM_BASE,
                modern(r#""left-pad": "^1.3.0""#, r#""left-pad": ".payload""#),
                "takes code from .payload,",
            ),
            (
                "a spec with a scheme npm reads and upd does not",
                NPM_BASE,
                modern(
                    r#""left-pad": "^1.3.0""#,
                    r#""left-pad": "workspace:payload""#,
                ),
                "unrecognised package spec 'workspace:payload' at packages..dependencies.left-pad",
            ),
        ] {
            assert_ne!(result, base, "{what}: the edit applies");
            let error = check(LockfileType::PackageLockJson, base, &result)
                .expect_err(&format!("{what} was accepted"));
            assert!(error.contains(names), "{what}: {error}");
        }
    }

    #[test]
    fn package_specs_within_the_known_places_are_accepted() {
        for (what, base, result) in [
            (
                "a legacy relock of a registry package and a known repository",
                NPM_LEGACY.to_string(),
                NPM_LEGACY
                    .replace("0.1.2", "0.1.3")
                    .replace("acme/tool#1111111", "acme/tool#2222222"),
            ),
            (
                "ranges, tags and aliases name the registry",
                NPM_BASE.to_string(),
                NPM_BASE.replace(
                    r#""left-pad": "^1.3.0""#,
                    r#""left-pad": "1.x || >=2.5.0 <3", "a": "latest", "b": "*", "c": "",
                    "d": "npm:@acme/core@^1", "e": "npm:left-pad@1.3.0", "version": "~1.0""#,
                ),
            ),
        ] {
            assert_ne!(result, base, "{what}: the edit applies");
            assert_eq!(
                check(LockfileType::PackageLockJson, &base, &result),
                Ok(()),
                "{what}"
            );
        }
    }

    /// A package npm installs without a `resolved` URL comes from whatever
    /// registry npm is configured with when it installs, which need not be
    /// the one the original lockfile named.
    #[test]
    fn a_package_left_to_the_configured_registry_is_a_new_place() {
        let private = r#"{
  "lockfileVersion": 3,
  "packages": {
    "": {"name": "demo", "dependencies": {"@acme/ui": "^1.0.0"}},
    "node_modules/@acme/ui": {
      "version": "1.0.0",
      "resolved": "https://gitlab.example.test/api/v4/projects/7/packages/npm/@acme/ui/-/@acme/ui-1.0.0.tgz",
      "integrity": "sha512-aa"
    }
  }
}"#;
        let resolved = r#"
      "resolved": "https://gitlab.example.test/api/v4/projects/7/packages/npm/@acme/ui/-/@acme/ui-1.0.0.tgz","#;
        let legacy_resolved = r#"
      "resolved": "https://registry.npmjs.org/is-odd/-/is-odd-0.1.2.tgz""#;
        let nested_resolved = r#",
          "resolved": "https://registry.npmjs.org/is-number/-/is-number-3.0.0.tgz""#;
        for (what, base, result) in [
            (
                "a package whose resolved URL is removed",
                private.to_string(),
                private.replacen(resolved, "", 1),
            ),
            (
                "a package added without a resolved URL",
                NPM_BASE.to_string(),
                NPM_BASE.replacen(
                    r#""node_modules/shared""#,
                    r#""node_modules/payload": {"version": "1.0.0"},
    "node_modules/shared""#,
                    1,
                ),
            ),
            (
                "a nested package whose resolved URL is removed",
                private.to_string(),
                private.replacen(
                    r#""node_modules/@acme/ui": {"#,
                    r#""node_modules/@acme/ui/node_modules/payload": {"version": "1.0.0"},
    "node_modules/@acme/ui": {"#,
                    1,
                ),
            ),
            (
                "a package added with neither a version nor a resolved URL",
                private.to_string(),
                private.replacen(
                    r#""node_modules/@acme/ui": {"#,
                    r#""node_modules/payload": {"integrity": "sha512-bb"},
    "node_modules/@acme/ui": {"#,
                    1,
                ),
            ),
            (
                "a workspace's package added without a resolved URL",
                private.to_string(),
                private.replacen(
                    r#""node_modules/@acme/ui": {"#,
                    r#""packages/web/node_modules/payload": {"version": "1.0.0"},
    "node_modules/@acme/ui": {"#,
                    1,
                ),
            ),
            (
                "a legacy package whose resolved URL is removed",
                NPM_LEGACY.to_string(),
                NPM_LEGACY.replacen(legacy_resolved, "", 1).replacen(
                    r#""version": "0.1.2","#,
                    r#""version": "0.1.2""#,
                    1,
                ),
            ),
            (
                "a nested legacy package whose resolved URL is removed",
                NPM_LEGACY.to_string(),
                NPM_LEGACY.replacen(nested_resolved, "", 1),
            ),
        ] {
            assert_ne!(result, base, "{what}: the edit applies");
            let error = check(LockfileType::PackageLockJson, &base, &result)
                .expect_err(&format!("{what} was accepted"));
            assert!(
                error.contains("leaves a package to the registry npm is configured with"),
                "{what}: {error}"
            );
        }
        // npm reads a `resolved` that is not a string as no `resolved` at
        // all, in a modern and in a legacy lockfile alike.
        for value in ["null", "false", "0", "{}", "[]"] {
            for (base, result) in [
                (
                    private.to_string(),
                    private.replacen(resolved, &format!("\n      \"resolved\": {value},"), 1),
                ),
                (
                    NPM_LEGACY.to_string(),
                    NPM_LEGACY.replacen(
                        nested_resolved,
                        &format!(",\n          \"resolved\": {value}"),
                        1,
                    ),
                ),
            ] {
                assert_ne!(result, base, "{value}: the edit applies");
                let error = check(LockfileType::PackageLockJson, &base, &result)
                    .expect_err(&format!("a resolved of {value} was accepted"));
                assert!(
                    error.contains("leaves a package to the registry npm is configured with"),
                    "{value}: {error}"
                );
            }
        }
        // An empty `resolved` names a local path, which is a new place too.
        let empty = private.replacen(resolved, "\n      \"resolved\": \"\",", 1);
        assert!(check(LockfileType::PackageLockJson, private, &empty).is_err());
    }

    #[test]
    fn packages_npm_installs_without_a_registry_are_accepted() {
        let omitted = NPM_BASE.replacen(
            r#"
      "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz","#,
            "",
            1,
        );
        for (what, base, result) in [
            (
                "a lockfile written without registry URLs, relocked the same way",
                omitted.clone(),
                omitted.replace("1.3.0", "1.3.1"),
            ),
            (
                "a bundled package and a workspace",
                NPM_BASE.to_string(),
                NPM_BASE.replacen(
                    r#""node_modules/shared""#,
                    r#""node_modules/tool/node_modules/bundled": {"version": "1.0.0", "inBundle": true},
    "packages/web": {"name": "web", "version": "1.0.0"},
    "node_modules/shared""#,
                    1,
                ),
            ),
            (
                "a bundled legacy package",
                NPM_LEGACY.to_string(),
                NPM_LEGACY.replacen(
                    r#""tool": {"#,
                    r#""bundled": {"version": "1.0.0", "bundled": true},
    "tool": {"#,
                    1,
                ),
            ),
        ] {
            assert_ne!(result, base, "{what}: the edit applies");
            assert_eq!(
                check(LockfileType::PackageLockJson, &base, &result),
                Ok(()),
                "{what}"
            );
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
                "registry https://crates.example.test/index/,",
            ),
            (
                "another registry on the same host",
                "registry+https://github.com/rust-lang/crates.io-index",
                "registry+https://github.com/attacker/other-index",
                "https://github.com/attacker/other-index",
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
