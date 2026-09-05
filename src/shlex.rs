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

use anyhow::Result;

/// Split `input` into arguments, honoring `'…'` and `"…"` quoting.
#[allow(dead_code)]
pub fn split(input: &str) -> Result<Vec<String>> {
    // Stub: whitespace only. To be replaced by a quote-aware implementation.
    Ok(input.split_whitespace().map(|s| s.to_string()).collect())
}
