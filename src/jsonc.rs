//! JSONC reading: JSON with comments and trailing commas, as `deno.jsonc` —
//! and `deno.json` in the wild — are written.
//!
//! The readers here exist for the *read* side only: they turn permissive JSON
//! into a [`serde_json::Value`] so a manifest's structure can be inspected.
//! Writing back never goes through them — updaters rewrite the matched
//! specifier in the original bytes, which is what keeps comments and
//! formatting alive in the files this tool edits.
//!
//! Two relaxations are handled, both string-aware so a quote inside a string
//! cannot turn into structure:
//!
//! * `//` line comments and `/* */` block comments are dropped. The newline
//!   that ends a line comment survives, so the stripped text keeps the
//!   original's line count.
//! * a comma directly followed by a closing brace or bracket (past whitespace)
//!   is dropped. Deno's parser tolerates the trailing commas TypeScript's
//!   ecosystem writes; `serde_json` does not.

use anyhow::{Result, anyhow};
use serde_json::Value;

/// Strip `//` and `/* */` comments from JSONC, preserving string contents.
pub fn strip_comments(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else {
                match ch {
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    for skipped in chars.by_ref() {
                        if skipped == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    while let Some(skipped) = chars.next() {
                        if skipped == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            break;
                        }
                    }
                    // One space keeps the tokens the comment separated from
                    // fusing into one.
                    out.push(' ');
                }
                _ => out.push(ch),
            },
            _ => out.push(ch),
        }
    }

    out
}

/// Remove trailing commas from JSONC, outside strings.
pub fn strip_trailing_commas(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else {
                match ch {
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            ',' => {
                let mut lookahead = chars.clone();
                let next_significant = loop {
                    match lookahead.peek() {
                        Some(c) if c.is_whitespace() => {
                            lookahead.next();
                        }
                        other => break other.copied(),
                    }
                };
                if matches!(next_significant, Some('}') | Some(']')) {
                    continue;
                }
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }

    out
}

/// Parse permissive JSONC into JSON.
///
/// An empty input parses as an empty object, the way a zero-byte manifest is
/// an empty project rather than a broken one.
pub fn parse(content: &str) -> Result<Value> {
    if content.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let stripped = strip_trailing_commas(&strip_comments(content));
    serde_json::from_str(&stripped).map_err(|e| anyhow!("invalid JSONC: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_are_dropped_but_strings_and_newlines_survive() {
        let text = "{\n  \"url\": \"https://example.com/a//b\", // comment after a string\n  \"after\": 1\n}\n";
        let stripped = strip_comments(text);
        assert!(stripped.contains("\"https://example.com/a//b\""));
        assert!(!stripped.contains("// comment after a string"));
        // Line comments keep their newline, so line-based reporting stays
        // honest for the files whose lines carry them.
        assert_eq!(stripped.lines().count(), text.lines().count());
    }

    #[test]
    fn block_comments_collapse_to_one_space() {
        let stripped = strip_comments("{ \"a\": 1, /* block\n   comment */ \"b\": 2 }");
        assert_eq!(stripped, "{ \"a\": 1,   \"b\": 2 }");
    }

    #[test]
    fn unterminated_block_comment_swallows_the_rest() {
        let stripped = strip_comments("{ \"a\": 1 } /* never closed");
        assert_eq!(stripped.trim_end(), "{ \"a\": 1 }");
    }

    #[test]
    fn escapes_keep_strings_closed() {
        let stripped = strip_comments(r#"{ "q": "a \"quoted\" // thing" } // tail"#);
        assert!(stripped.contains(r#""a \"quoted\" // thing""#));
        assert!(!stripped.contains("tail"));
    }

    #[test]
    fn trailing_commas_are_dropped_only_before_closers() {
        let stripped =
            strip_trailing_commas("{ \"a\": [1, 2,], \"b\": { \"c\": 1, }, \"d\": \"x, y\" }");
        assert!(stripped.contains("[1, 2]"));
        assert!(stripped.contains("{ \"c\": 1 }"));
        assert!(stripped.contains("\"x, y\""));
    }

    #[test]
    fn parse_reads_what_deno_writes() {
        let value = parse(
            r#"{
  // a deno.jsonc comment ahead of the keys it describes
  "lock": false,
  "imports": { "@std/path": "jsr:@std/path@^1.0.0", },
}"#,
        )
        .unwrap();
        assert_eq!(value["lock"], false);
        assert!(value["imports"]["@std/path"] == "jsr:@std/path@^1.0.0");
        assert!(parse("{ not json").is_err());
        assert_eq!(parse("").unwrap()["__absent__"], Value::Null);
    }
}
