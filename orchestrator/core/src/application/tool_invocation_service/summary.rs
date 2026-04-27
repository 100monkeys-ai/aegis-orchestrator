// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Helpers for deriving the `summary` field surfaced on execution list
//! responses.
//!
//! The summary is sourced from the execution's free-text `intent` (for
//! task executions) or from `input_params.intent` (for workflow
//! executions). It is normalised — whitespace collapsed, capped at 160
//! display chars — so list responses stay greppable without forcing
//! callers to fetch each execution individually.

/// Maximum number of source characters retained from the intent before
/// the truncation marker is appended. The marker itself is NOT counted
/// against this cap — the cap is on source chars.
const SUMMARY_CHAR_CAP: usize = 160;

/// Derive a normalised summary string from an optional `intent`.
///
/// Behaviour:
/// - `None` → `None`.
/// - Trim leading/trailing whitespace; collapse internal runs of any
///   whitespace (spaces, tabs, newlines) to a single space.
/// - Empty after normalisation → `None`.
/// - At most `SUMMARY_CHAR_CAP` source characters are retained. If the
///   normalised string exceeds the cap, it is truncated on a UTF-8
///   codepoint boundary (via `char_indices`) and a trailing `…` marker
///   is appended.
pub(super) fn summarize_intent(intent: &Option<String>) -> Option<String> {
    let raw = intent.as_deref()?;

    let normalised = collapse_whitespace(raw);
    if normalised.is_empty() {
        return None;
    }

    let char_count = normalised.chars().count();
    if char_count <= SUMMARY_CHAR_CAP {
        return Some(normalised);
    }

    // Find the byte offset of the (SUMMARY_CHAR_CAP)-th char so we cut
    // on a codepoint boundary.
    let cut_byte = normalised
        .char_indices()
        .nth(SUMMARY_CHAR_CAP)
        .map(|(idx, _)| idx)
        .unwrap_or(normalised.len());

    let mut truncated = String::with_capacity(cut_byte + '…'.len_utf8());
    truncated.push_str(&normalised[..cut_byte]);
    truncated.push('…');
    Some(truncated)
}

/// Trim and collapse whitespace runs in a single pass.
fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_was_space = false;
    for ch in input.trim().chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                out.push(' ');
                prev_was_space = true;
            }
        } else {
            out.push(ch);
            prev_was_space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_none_when_intent_is_none() {
        assert_eq!(summarize_intent(&None), None);
    }

    #[test]
    fn returns_none_when_intent_is_empty_after_trim() {
        assert_eq!(summarize_intent(&Some("   \t\n  ".to_string())), None);
    }

    #[test]
    fn handles_multibyte_truncation_without_panic() {
        // CJK character that occupies 3 bytes in UTF-8. A naive
        // `[..160]` slice on a string of these would panic mid-codepoint.
        let intent: String = "字".repeat(500);
        let summary = summarize_intent(&Some(intent)).expect("summary present");
        // 160 source chars + the trailing ellipsis.
        assert_eq!(summary.chars().count(), SUMMARY_CHAR_CAP + 1);
        assert!(summary.ends_with('…'));
        // Ensure all source chars are the original CJK char.
        assert!(summary.chars().take(SUMMARY_CHAR_CAP).all(|c| c == '字'));
    }

    #[test]
    fn exact_cap_returns_unchanged_no_ellipsis() {
        let intent: String = "a".repeat(SUMMARY_CHAR_CAP);
        let summary = summarize_intent(&Some(intent.clone())).expect("summary present");
        assert_eq!(summary, intent);
        assert!(!summary.ends_with('…'));
        assert_eq!(summary.chars().count(), SUMMARY_CHAR_CAP);
    }

    #[test]
    fn one_over_cap_appends_ellipsis() {
        let intent: String = "a".repeat(SUMMARY_CHAR_CAP + 1);
        let summary = summarize_intent(&Some(intent)).expect("summary present");
        assert!(summary.ends_with('…'));
        assert_eq!(summary.chars().count(), SUMMARY_CHAR_CAP + 1);
        // The source content is exactly cap-many 'a's followed by '…'.
        assert_eq!(
            summary.chars().filter(|c| *c == 'a').count(),
            SUMMARY_CHAR_CAP
        );
    }
}
