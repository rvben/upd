//! Parsing and rewriting of `upd:` / `renovate:` version annotations.
//!
//! Everything here is pure text work: no file type, no registry, no options.
//! Both write paths (non-interactive and interactive) share this module.

use crate::updater::Lang;

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
}
