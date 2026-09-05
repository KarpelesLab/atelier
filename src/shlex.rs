//! Quote-aware tokenizer for slash-command argument lines.
//!
//! Splits a command line into arguments, honoring single and double quotes so a
//! single argument may contain spaces — e.g.
//! `/mcp add srv sh -c "server --root /a b"` yields the script as one token.
//!
//! # Contract (stable — implementer must not change this signature)
//!
//! [`split`] returns the token list, or an error for an unterminated quote.
//!
//! ## For the implementer (owns `src/shlex.rs`)
//!
//! Replace the whitespace-only stub with a real quote-aware splitter (single
//! and double quotes; a backslash escape inside double quotes is a plus). Add
//! unit tests. Do not edit files outside `src/shlex.rs`. Keep the
//! `#[allow(dead_code)]` — the caller is wired up separately.

use anyhow::{Result, bail};

/// Split `input` into arguments, honoring `'…'` and `"…"` quoting.
///
/// - Unquoted spaces/tabs separate tokens.
/// - Single quotes preserve their contents literally (no escapes).
/// - Double quotes preserve spaces and support `\"` and `\\` escapes; any
///   other backslash is kept literal.
/// - Quoted and unquoted segments may be concatenated within a single token
///   (e.g. `a"b c"d` -> `ab cd`), and an empty quoted segment can produce an
///   empty-string token (e.g. `''` -> `""`).
/// - An unterminated quote is an error.
#[allow(dead_code)]
pub fn split(input: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current: Option<String> = None;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if let Some(tok) = current.take() {
                    tokens.push(tok);
                }
            }
            '\'' => {
                let buf = current.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => buf.push(ch),
                        None => bail!("unterminated quote"),
                    }
                }
            }
            '"' => {
                let buf = current.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('"') => buf.push('"'),
                            Some('\\') => buf.push('\\'),
                            Some(other) => {
                                buf.push('\\');
                                buf.push(other);
                            }
                            None => bail!("unterminated quote"),
                        },
                        Some(ch) => buf.push(ch),
                        None => bail!("unterminated quote"),
                    }
                }
            }
            ch => {
                current.get_or_insert_with(String::new).push(ch);
            }
        }
    }

    if let Some(tok) = current.take() {
        tokens.push(tok);
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        assert_eq!(split("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn whitespace_only_input() {
        assert_eq!(split("   \t  ").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn plain_words() {
        assert_eq!(
            split("foo bar  baz\tqux").unwrap(),
            vec!["foo", "bar", "baz", "qux"]
        );
    }

    #[test]
    fn leading_and_trailing_whitespace_ignored() {
        assert_eq!(split("  foo bar  ").unwrap(), vec!["foo", "bar"]);
    }

    #[test]
    fn single_quoted_spaces() {
        assert_eq!(split("'a b'").unwrap(), vec!["a b"]);
    }

    #[test]
    fn single_quotes_no_escapes() {
        // Inside single quotes, backslash is literal.
        assert_eq!(split(r"'a\b'").unwrap(), vec![r"a\b"]);
    }

    #[test]
    fn double_quoted_spaces() {
        assert_eq!(split("\"a b\"").unwrap(), vec!["a b"]);
    }

    #[test]
    fn double_quote_escapes() {
        assert_eq!(split(r#""a\"b""#).unwrap(), vec![r#"a"b"#]);
        assert_eq!(split(r#""a\\b""#).unwrap(), vec![r"a\b"]);
    }

    #[test]
    fn double_quote_other_backslash_is_literal() {
        // Backslash followed by something other than " or \ stays as-is.
        assert_eq!(split(r#""a\nb""#).unwrap(), vec![r"a\nb"]);
    }

    #[test]
    fn adjacent_quote_concatenation() {
        assert_eq!(split(r#"a"b c"d"#).unwrap(), vec!["ab cd"]);
        assert_eq!(split("'a'\"b\"c").unwrap(), vec!["abc"]);
    }

    #[test]
    fn empty_quote_token() {
        assert_eq!(split("''").unwrap(), vec![""]);
        assert_eq!(split("\"\"").unwrap(), vec![""]);
    }

    #[test]
    fn empty_quote_among_other_tokens() {
        assert_eq!(split("a '' b").unwrap(), vec!["a", "", "b"]);
    }

    #[test]
    fn unterminated_single_quote_errors() {
        let err = split("'abc").unwrap_err();
        assert!(err.to_string().contains("unterminated quote"));
    }

    #[test]
    fn unterminated_double_quote_errors() {
        let err = split("\"abc").unwrap_err();
        assert!(err.to_string().contains("unterminated quote"));
    }

    #[test]
    fn unterminated_double_quote_trailing_backslash_errors() {
        let err = split("\"abc\\").unwrap_err();
        assert!(err.to_string().contains("unterminated quote"));
    }

    #[test]
    fn realistic_command_line() {
        assert_eq!(
            split(r#"add srv sh -c "server --root /a b""#).unwrap(),
            vec!["add", "srv", "sh", "-c", "server --root /a b"]
        );
    }
}
