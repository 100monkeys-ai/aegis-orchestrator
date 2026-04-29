// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Log-injection sanitization helpers.
//!
//! Untrusted strings (Stripe webhook metadata values, request headers,
//! anything reaching the daemon from an external client) MUST be sanitized
//! before being interpolated into a structured log line. Otherwise a hostile
//! value containing `\r\n` can fabricate a forged log entry that downstream
//! log aggregators will index as if it were emitted by the daemon.
//!
//! See security audit 002 finding 4.37.16.

/// Replace newline and other control characters with the Unicode "symbol for
/// newline" (`U+2424`) so a hostile value cannot terminate the current log
/// record and inject a forged second record.
///
/// Preserves printable ASCII and non-control Unicode characters unchanged.
/// Specifically replaces:
///
/// - `\r`, `\n` — line terminators (the primary log-injection vector)
/// - all C0 control characters (`U+0000`..=`U+001F`) except tab
/// - `U+007F` DEL
/// - C1 control characters (`U+0080`..=`U+009F`)
///
/// Tab (`U+0009`) is left intact because structured-log formatters treat it
/// as ordinary whitespace and it is harmless inside a field value.
pub fn sanitize_for_log(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '\t' => '\t',
            '\u{0000}'..='\u{001F}' | '\u{007F}'..='\u{009F}' => '\u{2424}',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_lf() {
        let out = sanitize_for_log("tenant_a\nFAKE LOG ENTRY");
        assert!(!out.contains('\n'), "LF must be replaced: {out:?}");
        assert!(out.contains("tenant_a"));
        assert!(out.contains("FAKE LOG ENTRY"));
    }

    #[test]
    fn sanitize_strips_crlf() {
        let out = sanitize_for_log("tenant_a\r\nFAKE");
        assert!(!out.contains('\r'));
        assert!(!out.contains('\n'));
    }

    #[test]
    fn sanitize_strips_other_c0_controls() {
        // ESC (0x1B) — used in ANSI escape sequences that could re-colour
        // log output and confuse log scrapers.
        let out = sanitize_for_log("a\x1b[31mEVIL\x1b[0mb");
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn sanitize_strips_del() {
        let out = sanitize_for_log("a\x7fb");
        assert!(!out.contains('\x7f'));
    }

    #[test]
    fn sanitize_preserves_tab() {
        let out = sanitize_for_log("a\tb");
        assert_eq!(out, "a\tb");
    }

    #[test]
    fn sanitize_preserves_plain_ascii_and_unicode() {
        let out = sanitize_for_log("tenant_abc-123 \u{1f600}");
        assert_eq!(out, "tenant_abc-123 \u{1f600}");
    }

    #[test]
    fn sanitize_replacement_is_visible_marker() {
        // The replacement symbol U+2424 is the canonical Unicode "symbol
        // for newline" — visible in logs so an analyst can see that
        // sanitization fired.
        let out = sanitize_for_log("\n");
        assert_eq!(out, "\u{2424}");
    }
}
