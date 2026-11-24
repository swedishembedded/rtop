// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Neutralising untrusted text before it reaches a terminal or an export.
//!
//! Process names, command lines, mount paths and kernel log lines are
//! controlled by other users. Printing them raw would let any process inject
//! terminal escape sequences into rtop's screen. Every such string passes
//! through [`sanitize`], which makes control characters visible instead of
//! interpreting them.

use std::borrow::Cow;
use std::fmt::Write;

fn is_unsafe(c: char) -> bool {
    // C0 controls, DEL, C1 controls, and bidi overrides that can visually
    // reorder text (Trojan-Source style spoofing).
    c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Replaces control and bidi-override characters with `\xNN` / `\u{..}`
/// escapes. Returns the input unchanged (borrowed) when it is already safe.
pub fn sanitize(s: &str) -> Cow<'_, str> {
    if !s.chars().any(is_unsafe) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if is_unsafe(c) {
            if (c as u32) < 0x100 {
                let _ = write!(out, "\\x{:02x}", c as u32);
            } else {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Decodes kernel bytes (which need not be UTF-8) and sanitizes them.
pub fn sanitize_bytes(b: &[u8]) -> String {
    sanitize(&String::from_utf8_lossy(b)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn escape_sequences_are_neutralised() {
        assert_eq!(sanitize("evil\x1b[2Jname"), "evil\\x1b[2Jname");
        assert_eq!(sanitize("a\nb"), "a\\x0ab");
        assert_eq!(sanitize("\u{202e}txt"), "\\u{202e}txt");
        assert!(matches!(sanitize("plain chrome"), Cow::Borrowed(_)));
    }

    proptest! {
        #[test]
        fn output_never_contains_control_characters(s in any::<String>()) {
            prop_assert!(!sanitize(&s).chars().any(is_unsafe));
        }

        #[test]
        fn arbitrary_bytes_never_panic(b in proptest::collection::vec(any::<u8>(), 0..256)) {
            let out = sanitize_bytes(&b);
            prop_assert!(!out.chars().any(is_unsafe));
        }
    }
}
