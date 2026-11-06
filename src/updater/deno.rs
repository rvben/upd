//! Deno dependency updating: the `"imports"` map of `deno.json`/`deno.jsonc`.
//!
//! A Deno project declares its dependencies in the top-level `"imports"` field
//! of its configuration file, whose values are specifiers such as
//! `"jsr:@std/path@^1.0.0"` and `"npm:chalk@^5.0.0"`. `upd` updates the version
//! range inside those specifiers:
//!
//! * `jsr:` ranges resolve through the JSR registry the updater is built with
//!   ([`crate::registry::JsrRegistry`] in the binary), whose metadata endpoint
//!   lists every published release with its publish time.
//! * `npm:` ranges resolve through the registry the caller passes to
//!   [`Updater::update`], the same client the Node.js updater uses, so
//!   credentials and scoped-registry configuration in `.npmrc` apply unchanged.
//! * Range grammar is npm's in both cases (JSR adopted it for `jsr:` too), so
//!   [`crate::npm_range`] decides what a specifier admits and where its floor
//!   is written.
//!
//! `deno.jsonc` is JSON with comments. The read path strips them with a
//! string-aware scanner; the write path rewrites only the matched specifier
//! inside the original text, so comments and formatting survive byte for byte.
//! `deno.lock` is never hand-edited: `upd --lock` refreshes it with
//! `deno install --lockfile-only` (see [`crate::lockfile`]).
//!
//! A standalone `import_map.json` (referenced by `"importMap"`) has the same
//! `imports` shape but is not discovered; its entries duplicate what
//! `deno.json` already states in the projects where it matters.

use super::{
    FileType, Lang, ParsedDependency, RegistryRoute, UpdateOptions, UpdateResult, Updater,
    downgrade_warning, read_file_safe, unpinnable_error, unreadable_error, unrewritable_warning,
    write_file_atomic,
};
use crate::align::compare_versions;
use crate::jsonc;
use crate::npm_range::{
    SpecShape, admits, classify, holds_its_floor, lower_bound_anchor, rewrite_lower_bound,
};
use crate::registry::Registry;
use crate::version::{is_prerelease_semver, match_version_precision};
use anyhow::{Result, anyhow};
use futures::future::join_all;
use regex::Regex;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Updater for the `"imports"` map of a Deno configuration file.
///
/// `jsr` answers for `jsr:` specifiers; the registry the caller passes to
/// [`Updater::update`] answers for `npm:` ones. Holding the JSR registry
/// mirrors how [`crate::updater::MiseUpdater`] carries the registries its
/// entries route between, while keeping `npm:` resolution on the shared,
/// credential-aware npm client.
pub struct DenoUpdater {
    /// The registry `jsr:` specifiers resolve through. `None` on a
    /// parse-only updater, where every jsr: lookup is an error.
    jsr: Option<Arc<dyn Registry>>,
}

/// The scheme a `deno.json` import value is written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Jsr,
    Npm,
}

/// One parsed import value: `jsr:@std/path@^1.0.0/join` decomposed.
///
/// `prefix` keeps the exact scheme spelling the file used (`jsr:` vs the
/// import-map-standard `jsr:/`), `range` the npm-style version range (absent
/// when the specifier names no version), and `suffix` any subpath after the
/// range, slash included. Rebuilding the specifier from the same parts with a
/// new range is what keeps an update from reshaping the line it edits.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSpecifier {
    scheme: Scheme,
    prefix: String,
    package: String,
    range: Option<String>,
    suffix: String,
}

impl ParsedSpecifier {
    #[cfg(test)]
    fn range(&self) -> &str {
        self.range.as_deref().unwrap_or_default()
    }

    fn rebuild(&self, range: &str) -> String {
        format!("{}{}@{}{}", self.prefix, self.package, range, self.suffix)
    }
}

/// Split an import value into scheme, package, range and subpath.
///
/// Returns `None` for anything that is not a registry specifier: HTTPS URLs,
/// relative paths, `node:` builtins. The package ends at the first `@` that
/// does not open a scope, which is why the scope's own `@` is walked past
/// first.
fn parse_specifier(value: &str) -> Option<ParsedSpecifier> {
    // Longest prefix first, so `jsr:/` is not read as `jsr:` over a leading
    // slash the import-map standard allows.
    let (scheme, prefix, rest) = value
        .strip_prefix("jsr:/")
        .map(|rest| (Scheme::Jsr, "jsr:/", rest))
        .or_else(|| {
            value
                .strip_prefix("jsr:")
                .map(|rest| (Scheme::Jsr, "jsr:", rest))
        })
        .or_else(|| {
            value
                .strip_prefix("npm:/")
                .map(|rest| (Scheme::Npm, "npm:/", rest))
        })
        .or_else(|| {
            value
                .strip_prefix("npm:")
                .map(|rest| (Scheme::Npm, "npm:", rest))
        })?;

    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }

    // The package runs to the first `@` that starts a version, which cannot be
    // the leading `@` of a scope: `@std/path@^1` finds it after the scope, and
    // a scoped name without a version has none.
    let version_at = rest
        .char_indices()
        .skip(1)
        .find(|(_, ch)| *ch == '@')
        .map(|(idx, _)| idx);
    let (package, after) = match version_at {
        Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
        None => (rest, None),
    };
    if package.is_empty() {
        return None;
    }

    // After the range a subpath may follow: `npm:pkg@^1.0.0/subpath`. With a
    // version, the first slash after it opens the subpath and the package is
    // already complete. Without one, a slash beyond the package name itself
    // opens the subpath (`jsr:@std/path/join`), and the package ends there.
    let (package, range, suffix) = match after {
        Some(after) => match after.find('/') {
            Some(slash) => (
                package.to_string(),
                Some(after[..slash].to_string()),
                after[slash..].to_string(),
            ),
            None => (package.to_string(), Some(after.to_string()), String::new()),
        },
        None => {
            // For `@scope/name` the package's own slash is the one after the
            // scope; only a later one is a subpath.
            let name_start = if package.starts_with('@') {
                package
                    .find('/')
                    .map(|idx| idx + 1)
                    .unwrap_or(package.len())
            } else {
                0
            };
            match package[name_start..].find('/') {
                Some(rel) => {
                    let slash = name_start + rel;
                    (
                        package[..slash].to_string(),
                        None,
                        package[slash..].to_string(),
                    )
                }
                None => (package.to_string(), None, String::new()),
            }
        }
    };
    let range = range.filter(|r| !r.is_empty());

    Some(ParsedSpecifier {
        scheme,
        prefix: prefix.to_string(),
        package,
        range,
        suffix,
    })
}

/// Parse a Deno manifest into JSON, tolerating comments and trailing commas.
fn parse_deno_manifest(content: &str) -> Result<Value> {
    jsonc::parse(content).map_err(|e| anyhow!("invalid deno.json/deno.jsonc: {e}"))
}

/// The `"imports"` map of a parsed manifest, as the string-valued entries it
/// declares. Non-string values are not import mappings.
fn imports_of(json: &Value) -> Vec<(String, String)> {
    let Some(imports) = json.get("imports").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    imports
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
        .collect()
}

/// The line an import is written on, found by its key in the raw content.
///
/// The search covers only the top-level `"imports"` object, the map the entry
/// was read from: a `"scopes"` map can carry the same key. Within it the match
/// is anchored to a JSON key shape (optional indent, the quoted key, a colon),
/// so the same string inside a comment's prose does not match. The first
/// match wins, which is what the line index in `package_json.rs` does for
/// duplicate keys too.
fn line_for_import(content: &str, key: &str) -> Option<usize> {
    let span = crate::jsonc::object_span(content, "imports")?;
    let pattern = format!(r#"^\s*"{}"\s*:"#, regex::escape(key));
    let re = Regex::new(&pattern).ok()?;
    let lines_before = content[..span.start].matches('\n').count();
    content[span]
        .lines()
        .position(|line| re.is_match(line))
        .map(|idx| lines_before + idx + 1)
}

impl DenoUpdater {
    pub fn new(jsr: Arc<dyn Registry>) -> Self {
        Self { jsr: Some(jsr) }
    }

    /// An updater that can read a file but not resolve anything in it, for
    /// callers that only ever call `parse_dependencies`. This is the pattern
    /// [`crate::updater::MiseUpdater::new_parse_only`] follows for the same
    /// reason: alignment never resolves, so building a live registry client
    /// there would only be a request waiting to happen.
    pub fn new_parse_only() -> Self {
        Self { jsr: None }
    }

    /// The registry a `jsr:` specifier resolves through.
    ///
    /// Fallible rather than silently skipped: a parse-only updater asked to
    /// resolve is a caller bug, and the error names it, the way
    /// [`crate::updater::RegistrySet::for_source`] refuses a source a
    /// parse-only set was built without.
    fn jsr(&self) -> Result<&dyn Registry> {
        self.jsr.as_deref().ok_or_else(|| {
            anyhow!("no jsr registry available: this updater was built for parsing only")
        })
    }

    /// Rewrite one import value in the original content, comments intact.
    ///
    /// The edit is confined to the top-level `"imports"` object and anchors
    /// on the quoted key and the exact old value, so it cannot reach past the
    /// entry it was decided for, including a `"scopes"` entry of the same key.
    fn update_specifier_in_content(
        &self,
        content: &str,
        key: &str,
        old_value: &str,
        new_value: &str,
    ) -> String {
        let pattern = format!(
            r#"("{}"\s*:\s*"){}(")"#,
            regex::escape(key),
            regex::escape(old_value)
        );
        let re = Regex::new(&pattern).expect("Invalid pattern");
        let replacement = format!(r#"${{1}}{new_value}${{2}}"#);
        let Some(span) = crate::jsonc::object_span(content, "imports") else {
            return content.to_string();
        };
        let section = re.replace(&content[span.clone()], replacement.as_str());
        format!(
            "{}{}{}",
            &content[..span.start],
            section,
            &content[span.end..]
        )
    }

    /// Shared in-memory update path over the `"imports"` map.
    pub(crate) async fn update_content(
        &self,
        content: String,
        npm: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<(UpdateResult, String)> {
        let json = parse_deno_manifest(&content)?;
        let mut result = UpdateResult::default();
        let mut new_content = content.clone();

        let imports = imports_of(&json);
        let mut ignored: Vec<(String, String, Option<usize>)> = Vec::new();
        // (key, value, specifier, range, pinned version)
        let mut pinned_entries: Vec<(String, String, ParsedSpecifier, String, String)> = Vec::new();
        // (key, value, specifier)
        let mut to_check: Vec<(String, String, ParsedSpecifier)> = Vec::new();

        for (key, value) in &imports {
            let Some(spec) = parse_specifier(value) else {
                // Not a registry specifier: an HTTPS URL, a local mapping.
                // There is nothing to look up, so there is nothing to report.
                continue;
            };
            let Some(range) = spec.range.clone() else {
                // `jsr:@std/path` with no version means "latest" at resolve
                // time; there is no pin to compare and nothing to move.
                continue;
            };

            // The name a config pin, a `--package` filter or an ignore entry
            // matches is the package the specifier resolves, not the bare key
            // it is mapped from.
            let line = line_for_import(&content, key);
            if options.is_package_filtered_out(&spec.package) {
                result.unchanged += 1;
                continue;
            }
            if options.should_ignore(&spec.package) {
                ignored.push((spec.package.clone(), range, line));
                continue;
            }
            if let Some(pinned) = options.get_pinned_version(&spec.package) {
                match classify(&range) {
                    SpecShape::ExactPin | SpecShape::CaretOrTilde => {
                        // The version sits at a known place behind a known
                        // operator, so the standard pin flow can put the
                        // pinned one there.
                        pinned_entries.push((
                            key.clone(),
                            value.clone(),
                            spec,
                            range,
                            pinned.to_string(),
                        ));
                    }
                    SpecShape::ShapeRange | SpecShape::BoundedRange => {
                        // The range keeps its own ceiling; the pin becomes its
                        // floor, and a pin the range cannot hold is a failed
                        // instruction rather than a note.
                        match rewrite_lower_bound(&range, pinned)
                            .filter(|new_spec| holds_its_floor(new_spec))
                        {
                            Some(new_spec) if new_spec == range => result.unchanged += 1,
                            Some(new_spec) => {
                                if !result.hold_strict_pin(
                                    &options,
                                    &spec.package,
                                    &range,
                                    &new_spec,
                                    line,
                                ) {
                                    result.pinned.push((
                                        spec.package.clone(),
                                        range.clone(),
                                        new_spec.clone(),
                                        line,
                                    ));
                                    new_content = self.update_specifier_in_content(
                                        &new_content,
                                        key,
                                        value,
                                        &spec.rebuild(&new_spec),
                                    );
                                }
                            }
                            None => {
                                result
                                    .errors
                                    .push(unpinnable_error(&spec.package, pinned, &range))
                            }
                        }
                    }
                    SpecShape::OpaqueRange => {
                        // No single floor, so nowhere to put the pin without
                        // rewriting the range into something the author did
                        // not write.
                        result
                            .errors
                            .push(unpinnable_error(&spec.package, pinned, &range));
                    }
                    SpecShape::Unsupported => {
                        result.errors.push(unreadable_error(&spec.package, &range));
                    }
                    SpecShape::NoVersion => continue,
                }
                continue;
            }

            match classify(&range) {
                SpecShape::Unsupported => {
                    result.errors.push(unreadable_error(&spec.package, &range));
                }
                // Ranges with a floor to raise or a pin to compare are checked
                // below; the remaining shapes were decided above.
                SpecShape::NoVersion => continue,
                _ => to_check.push((key.clone(), value.clone(), spec)),
            }
        }

        result.ignored.extend(ignored);

        // Config pins: the operator and precision the file already uses are
        // kept, only the version moves.
        for (key, value, spec, range, pinned) in pinned_entries {
            let prefix_len = range
                .find(|c: char| c.is_ascii_digit())
                .unwrap_or(range.len());
            let (prefix, current_token) = range.split_at(prefix_len);
            let matched = if options.full_precision {
                pinned.clone()
            } else {
                match_version_precision(current_token, &pinned)
            };
            let new_range = format!("{prefix}{matched}");
            let line = line_for_import(&content, &key);
            if new_range == range {
                result.unchanged += 1;
            } else if !result.hold_strict_pin(
                &options,
                &spec.package,
                current_token,
                &matched,
                line,
            ) {
                result
                    .pinned
                    .push((spec.package.clone(), range, new_range.clone(), line));
                new_content = self.update_specifier_in_content(
                    &new_content,
                    &key,
                    &value,
                    &spec.rebuild(&new_range),
                );
            }
        }

        // Registry lookups run concurrently, one future per specifier. The
        // scheme decides the registry inside each future, where both the
        // updater's JSR client and the caller's npm client are in scope. A
        // parse-only updater has no JSR client, so a jsr: lookup errors the
        // way every other failed lookup does.
        let version_futures: Vec<_> = to_check
            .iter()
            .map(|(_, _, spec)| {
                let package = spec.package.clone();
                let range = spec.range.clone().unwrap_or_default();
                async move {
                    let registry: &dyn Registry = match spec.scheme {
                        Scheme::Jsr => self.jsr()?,
                        Scheme::Npm => npm,
                    };
                    match classify(&range) {
                        SpecShape::ExactPin => {
                            // A prerelease pin asks for the newest prerelease,
                            // so a pin never silently falls back to stable.
                            if is_prerelease_semver(&range) {
                                registry
                                    .get_latest_version_including_prereleases(&package)
                                    .await
                            } else {
                                registry.get_latest_version(&package).await
                            }
                        }
                        SpecShape::CaretOrTilde => {
                            // The ceiling is implied by the floor, so the
                            // lookup is held to the range the file states.
                            registry.get_latest_version_matching(&package, &range).await
                        }
                        SpecShape::ShapeRange => {
                            // The whole shape moves with the newest release.
                            registry.get_latest_version(&package).await
                        }
                        SpecShape::BoundedRange => {
                            // The ceiling was chosen independently; the
                            // replacement must come from inside the range.
                            registry.get_latest_version_matching(&package, &range).await
                        }
                        SpecShape::OpaqueRange | SpecShape::Unsupported | SpecShape::NoVersion => {
                            unreachable!("routed before lookup")
                        }
                    }
                }
            })
            .collect();
        let version_results = join_all(version_futures).await;

        for ((key, value, spec), lookup) in to_check.into_iter().zip(version_results) {
            let package = spec.package.clone();
            let range = spec.range.clone().expect("routed only with a range");
            let line = line_for_import(&content, &key);

            match lookup {
                Err(e) => {
                    // The lookup did not happen, which is not the same as the
                    // dependency being current.
                    result.errors.push(format!("{package}: {e}"));
                }
                Ok(latest) => {
                    let outcome =
                        route_update(&spec, &range, &latest, npm, self.jsr.as_deref(), &options)
                            .await;
                    match outcome {
                        Routed::Unchanged => result.unchanged += 1,
                        Routed::Held {
                            write,
                            reported: (old, new),
                            held: (skipped_version, skipped_published_at),
                        } => {
                            new_content = self.update_specifier_in_content(
                                &new_content,
                                &key,
                                &value,
                                &spec.rebuild(&write),
                            );
                            result.updated.push((package.clone(), old, new, line));
                            result.held_back.push((
                                package,
                                range,
                                write,
                                skipped_version,
                                skipped_published_at,
                            ));
                        }
                        Routed::Update { write, reported } => {
                            new_content = self.update_specifier_in_content(
                                &new_content,
                                &key,
                                &value,
                                &spec.rebuild(&write),
                            );
                            result.updated.push((package, reported.0, reported.1, line));
                        }
                        Routed::Capped { current, available } => {
                            result.record_capped(&package, &current, &available, line);
                        }
                        Routed::DowngradeWarning(effective) => {
                            result
                                .warnings
                                .push(downgrade_warning(&package, &effective, &range));
                            result.unchanged += 1;
                        }
                        Routed::Unrewritable => {
                            result
                                .warnings
                                .push(unrewritable_warning(&package, &latest, &range));
                        }
                        Routed::SkippedByCooldown {
                            skipped_version,
                            skipped_published_at,
                        } => {
                            result.skipped_by_cooldown.push((
                                package,
                                range,
                                skipped_version,
                                skipped_published_at,
                            ));
                        }
                        Routed::Error(message) => result.errors.push(message),
                    }
                }
            }
        }

        Ok((result, new_content))
    }
}

/// What one specifier's update decided, after its registry lookup came back.
///
/// Shaping the outcomes as an enum keeps the write/recording split out of the
/// shape router: every branch either moves the range, declines to with a
/// recorded reason, or leaves the entry counted as current.
enum Routed {
    /// The range already admits the newest release.
    Unchanged,
    /// Move the range to `write`; a cooldown chose it over a newer release,
    /// recorded as `(skipped_version, published_at)`.
    Held {
        write: String,
        reported: (String, String),
        held: (String, Option<chrono::DateTime<chrono::Utc>>),
    },
    /// Move the range to `write`, reported as `reported`.
    Update {
        write: String,
        reported: (String, String),
    },
    /// A newer release exists but the bump ceiling refused it.
    Capped { current: String, available: String },
    /// The lookup returned something older than the current floor.
    DowngradeWarning(String),
    /// The range cannot be rewritten; the newest release falls outside it.
    Unrewritable,
    /// Cooldown held every newer release.
    SkippedByCooldown {
        skipped_version: String,
        skipped_published_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// A cooldown lookup failed; the entry is not counted as current.
    Error(String),
}

/// The release a specifier is measured from: the version an exact pin or a
/// caret/tilde range names after its operator, otherwise the completed floor
/// of a range that states one.
///
/// Parsing and cooldown share it, so a specifier audit and align can see is
/// also one cooldown can age, and the reverse. `None` means the specifier
/// names no single complete release.
fn current_version(range: &str, shape: SpecShape) -> Option<String> {
    let version = match shape {
        SpecShape::ExactPin | SpecShape::CaretOrTilde => {
            let start = range.find(|c: char| c.is_ascii_digit())?;
            range[start..].trim().to_string()
        }
        _ => lower_bound_anchor(range)?,
    };
    semver::Version::parse(&version).ok()?;
    Some(version)
}

/// Decide what one looked-up release means for one specifier.
///
/// Exact pins and caret/tilde ranges keep their operator and precision, the
/// way `package_json.rs` rewrites its specs; shape and bounded ranges rewrite
/// their floor through [`rewrite_lower_bound`], with cooldown applied to the
/// same lookup the registry answered.
async fn route_update(
    spec: &ParsedSpecifier,
    range: &str,
    latest: &str,
    npm: &dyn Registry,
    jsr: Option<&dyn Registry>,
    options: &UpdateOptions,
) -> Routed {
    let registry = match spec.scheme {
        Scheme::Jsr => match jsr {
            Some(jsr) => jsr,
            None => {
                return Routed::Error(
                    "no jsr registry available: this updater was built for parsing only"
                        .to_string(),
                );
            }
        },
        Scheme::Npm => npm,
    };
    let shape = classify(range);

    // Cooldown needs a current version to measure an age from. Specifiers
    // naming no single release skip cooldown, like package_json.rs.
    let anchor = current_version(range, shape);
    let current_is_pre = anchor.as_deref().map(is_prerelease_semver).unwrap_or(false);

    let (effective, held_back) = if let Some(anchor) = &anchor {
        let constraint = match shape {
            // Only a range that bounds its own lookup may narrow the cooldown
            // choice to that lookup: a shape range follows the newest release
            // even when the release leaves today's shape behind.
            SpecShape::BoundedRange => Some(range),
            _ => None,
        };
        let (outcome, note) = crate::updater::apply_cooldown(
            registry,
            &spec.package,
            anchor,
            latest,
            constraint,
            current_is_pre,
            options,
        )
        .await;
        if let Some(note) = note {
            options.note_cooldown_unavailable(&note);
        }
        match outcome {
            crate::updater::CooldownOutcome::Unchanged(version) => (Some(version), None),
            crate::updater::CooldownOutcome::HeldBack {
                chosen,
                skipped_version,
                skipped_published_at,
            } => (Some(chosen), Some((skipped_version, skipped_published_at))),
            crate::updater::CooldownOutcome::LookupFailed { error } => {
                return Routed::Error(error);
            }
            crate::updater::CooldownOutcome::Skipped {
                skipped_version,
                skipped_published_at,
            } => {
                return Routed::SkippedByCooldown {
                    skipped_version,
                    skipped_published_at,
                };
            }
        }
    } else {
        (Some(latest.to_string()), None)
    };

    let Some(effective) = effective else {
        // The cooldown Skipped branch already recorded the hold.
        return Routed::Unchanged;
    };

    match shape {
        SpecShape::ExactPin | SpecShape::CaretOrTilde => {
            let prefix_len = range
                .find(|c: char| c.is_ascii_digit())
                .unwrap_or(range.len());
            let (prefix, current_token) = range.split_at(prefix_len);
            // A prerelease pin fetched the newest prerelease; a registry that
            // answered with a stable release must not silently promote it.
            if is_prerelease_semver(current_token) && !is_prerelease_semver(&effective) {
                return Routed::Unchanged;
            }
            let matched = if options.full_precision {
                effective.clone()
            } else {
                match_version_precision(current_token, &effective)
            };
            if compare_versions(&matched, current_token, Lang::Node) == std::cmp::Ordering::Greater
                && !options.allows_bump(current_token, &matched)
            {
                return Routed::Capped {
                    current: current_token.to_string(),
                    available: matched,
                };
            }
            let new_range = format!("{prefix}{matched}");
            if new_range == range {
                Routed::Unchanged
            } else {
                // The report names the release, the way package_json.rs
                // reports its specs: the version tokens, operator aside.
                let reported = (current_token.to_string(), matched);
                if let Some((skipped_version, skipped_published_at)) = held_back {
                    Routed::Held {
                        write: new_range,
                        reported,
                        held: (skipped_version, skipped_published_at),
                    }
                } else {
                    Routed::Update {
                        write: new_range,
                        reported,
                    }
                }
            }
        }
        SpecShape::ShapeRange | SpecShape::BoundedRange => {
            if anchor
                .as_deref()
                .is_some_and(|floor| compare_versions(&effective, floor, Lang::Node).is_lt())
            {
                return Routed::DowngradeWarning(effective);
            }
            match rewrite_lower_bound(range, &effective) {
                Some(new_range) if new_range != range => {
                    if let Some(floor) = &anchor
                        && !options.allows_bump(floor, &effective)
                    {
                        return Routed::Capped {
                            current: range.to_string(),
                            available: new_range,
                        };
                    }
                    // A range reports itself: its floor moved from the old
                    // spec to the new one, and neither is a bare release.
                    let reported = (range.to_string(), new_range.clone());
                    if let Some((skipped_version, skipped_published_at)) = held_back {
                        Routed::Held {
                            write: new_range,
                            reported,
                            held: (skipped_version, skipped_published_at),
                        }
                    } else {
                        Routed::Update {
                            write: new_range,
                            reported,
                        }
                    }
                }
                Some(_) => Routed::Unchanged,
                None => Routed::Error(format!(
                    "{}: '{range}' was read as a range with a lower bound to raise, but it has none",
                    spec.package
                )),
            }
        }
        SpecShape::OpaqueRange => match admits(range, &effective) {
            Some(true) => Routed::Unchanged,
            Some(false) => Routed::Unrewritable,
            None => Routed::Error(unreadable_error(&spec.package, range)),
        },
        SpecShape::Unsupported | SpecShape::NoVersion => {
            // Routed before the lookup; a shape added to npm_range must choose
            // a route here rather than silently do nothing.
            Routed::Error(unreadable_error(&spec.package, range))
        }
    }
}

#[async_trait::async_trait]
impl Updater for DenoUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let (result, new_content) = self
            .update_content(content, registry, options.clone())
            .await?;
        if (!result.updated.is_empty() || !result.pinned.is_empty()) && !options.dry_run {
            write_file_atomic(path, &new_content)?;
        }
        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::DenoJson
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let json = parse_deno_manifest(&content)?;
        let mut deps = Vec::new();

        for (key, value) in imports_of(&json) {
            let Some(spec) = parse_specifier(&value) else {
                continue;
            };
            let Some(range) = spec.range else {
                continue;
            };
            // A specifier naming no single release has nothing for audit or
            // align to line up; one that does is reported at that release,
            // the same spelling every other ecosystem's parser hands them.
            let Some(anchor) = current_version(&range, classify(&range)) else {
                continue;
            };
            // The registry the specifier resolves through. Audit routes on
            // it: `npm:` specifiers are real npm names OSV indexes, `jsr:`
            // ones name packages no OSV ecosystem covers.
            let registry = match spec.scheme {
                Scheme::Jsr => RegistryRoute::Jsr,
                Scheme::Npm => RegistryRoute::Npm,
            };
            deps.push(
                // Alignment groups by the package the specifier resolves, so
                // the same package is comparable across deno.json files
                // whatever key each imports it under.
                ParsedDependency::new(spec.package, anchor, line_for_import(&content, &key))
                    .with_registry(registry),
            );
        }

        Ok(deps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn jsr_mock(versions: &[(&str, &str)]) -> MockRegistry {
        let mut mock = MockRegistry::new("jsr-test");
        for (package, version) in versions {
            mock = mock.with_version(package, version);
        }
        mock
    }

    const MANIFEST: &str = r#"{
  // A comment that must survive every update.
  "node": true,
  "imports": {
    "@std/path": "jsr:@std/path@^1.0.0",
    "chalk": "npm:chalk@^5.0.0"
  }
}
"#;

    async fn update_with(
        content: &str,
        jsr: MockRegistry,
        npm: MockRegistry,
        options: UpdateOptions,
    ) -> (UpdateResult, String) {
        DenoUpdater::new(Arc::new(jsr))
            .update_content(content.to_string(), &npm, options)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn valid_update_moves_jsr_and_npm_ranges() {
        let jsr = jsr_mock(&[("@std/path", "1.1.6")]);
        let npm = MockRegistry::new("npm-test").with_version("chalk", "5.6.2");

        let (result, new_content) =
            update_with(MANIFEST, jsr, npm, UpdateOptions::new(true, false)).await;

        assert_eq!(result.updated.len(), 2, "{:?}", result.errors);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(new_content.contains("jsr:@std/path@^1.1.6"));
        assert!(new_content.contains("npm:chalk@^5.6.2"));
        // Comments and formatting survive.
        assert!(new_content.contains("// A comment that must survive every update."));
        assert!(new_content.contains("\"node\": true,"));
    }

    #[tokio::test]
    async fn pinned_entry_moves_the_configured_version() {
        let jsr = jsr_mock(&[("@std/path", "1.1.6")]);
        let npm = MockRegistry::new("npm-test").with_version("chalk", "5.6.2");
        let mut config = crate::config::UpdConfig::default();
        config
            .pin
            .insert("@std/path".to_string(), "1.1.6".to_string());
        let options = UpdateOptions::new(true, false).with_config(Arc::new(config));

        let (result, new_content) = update_with(MANIFEST, jsr, npm, options).await;

        assert!(
            result
                .pinned
                .iter()
                .any(|(pkg, old, new, _)| pkg == "@std/path" && old == "^1.0.0" && new == "^1.1.6"),
            "pinned: {:?}",
            result.pinned
        );
        assert!(new_content.contains("jsr:@std/path@^1.1.6"));
    }

    #[tokio::test]
    async fn range_at_latest_reports_unchanged() {
        // The registries' newest releases are exactly what the ranges already
        // name: nothing moves and nothing is reported as available.
        let jsr = jsr_mock(&[("@std/path", "1.0.0")]);
        let npm = MockRegistry::new("npm-test").with_version("chalk", "5.0.0");

        let (result, new_content) =
            update_with(MANIFEST, jsr, npm, UpdateOptions::new(true, false)).await;

        assert_eq!(result.updated.len(), 0, "{:?}", result.updated);
        assert_eq!(result.unchanged, 2);
        assert!(new_content.contains("jsr:@std/path@^1.0.0"));
        assert!(new_content.contains("npm:chalk@^5.0.0"));
    }

    #[tokio::test]
    async fn malformed_manifest_is_an_error_not_a_panic() {
        let jsr = jsr_mock(&[("@std/path", "1.0.0")]);
        let npm = MockRegistry::new("npm-test");
        let broken = r#"{ "imports": { "@std/path": "jsr:@std/path@^1.0.0",, "x": }"#;

        let outcome = DenoUpdater::new(Arc::new(jsr))
            .update_content(broken.to_string(), &npm, UpdateOptions::new(true, false))
            .await;

        assert!(outcome.is_err());
    }

    #[tokio::test]
    async fn missing_lockfile_does_not_block_the_manifest() {
        // deno.lock is regenerated by `deno install --lockfile-only` under
        // --lock; the updater itself decides from the manifest and the
        // registry only. A directory with no deno.lock updates normally.
        let jsr = jsr_mock(&[("@std/path", "1.1.6")]);
        let npm = MockRegistry::new("npm-test").with_version("chalk", "5.6.2");

        let (result, _) = update_with(MANIFEST, jsr, npm, UpdateOptions::new(true, false)).await;
        assert_eq!(result.updated.len(), 2);
    }

    #[tokio::test]
    async fn lookup_failure_is_reported_not_counted_current() {
        let jsr = jsr_mock(&[]);
        let npm = MockRegistry::new("npm-test").with_version("chalk", "5.6.2");

        let (result, _) = update_with(MANIFEST, jsr, npm, UpdateOptions::new(true, false)).await;
        assert_eq!(result.updated.len(), 1);
        assert!(
            result.errors.iter().any(|e| e.contains("@std/path")),
            "errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn a_parse_only_updater_parses_the_same_dependencies_as_a_resolving_one() {
        // align builds the parse-only updater; the deps it reports must be
        // the ones a resolving updater would have reported, so alignment
        // never depends on which constructor the caller picked.
        let dir = tempdir().unwrap();
        let path = dir.path().join("deno.json");
        let content = r#"{
  "imports": {
    "@std/path": "jsr:@std/path@>=1.0.0",
    "chalk": "npm:chalk@>=5.0.0"
  }
}
"#;
        std::fs::write(&path, content).unwrap();

        let parse_only = DenoUpdater::new_parse_only()
            .parse_dependencies(&path)
            .unwrap();
        let resolving = DenoUpdater::new(Arc::new(MockRegistry::new("jsr-test")))
            .parse_dependencies(&path)
            .unwrap();

        let shape = |deps: &[ParsedDependency]| {
            let mut named: Vec<(String, String, Option<RegistryRoute>)> = deps
                .iter()
                .map(|d| (d.name.clone(), d.version.clone(), d.registry))
                .collect();
            named.sort();
            named
        };
        assert_eq!(shape(&parse_only), shape(&resolving));
        assert_eq!(
            shape(&parse_only),
            vec![
                (
                    "@std/path".to_string(),
                    "1.0.0".to_string(),
                    Some(RegistryRoute::Jsr),
                ),
                (
                    "chalk".to_string(),
                    "5.0.0".to_string(),
                    Some(RegistryRoute::Npm),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_parse_only_updater_resolves_nothing_and_says_so() {
        // The npm registry still answers: parse-only withholds the updater's
        // own JSR client, not the caller's registry.
        let npm = MockRegistry::new("npm-test").with_version("chalk", "5.6.2");

        let (result, new_content) = DenoUpdater::new_parse_only()
            .update_content(MANIFEST.to_string(), &npm, UpdateOptions::new(true, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1, "{:?}", result.updated);
        assert!(new_content.contains("npm:chalk@^5.6.2"));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("@std/path") && e.contains("built for parsing only")),
            "errors: {:?}",
            result.errors
        );
    }

    #[tokio::test]
    async fn non_registry_imports_are_ignored() {
        let jsr = jsr_mock(&[("@std/path", "1.0.0")]);
        let npm = MockRegistry::new("npm-test");
        let content = r#"{
  "imports": {
    "local/": "./src/",
    "remote": "https://example.com/mod.ts",
    "@std/path": "jsr:@std/path@^1.0.0"
  }
}
"#;

        let (result, _) = update_with(content, jsr, npm, UpdateOptions::new(true, false)).await;
        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 1);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn subpaths_and_import_map_slashes_survive_a_rewrite() {
        let jsr = jsr_mock(&[("@std/path", "1.2.0")]);
        let npm = MockRegistry::new("npm-test").with_version("pkg", "1.1.0");
        let content = r#"{
  "imports": {
    "@std/path/join": "jsr:/@std/path@^1.0.0/join",
    "mod": "npm:pkg@1.0.0/lib"
  }
}
"#;

        let (result, new_content) =
            update_with(content, jsr, npm, UpdateOptions::new(true, false)).await;

        assert_eq!(result.updated.len(), 2, "{:?}", result.errors);
        assert!(new_content.contains("jsr:/@std/path@^1.2.0/join"));
        assert!(new_content.contains("npm:pkg@1.1.0/lib"));
    }

    #[tokio::test]
    async fn jsonc_block_comments_and_trailing_commas_parse() {
        let jsr = jsr_mock(&[("@std/path", "1.2.0")]);
        let npm = MockRegistry::new("npm-test");
        let content = r#"{
  /* block comment
     spanning lines */
  "imports": {
    "@std/path": "jsr:@std/path@^1.0.0", // trailing comment
  },
}
"#;

        let (result, new_content) =
            update_with(content, jsr, npm, UpdateOptions::new(true, false)).await;
        assert_eq!(result.updated.len(), 1);
        assert!(new_content.contains("jsr:@std/path@^1.2.0"));
        assert!(new_content.contains("/* block comment"));
        assert!(new_content.contains("// trailing comment"));
        // The trailing commas the write preserved.
        assert!(new_content.contains("^1.2.0\", //"));
    }

    #[tokio::test]
    async fn prerelease_pin_stays_on_prereleases() {
        // get_latest_version returns the stable answer on a mock; the updater
        // fetched the prerelease channel because the pin is one, saw the
        // stable answer, and refused the silent promotion.
        let jsr = jsr_mock(&[("@std/path", "1.0.0")]);
        let npm = MockRegistry::new("npm-test");
        let content = r#"{ "imports": { "@std/path": "jsr:@std/path@2.0.0-rc.1" } }"#;

        let (result, new_content) =
            update_with(content, jsr, npm, UpdateOptions::new(true, false)).await;
        assert!(result.updated.is_empty(), "{:?}", result.updated);
        assert!(new_content.contains("jsr:@std/path@2.0.0-rc.1"));
    }

    #[tokio::test]
    async fn prerelease_pin_moves_to_newer_prerelease() {
        // The pin names the prerelease channel, so the lookup went to the
        // prerelease slot the mock declares and the newer candidate moves in
        // behind its operator.
        let jsr = MockRegistry::new("jsr-test")
            .with_prerelease("@std/path", "1.0.0", "2.0.0-rc.3")
            // The caret range is honored at lookup time, like the real
            // registry's semver resolution.
            .with_constrained("@std/path", "^2.0.0-rc.1", "2.0.0-rc.3");
        let content = r#"{ "imports": { "@std/path": "jsr:@std/path@^2.0.0-rc.1" } }"#;

        let (result, new_content) = DenoUpdater::new(Arc::new(jsr))
            .update_content(
                content.to_string(),
                &MockRegistry::new("npm-test"),
                UpdateOptions::new(true, false),
            )
            .await
            .unwrap();
        assert_eq!(result.updated.len(), 1, "{:?}", result.errors);
        assert!(new_content.contains("jsr:@std/path@^2.0.0-rc.3"));
    }

    #[test]
    fn specifiers_parse_with_parts_preserved() {
        let spec = parse_specifier("jsr:@std/path@^1.0.0/join").unwrap();
        assert_eq!(spec.scheme, Scheme::Jsr);
        assert_eq!(spec.prefix, "jsr:");
        assert_eq!(spec.package, "@std/path");
        assert_eq!(spec.range(), "^1.0.0");
        assert_eq!(spec.suffix, "/join");
        assert_eq!(spec.rebuild("2.0.0"), "jsr:@std/path@2.0.0/join");

        let dir = parse_specifier("jsr:/@std/async@^1.0.0/").unwrap();
        assert_eq!(dir.prefix, "jsr:/");
        assert_eq!(dir.package, "@std/async");
        assert_eq!(dir.suffix, "/");

        let plain = parse_specifier("jsr:@std/path").unwrap();
        assert_eq!(plain.range, None);
        assert_eq!(plain.package, "@std/path");

        let scoped_subpath = parse_specifier("npm:@scope/pkg/twin").unwrap();
        assert_eq!(scoped_subpath.package, "@scope/pkg");
        assert_eq!(scoped_subpath.suffix, "/twin");

        let unscoped_subpath = parse_specifier("npm:chalk/colors").unwrap();
        assert_eq!(unscoped_subpath.package, "chalk");
        assert_eq!(unscoped_subpath.suffix, "/colors");

        let scoped_npm = parse_specifier("npm:@scope/pkg@~2.1.0").unwrap();
        assert_eq!(scoped_npm.package, "@scope/pkg");
        assert_eq!(scoped_npm.range(), "~2.1.0");

        let unscoped = parse_specifier("npm:chalk@^5.0.0").unwrap();
        assert_eq!(unscoped.package, "chalk");

        for not_a_specifier in [
            "https://example.com/mod.ts",
            "./src/mod.ts",
            "./local/",
            "node:fs",
        ] {
            assert!(
                parse_specifier(not_a_specifier).is_none(),
                "{not_a_specifier}"
            );
        }
    }

    #[test]
    fn jsonc_stripper_keeps_strings_and_drops_comments() {
        let text = r#"{
  "url": "https://example.com/a//b", // comment after a string
  "nested": { "deep": "/* also not a comment */" },
  "after": 1
}
"#;
        let stripped = jsonc::strip_comments(text);
        assert!(stripped.contains("\"https://example.com/a//b\""));
        assert!(stripped.contains("\"/* also not a comment */\""));
        assert!(!stripped.contains("// comment after a string"));
        assert_eq!(stripped.lines().count(), text.lines().count());
    }

    #[test]
    fn trailing_commas_are_dropped_only_before_closers() {
        let stripped = jsonc::strip_trailing_commas(
            "{ \"a\": [1, 2,], \"b\": { \"c\": 1, }, \"d\": \"x, y\" }",
        );
        assert!(stripped.contains("[1, 2]"));
        assert!(stripped.contains("{ \"c\": 1 }"));
        assert!(stripped.contains("\"x, y\""));
    }

    #[test]
    fn malformed_without_recovery_is_an_error() {
        // A non-string import value is not a mapping and is skipped; only
        // broken JSON turns into an error.
        let json =
            parse_deno_manifest("{ \"imports\": { \"a\": \"npm:x@^1\", \"b\": 4 } }").unwrap();
        assert_eq!(imports_of(&json).len(), 1);
        assert!(parse_deno_manifest("{ not json").is_err());
    }

    #[test]
    fn line_index_finds_imports_and_not_comments() {
        let content = "// \"chalk\": \"npm:chalk@1.0.0\" prose\n{\n  \"imports\": {\n    \"chalk\": \"npm:chalk@^5.0.0\"\n  }\n}\n";
        assert_eq!(line_for_import(content, "chalk"), Some(4));
        assert_eq!(line_for_import(content, "missing"), None);
    }

    #[test]
    fn exact_pins_and_caret_tilde_ranges_are_parsed_at_their_version() {
        // Audit and align see a deno.json only through parse_dependencies, so
        // the specifier shapes most manifests use must reach them.
        let dir = tempdir().unwrap();
        let path = dir.path().join("deno.json");
        let content = r#"{
  "imports": {
    "@std/path": "jsr:@std/path@^1.0.0",
    "@std/fmt": "jsr:@std/fmt@~1.2.3",
    "lodash": "npm:lodash@4.17.15",
    "chalk": "npm:chalk@^5"
  }
}
"#;
        std::fs::write(&path, content).unwrap();

        let mut deps: Vec<(String, String, Option<RegistryRoute>)> = DenoUpdater::new_parse_only()
            .parse_dependencies(&path)
            .unwrap()
            .into_iter()
            .map(|d| (d.name, d.version, d.registry))
            .collect();
        deps.sort();
        assert_eq!(
            deps,
            vec![
                (
                    "@std/fmt".to_string(),
                    "1.2.3".to_string(),
                    Some(RegistryRoute::Jsr)
                ),
                (
                    "@std/path".to_string(),
                    "1.0.0".to_string(),
                    Some(RegistryRoute::Jsr)
                ),
                (
                    "chalk".to_string(),
                    "5.0.0".to_string(),
                    Some(RegistryRoute::Npm)
                ),
                (
                    "lodash".to_string(),
                    "4.17.15".to_string(),
                    Some(RegistryRoute::Npm)
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_caret_range_holds_a_release_inside_the_cooldown_window() {
        use crate::cooldown::CooldownPolicy;
        use chrono::{Duration, Utc};

        let now = Utc::now();
        let content = "{\n  \"imports\": {\n    \"chalk\": \"npm:chalk@^5.0.0\"\n  }\n}\n";
        let npm = MockRegistry::new("npm-test")
            .with_version("chalk", "5.6.2")
            .with_version_meta(
                "chalk",
                "5.0.0",
                Some(now - Duration::days(400)),
                false,
                false,
            )
            .with_version_meta(
                "chalk",
                "5.6.1",
                Some(now - Duration::days(30)),
                false,
                false,
            )
            .with_version_meta(
                "chalk",
                "5.6.2",
                Some(now - Duration::days(1)),
                false,
                false,
            );
        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: std::collections::HashMap::new(),
            strict: false,
            force_override: None,
        };
        let options = UpdateOptions::new(false, false).with_cooldown_policy(policy, now);

        let (result, new_content) = update_with(content, jsr_mock(&[]), npm, options).await;

        assert!(
            new_content.contains("npm:chalk@^5.6.1"),
            "the newest release outside the window must be chosen, got: {new_content}"
        );
        assert_eq!(
            result.held_back.len(),
            1,
            "held_back: {:?}",
            result.held_back
        );
        assert_eq!(result.held_back[0].0, "chalk");
        assert_eq!(
            result.held_back[0].3, "5.6.2",
            "held_back: {:?}",
            result.held_back
        );
    }

    #[tokio::test]
    async fn a_scopes_entry_with_the_same_key_is_left_alone() {
        // Deno resolves "scopes" per directory; only the top-level "imports"
        // map is what this updater read and decided for.
        let jsr = jsr_mock(&[("@std/path", "1.2.0")]);
        let npm = MockRegistry::new("npm-test");
        let content = r#"{
  "scopes": {
    "./vendor/": {
      "@std/path": "jsr:@std/path@^1.0.0"
    }
  },
  "imports": {
    "@std/path": "jsr:@std/path@^1.0.0"
  }
}
"#;

        let (result, new_content) =
            update_with(content, jsr, npm, UpdateOptions::new(false, false)).await;

        assert_eq!(result.updated.len(), 1, "{:?}", result.updated);
        assert_eq!(
            result.updated[0].3,
            Some(8),
            "reported line: {:?}",
            result.updated
        );
        assert_eq!(
            new_content,
            content.replacen(
                "  \"imports\": {\n    \"@std/path\": \"jsr:@std/path@^1.0.0\"",
                "  \"imports\": {\n    \"@std/path\": \"jsr:@std/path@^1.2.0\"",
                1
            )
        );
    }
}
