//! Parsing and rewriting of `upd:` / `renovate:` version annotations.
//!
//! Everything here is pure text work: no file type, no registry, no options.
//! Both write paths (non-interactive and interactive) share this module.

use crate::align;
use crate::updater::{GemfileUpdater, Lang};
use crate::version::is_prerelease_pep440;
use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

/// Every warning about a source `upd` does not support starts with this, so the
/// updater can emit one warning per distinct source instead of one per line.
pub const UNSUPPORTED_SOURCE_PREFIX: &str = "unsupported source '";

const UPD_MARKER: &[u8] = b"upd:";
const RENOVATE_MARKER: &[u8] = b"renovate:";

/// The registry an annotated line resolves against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnnotationSource {
    PyPi,
    Npm,
    Crates,
    Go,
    RubyGems,
    NuGet,
    GitHubReleases,
}

impl AnnotationSource {
    /// The canonical `<source>` token, as it should be written in an annotation.
    pub fn token(self) -> &'static str {
        match self {
            Self::PyPi => "pypi",
            Self::Npm => "npm",
            Self::Crates => "crates",
            Self::Go => "go",
            Self::RubyGems => "rubygems",
            Self::NuGet => "nuget",
            Self::GitHubReleases => "github-releases",
        }
    }

    /// What `Registry::name()` returns for this source's registry. Not the same
    /// string as `token()` for `crates` and `go`.
    pub fn registry_name(self) -> &'static str {
        match self {
            Self::PyPi => "pypi",
            Self::Npm => "npm",
            Self::Crates => "crates.io",
            Self::Go => "go-proxy",
            Self::RubyGems => "rubygems",
            Self::NuGet => "nuget",
            Self::GitHubReleases => "github-releases",
        }
    }

    /// The ecosystem whose version comparison and prerelease rules apply.
    pub fn lang(self) -> Lang {
        match self {
            Self::PyPi => Lang::Python,
            Self::Npm => Lang::Node,
            Self::Crates => Lang::Rust,
            Self::Go => Lang::Go,
            Self::RubyGems => Lang::Ruby,
            Self::NuGet => Lang::DotNet,
            Self::GitHubReleases => Lang::GithubReleases,
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "pypi" => Some(Self::PyPi),
            "npm" => Some(Self::Npm),
            "crate" | "crates" => Some(Self::Crates),
            "go" => Some(Self::Go),
            "rubygems" => Some(Self::RubyGems),
            "nuget" => Some(Self::NuGet),
            "github-releases" => Some(Self::GitHubReleases),
            _ => None,
        }
    }
}

/// One parsed annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub source: AnnotationSource,
    pub package: String,
    /// Byte offset of the first comment introducer on the line. Only bytes
    /// before this are searched for a version token.
    pub comment_start: usize,
}

/// What a line yielded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    /// No marker present. Not a diagnostic: most lines look like this.
    None,
    Found(Annotation),
    /// Reason, surfaced as a warning. The line is left untouched.
    Malformed(String),
}

#[derive(Clone, Copy)]
enum MarkerKind {
    Upd,
    Renovate,
}

pub fn parse_line(line: &str) -> ParseOutcome {
    let bytes = line.as_bytes();
    let Some(comment_start) = first_comment_introducer(bytes) else {
        return ParseOutcome::None;
    };

    let hits = marker_hits(bytes, comment_start);
    let (kind, body_start) = match hits.as_slice() {
        [] => return ParseOutcome::None,
        [single] => *single,
        _ => return ParseOutcome::Malformed("two annotations on one line".to_string()),
    };

    // The body comes from the original line, not a lowercased copy, because
    // `<package>` is passed to the registry exactly as written.
    let body = &line[body_start..];
    match kind {
        MarkerKind::Upd => parse_upd_body(body, comment_start),
        MarkerKind::Renovate => parse_renovate_body(body, comment_start),
    }
}

fn first_comment_introducer(bytes: &[u8]) -> Option<usize> {
    (0..bytes.len())
        .find(|&i| bytes[i] == b'#' || (bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/')))
}

/// Byte offsets just past every marker at or after `comment_start`. Comparison
/// is on bytes, so a multi-byte code point can never be sliced mid-character;
/// both markers are ASCII, so a continuation byte cannot match one.
fn marker_hits(bytes: &[u8], comment_start: usize) -> Vec<(MarkerKind, usize)> {
    let mut hits = Vec::new();
    let mut i = comment_start;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let hit = if starts_with_ignore_ascii_case(rest, RENOVATE_MARKER) {
            Some((MarkerKind::Renovate, RENOVATE_MARKER.len()))
        } else if starts_with_ignore_ascii_case(rest, UPD_MARKER) {
            Some((MarkerKind::Upd, UPD_MARKER.len()))
        } else {
            None
        };
        match hit {
            Some((kind, len)) if starts_token(bytes, i) => {
                hits.push((kind, i + len));
                i += len;
            }
            _ => i += 1,
        }
    }
    hits
}

fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

/// A marker only counts when it starts a token, so `backupd:` is not one.
fn starts_token(bytes: &[u8], i: usize) -> bool {
    match i.checked_sub(1).map(|prev| bytes[prev]) {
        None => true,
        Some(b) => b.is_ascii_whitespace() || b == b'#' || b == b'/',
    }
}

fn parse_upd_body(body: &str, comment_start: usize) -> ParseOutcome {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    match tokens.as_slice() {
        [source, package] => resolve(source, package, comment_start),
        _ => ParseOutcome::Malformed(format!(
            "malformed annotation: expected `upd: <source> <package>`, found {} token(s)",
            tokens.len()
        )),
    }
}

fn parse_renovate_body(body: &str, comment_start: usize) -> ParseOutcome {
    let mut datasource: Option<&str> = None;
    let mut dep_name: Option<&str> = None;

    for token in body.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            return ParseOutcome::Malformed(format!(
                "malformed annotation: expected `key=value` in a renovate comment, found `{token}`"
            ));
        };
        match key.to_ascii_lowercase().as_str() {
            // A repeated key takes the last value.
            "datasource" => datasource = Some(value),
            "depname" => dep_name = Some(value),
            other => {
                // Honouring depName while ignoring extractVersion would rewrite
                // a version the annotation says to transform.
                return ParseOutcome::Malformed(format!(
                    "unhonoured renovate key '{other}': upd honours only datasource= and depName="
                ));
            }
        }
    }

    match (datasource, dep_name) {
        (Some(source), Some(package)) => resolve(source, package, comment_start),
        _ => ParseOutcome::Malformed(
            "malformed annotation: a renovate comment needs both datasource= and depName="
                .to_string(),
        ),
    }
}

fn resolve(source: &str, package: &str, comment_start: usize) -> ParseOutcome {
    if let Some(source) = AnnotationSource::from_token(source) {
        return ParseOutcome::Found(Annotation {
            source,
            package: package.to_string(),
            comment_start,
        });
    }

    if source.eq_ignore_ascii_case("github-tags") {
        return ParseOutcome::Malformed(format!(
            "{UNSUPPORTED_SOURCE_PREFIX}github-tags': not a synonym for github-releases, \
             a repo can publish tags without publishing releases"
        ));
    }

    ParseOutcome::Malformed(format!(
        "{UNSUPPORTED_SOURCE_PREFIX}{}'",
        source.to_ascii_lowercase()
    ))
}

/// A field is a version candidate only when this matches it in its entirety.
/// Two alternatives: a dotted number with an optional suffix, or a `v`-prefixed
/// bare major with an optional suffix. A bare `4` is neither.
static VERSION_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:v?\d+(?:\.\d+)+(?:[-+.][0-9A-Za-z.+-]*)?|v\d+(?:[-+.][0-9A-Za-z.+-]*)?)$")
        .expect("the version field pattern is a compile-time constant")
});

/// Candidate version spans in the code portion of `line`, ascending.
///
/// Fields are maximal runs of `[A-Za-z0-9_.+-]`, so a version inside a file name
/// (`bao_2.6.1_linux.tar.gz`) is part of one larger field and is not a candidate,
/// while a version after a `:` or `/` (`bun:1.2.4`) is a field of its own.
pub fn version_spans(line: &str, comment_start: usize) -> Vec<Range<usize>> {
    let code = &line[..comment_start];
    let bytes = code.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if !is_field_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_field_byte(bytes[i]) {
            i += 1;
        }
        // Every field byte is ASCII, so `start` and `i` are char boundaries.
        if VERSION_FIELD.is_match(&code[start..i]) {
            spans.push(start..i);
        }
    }

    spans
}

fn is_field_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-')
}

/// Whether `value` is a version by the Section 3.2 grammar, matched in its
/// entirety. Used to validate a registry answer before it is written to a line
/// that upd does not otherwise understand.
pub fn is_version_token(value: &str) -> bool {
    VERSION_FIELD.is_match(value)
}

/// The distinct values named by `spans` on `text`, in first-seen order.
///
/// Both write paths need this: the non-interactive scan uses it to decide
/// whether a line's version is unambiguous, and the interactive apply step
/// uses it again, on the same line read fresh from disk, to check the line
/// still matches what the user approved. A shared helper keeps that definition
/// of "distinct" identical in both places rather than two copies drifting.
pub fn distinct_values<'a>(text: &'a str, spans: &[Range<usize>]) -> Vec<&'a str> {
    let mut distinct: Vec<&str> = Vec::new();
    for span in spans {
        let value = &text[span.clone()];
        if !distinct.contains(&value) {
            distinct.push(value);
        }
    }
    distinct
}

/// Rewrite the given spans to `new_version`, right to left so earlier spans keep
/// their offsets. Never a `String::replace`, which would also hit the comment.
pub fn rewrite_spans(line: &str, spans: &[Range<usize>], new_version: &str) -> String {
    let mut out = line.to_string();
    for span in spans.iter().rev() {
        out.replace_range(span.clone(), new_version);
    }
    out
}

/// Give `candidate` the leading `v` of `original`, or take it away.
///
/// Strips exactly one `v`, never `trim_start_matches`, so a doubled prefix is
/// not silently swallowed.
pub fn reapply_v_prefix(original: &str, candidate: &str) -> String {
    let bare = candidate.strip_prefix('v').unwrap_or(candidate);
    if original.starts_with('v') {
        format!("v{bare}")
    } else {
        bare.to_string()
    }
}

/// Positive dual of `align::is_stable_version`, for tokens that have already
/// passed the Section 3.4 guards. Python is not the negation of it: both PEP 440
/// predicates report `false` for an unparseable string, which is why such a token
/// is refused before it ever reaches this helper.
///
/// Ruby delegates to `GemfileUpdater`'s classifier, not to `align`'s: the align
/// arm calls `8.0.0.dev1` stable, and asking for the latest stable release there
/// would write it straight over a dev pin.
pub fn is_prerelease_token(token: &str, lang: Lang) -> bool {
    let v = token.strip_prefix('v').unwrap_or(token);
    match lang {
        Lang::Python => is_prerelease_pep440(v),
        Lang::Ruby => GemfileUpdater::is_prerelease_ruby(v),
        _ => !align::is_stable_version(v, lang),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(line: &str) -> Annotation {
        match parse_line(line) {
            ParseOutcome::Found(a) => a,
            other => panic!("expected an annotation on {line:?}, got {other:?}"),
        }
    }

    fn malformed(line: &str) -> String {
        match parse_line(line) {
            ParseOutcome::Malformed(reason) => reason,
            other => panic!("expected a malformed annotation on {line:?}, got {other:?}"),
        }
    }

    #[test]
    fn parses_the_upd_form() {
        let line = "BAO_VERSION     ?= 2.6.1      # upd: pypi openbao-cli";
        let a = found(line);
        assert_eq!(a.source, AnnotationSource::PyPi);
        assert_eq!(a.package, "openbao-cli");
        assert_eq!(a.comment_start, line.find('#').unwrap());
    }

    #[test]
    fn parses_the_renovate_form_in_either_order() {
        let a = found("RUFF ?= 0.14.2 # renovate: datasource=pypi depName=ruff");
        assert_eq!(a.source, AnnotationSource::PyPi);
        assert_eq!(a.package, "ruff");

        let b = found("RUFF ?= 0.14.2 # renovate: depName=ruff datasource=pypi");
        assert_eq!(b.source, AnnotationSource::PyPi);
        assert_eq!(b.package, "ruff");
    }

    #[test]
    fn marker_and_source_are_case_insensitive_but_package_is_verbatim() {
        let a = found("X = 1.0 # UPD: PyPI Requests");
        assert_eq!(a.source, AnnotationSource::PyPi);
        assert_eq!(a.package, "Requests");
    }

    #[test]
    fn accepts_the_double_slash_introducer() {
        let a = found("const BUN = \"1.2.4\"; // upd: github-releases oven-sh/bun");
        assert_eq!(a.source, AnnotationSource::GitHubReleases);
        assert_eq!(a.package, "oven-sh/bun");
    }

    #[test]
    fn accepts_both_crate_spellings() {
        assert_eq!(
            found("X = 1.0 # upd: crates ripgrep").source,
            AnnotationSource::Crates
        );
        assert_eq!(
            found("X = 1.0 # renovate: datasource=crate depName=ripgrep").source,
            AnnotationSource::Crates
        );
    }

    #[test]
    fn a_marker_without_a_comment_introducer_is_not_an_annotation() {
        assert!(matches!(parse_line("upd: pypi ruff"), ParseOutcome::None));
    }

    #[test]
    fn a_marker_inside_a_word_is_not_an_annotation() {
        assert!(matches!(
            parse_line("X = 1.0 # backupd: pypi ruff"),
            ParseOutcome::None
        ));
    }

    #[test]
    fn a_line_without_any_marker_is_not_an_annotation() {
        assert!(matches!(
            parse_line("X = 1.0 # just a comment"),
            ParseOutcome::None
        ));
    }

    #[test]
    fn comment_start_is_the_first_introducer_on_the_line() {
        let line = "X = 1.0 # note # upd: pypi ruff";
        let a = found(line);
        assert_eq!(a.comment_start, line.find('#').unwrap());
        assert_eq!(&line[..a.comment_start], "X = 1.0 ");
    }

    #[test]
    fn a_third_token_is_malformed() {
        let reason = malformed("X = 1.0 # upd: pypi ruff extra");
        assert!(reason.contains("malformed annotation"), "{reason}");
        assert!(reason.contains("3 token(s)"), "{reason}");
    }

    #[test]
    fn a_second_marker_is_malformed() {
        let reason = malformed("X = 1.0 # upd: pypi ruff # upd: npm ruff");
        assert_eq!(reason, "two annotations on one line");
    }

    #[test]
    fn an_unknown_renovate_key_names_the_key() {
        let reason = malformed(
            "X = 1.0 # renovate: datasource=pypi depName=ruff extractVersion=^v(?<version>.*)$",
        );
        assert!(
            reason.starts_with("unhonoured renovate key 'extractversion'"),
            "{reason}"
        );
    }

    #[test]
    fn a_renovate_comment_missing_a_key_is_malformed() {
        let reason = malformed("X = 1.0 # renovate: datasource=pypi");
        assert!(reason.contains("datasource= and depName="), "{reason}");
    }

    #[test]
    fn an_unsupported_source_is_named() {
        let reason = malformed("X = 1.0 # upd: docker library/redis");
        assert_eq!(reason, "unsupported source 'docker'");
        assert!(reason.starts_with(UNSUPPORTED_SOURCE_PREFIX));
    }

    #[test]
    fn github_tags_is_refused_as_not_a_synonym_for_github_releases() {
        let reason = malformed("X = 1.0 # upd: github-tags oven-sh/bun");
        assert!(reason.starts_with(UNSUPPORTED_SOURCE_PREFIX), "{reason}");
        assert!(reason.contains("github-releases"), "{reason}");
    }

    #[test]
    fn terraform_is_not_a_v1_source() {
        assert_eq!(
            malformed("X = 1.0 # upd: terraform hashicorp/aws"),
            "unsupported source 'terraform'"
        );
    }

    #[test]
    fn a_repeated_renovate_key_takes_the_last_value() {
        let a = found("X = 1.0 # renovate: datasource=npm datasource=pypi depName=ruff");
        assert_eq!(a.source, AnnotationSource::PyPi);
    }

    #[test]
    fn source_tokens_and_langs_are_stable() {
        use crate::updater::Lang;
        let table = [
            (AnnotationSource::PyPi, "pypi", "pypi", Lang::Python),
            (AnnotationSource::Npm, "npm", "npm", Lang::Node),
            (AnnotationSource::Crates, "crates", "crates.io", Lang::Rust),
            (AnnotationSource::Go, "go", "go-proxy", Lang::Go),
            (
                AnnotationSource::RubyGems,
                "rubygems",
                "rubygems",
                Lang::Ruby,
            ),
            (AnnotationSource::NuGet, "nuget", "nuget", Lang::DotNet),
            (
                AnnotationSource::GitHubReleases,
                "github-releases",
                "github-releases",
                Lang::GithubReleases,
            ),
        ];
        for (source, token, registry_name, lang) in table {
            assert_eq!(source.token(), token);
            assert_eq!(source.registry_name(), registry_name);
            assert_eq!(source.lang(), lang);
        }
    }

    #[test]
    fn a_multibyte_line_does_not_panic() {
        // Byte scanning must not slice through a UTF-8 code point.
        assert!(matches!(
            parse_line("VERSIÓN = 1.0 # naïve comment"),
            ParseOutcome::None
        ));
        let a = found("VERSIÓN = 1.0 # upd: pypi ruff");
        assert_eq!(a.package, "ruff");
    }

    fn spans_of(line: &str) -> Vec<String> {
        let comment_start = line.find('#').unwrap_or(line.len());
        version_spans(line, comment_start)
            .into_iter()
            .map(|s| line[s].to_string())
            .collect()
    }

    #[test]
    fn finds_a_plain_version_token() {
        assert_eq!(
            spans_of("BAO_VERSION     ?= 2.6.1      # upd: pypi openbao-cli"),
            ["2.6.1"]
        );
    }

    #[test]
    fn finds_the_version_inside_an_image_reference() {
        // `/` and `:` are not field bytes, so the fields are ghcr.io, oven-sh,
        // bun and 1.2.4; only the last one matches in its entirety.
        assert_eq!(
            spans_of("IMAGE := ghcr.io/oven-sh/bun:1.2.4  # upd: github-releases oven-sh/bun"),
            ["1.2.4"]
        );
    }

    #[test]
    fn accepts_a_v_prefixed_token_and_a_bare_v_major() {
        assert_eq!(spans_of("A := v0.2.5 # upd: go example.com/m"), ["v0.2.5"]);
        assert_eq!(spans_of("A := v4 # upd: github-releases o/r"), ["v4"]);
    }

    #[test]
    fn a_bare_integer_is_not_a_version() {
        assert!(spans_of("WORKERS := 4 # upd: pypi ruff").is_empty());
    }

    #[test]
    fn a_date_is_not_a_version() {
        assert!(spans_of("BUILT := 2026-08-11 # upd: pypi ruff").is_empty());
    }

    #[test]
    fn a_version_embedded_in_a_file_name_is_not_a_candidate() {
        // `_` is a field byte, so the whole file name is one field and it does
        // not match in its entirety.
        assert!(spans_of("TARBALL := bao_2.6.1_linux.tar.gz # upd: pypi openbao-cli").is_empty());
    }

    #[test]
    fn a_dotted_suffix_belongs_to_the_same_span() {
        assert_eq!(spans_of("A := 1.2.3.post1 # upd: pypi x"), ["1.2.3.post1"]);
        assert_eq!(
            spans_of("A := 8.0.0.beta1 # upd: rubygems x"),
            ["8.0.0.beta1"]
        );
    }

    #[test]
    fn the_comment_portion_is_never_searched() {
        let line = "A := 1.0.0 # upd: pypi ruff";
        assert_eq!(spans_of(line), ["1.0.0"]);
        let with_noise = "A := 1.0.0 # see 9.9.9 too";
        let comment_start = with_noise.find('#').unwrap();
        assert_eq!(version_spans(with_noise, comment_start).len(), 1);
    }

    #[test]
    fn every_candidate_span_is_returned_in_ascending_order() {
        let line = "A := 1.2.3 B := 1.2.3 # upd: pypi x";
        let comment_start = line.find('#').unwrap();
        let spans = version_spans(line, comment_start);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].start < spans[1].start);
        assert_eq!(&line[spans[0].clone()], "1.2.3");
        assert_eq!(&line[spans[1].clone()], "1.2.3");
    }

    #[test]
    fn distinct_values_collapses_a_repeated_value_to_one() {
        let line = "A := 1.2.3 B := 1.2.3 # upd: pypi x";
        let comment_start = line.find('#').unwrap();
        let spans = version_spans(line, comment_start);
        assert_eq!(distinct_values(line, &spans), vec!["1.2.3"]);
    }

    #[test]
    fn distinct_values_keeps_two_different_values_separate() {
        let line = "IMG ?= app:1.2.3 helper:2.0.0 # upd: pypi x";
        let comment_start = line.find('#').unwrap();
        let spans = version_spans(line, comment_start);
        assert_eq!(distinct_values(line, &spans), vec!["1.2.3", "2.0.0"]);
    }

    #[test]
    fn rewrite_spans_replaces_right_to_left() {
        let line = "A := 1.2.3 B := 1.2.3 # upd: pypi x";
        let comment_start = line.find('#').unwrap();
        let spans = version_spans(line, comment_start);
        assert_eq!(
            rewrite_spans(line, &spans, "10.0.0"),
            "A := 10.0.0 B := 10.0.0 # upd: pypi x"
        );
    }

    #[test]
    fn rewrite_spans_leaves_the_comment_untouched() {
        let line = "A := 1.2.3 # upd: pypi x";
        let comment_start = line.find('#').unwrap();
        let spans = version_spans(line, comment_start);
        assert_eq!(
            rewrite_spans(line, &spans, "2.0.0"),
            "A := 2.0.0 # upd: pypi x"
        );
    }

    #[test]
    fn reapply_v_prefix_follows_the_original_token() {
        assert_eq!(reapply_v_prefix("v1.2.3", "2.0.0"), "v2.0.0");
        assert_eq!(reapply_v_prefix("v1.2.3", "v2.0.0"), "v2.0.0");
        assert_eq!(reapply_v_prefix("1.2.3", "v2.0.0"), "2.0.0");
        assert_eq!(reapply_v_prefix("1.2.3", "2.0.0"), "2.0.0");
        // Exactly one `v` is stripped, so a doubled prefix is not eaten whole.
        assert_eq!(reapply_v_prefix("1.2.3", "vv2.0.0"), "v2.0.0");
    }

    #[test]
    fn classifies_prereleases_per_ecosystem() {
        use crate::updater::Lang;
        assert!(!is_prerelease_token("1.2.3", Lang::Python));
        assert!(is_prerelease_token("1.2.3rc1", Lang::Python));
        assert!(is_prerelease_token("v1.2.3rc1", Lang::Python));
        assert!(!is_prerelease_token("1.0.0", Lang::Node));
        assert!(is_prerelease_token("1.0.0-rc.1", Lang::Node));
        assert!(!is_prerelease_token("v1.2.0", Lang::GithubReleases));
        assert!(is_prerelease_token("v1.2.0-rc1", Lang::GithubReleases));
    }

    #[test]
    fn ruby_uses_the_gemfile_classifier_not_the_align_one() {
        use crate::updater::Lang;
        // align::is_stable_version calls all of these stable; GemfileUpdater's
        // classifier does not, and writing a stable release over any of them
        // would promote a pin the user chose on purpose.
        for token in ["8.0.0.dev1", "8.0.0.a1", "8.0.0.b2", "2.0.0.final"] {
            assert!(
                is_prerelease_token(token, Lang::Ruby),
                "{token} must classify as a Ruby prerelease"
            );
        }
        assert!(!is_prerelease_token("8.0.0", Lang::Ruby));
    }

    #[test]
    fn is_version_token_accepts_versions_and_rejects_registry_junk() {
        assert!(is_version_token("1.2.3"));
        assert!(is_version_token("v1.2.3"));
        assert!(is_version_token("2.0.0-rc.1"));
        assert!(is_version_token("v2"));
        assert!(is_version_token("1.2.3+build.5"));
        // A registry can answer with a tag, a branch, a digest or a range.
        assert!(!is_version_token("latest"));
        assert!(!is_version_token("main"));
        assert!(!is_version_token("^1.2.3"));
        assert!(!is_version_token("1.2.3 "));
        assert!(!is_version_token("release-1.2.3"));
        assert!(!is_version_token(""));
    }
}
