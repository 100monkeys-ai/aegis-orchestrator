// SPDX-License-Identifier: AGPL-3.0-only

//! Log sanitization utilities to prevent sensitive data exposure.
//!
//! All potentially sensitive data (prompts, user inputs, URLs with query params,
//! request/response bodies) MUST be sanitized before logging at levels above DEBUG.

use crate::domain::secrets::redact_url;

/// Redacts credentials from a URL string before it is logged.
///
/// Delegates to [`redact_url`], the one redaction rule shared with
/// [`crate::domain::secrets::SensitiveUrl`]: user info is removed and
/// sensitive query parameter values are replaced by `[REDACTED]`. If the URL
/// cannot be parsed, returns `[unparseable-url]`.
pub fn sanitize_url(raw: &str) -> String {
    redact_url(raw)
}

/// Truncates a string for safe logging, appending an ellipsis if truncated.
///
/// Use this for any user-supplied content that must appear in logs (e.g., error context).
/// Sensitive content (prompts, full inputs) should use `redact()` instead.
pub fn truncate_for_log(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}…[truncated, {} bytes total]", &s[..max_len], s.len())
    }
}

/// Returns a fixed redaction string. Use for values that must never appear in logs
/// at any level above DEBUG (prompts, user inputs, request bodies).
pub fn redact(label: &str) -> String {
    format!("[REDACTED:{label}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_url_redacts_sensitive_params() {
        let url = "https://api.example.com/data?api_key=sk_live_123&format=json";
        let result = sanitize_url(url);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk_live_123"));
        assert!(result.contains("format=json"));
    }

    #[test]
    fn sanitize_url_preserves_safe_urls() {
        let url = "https://example.com/page?q=hello&lang=en";
        let result = sanitize_url(url);
        assert!(result.contains("q=hello"));
        assert!(result.contains("lang=en"));
    }

    #[test]
    fn sanitize_url_handles_no_query() {
        let url = "https://example.com/page";
        let result = sanitize_url(url);
        assert_eq!(result, "https://example.com/page");
    }

    #[test]
    fn sanitize_url_handles_unparseable() {
        assert_eq!(sanitize_url("not a url"), "[unparseable-url]");
    }

    /// Regression test: ensure all known-sensitive query params are stripped from
    /// URL logs. This guards against the leak fixed when `info!` was emitting a
    /// generic message while `debug!` used `sanitize_url` — once `info!` was
    /// switched to log the sanitized URL, every sensitive param the function
    /// claims to redact must actually be redacted.
    #[test]
    fn sanitize_url_strips_all_known_sensitive_params() {
        let url = "https://api.example.com/x?api_key=secret123&token=tok456&access_token=at789&signature=sig000&key=k111&safe=ok";
        let result = sanitize_url(url);
        assert!(
            !result.contains("secret123"),
            "api_key value leaked: {result}"
        );
        assert!(!result.contains("tok456"), "token value leaked: {result}");
        assert!(
            !result.contains("at789"),
            "access_token value leaked: {result}"
        );
        assert!(
            !result.contains("sig000"),
            "signature value leaked: {result}"
        );
        assert!(!result.contains("k111"), "key value leaked: {result}");
        assert!(result.contains("safe=ok"), "safe param dropped: {result}");
    }

    /// Targeted regression test: a URL with `?api_key=secret123` MUST NOT contain
    /// `secret123` in the sanitized output. This is the precise leak vector the
    /// `web_tools.rs` info!/debug! mismatch could have exposed.
    #[test]
    fn sanitize_url_api_key_value_never_leaks() {
        let url = "https://api.example.com/data?api_key=secret123";
        let result = sanitize_url(url);
        assert!(
            !result.contains("secret123"),
            "api_key secret leaked into sanitized output: {result}"
        );
    }

    /// A URL whose user info carries a password must not reach a log line
    /// with the password in it, whether or not it also has a query string.
    #[test]
    fn sanitize_url_removes_user_info() {
        let marker = "Mk7-redaction-marker";
        let mut leaked = Vec::new();
        for url in [
            format!("https://agent:{marker}@example.com/data"),
            format!("https://agent:{marker}@example.com/data?format=json"),
            format!("https://{marker}@example.com/data"),
        ] {
            let result = sanitize_url(&url);
            if result.contains(marker) {
                leaked.push(format!("{url} -> {result}"));
            }
        }
        assert!(
            leaked.is_empty(),
            "user-info credential leaked into sanitized output:\n{}",
            leaked.join("\n")
        );
    }

    #[test]
    fn truncate_for_log_short_string() {
        assert_eq!(truncate_for_log("hello", 10), "hello");
    }

    #[test]
    fn truncate_for_log_long_string() {
        let long = "a".repeat(200);
        let result = truncate_for_log(&long, 50);
        assert!(result.contains("…[truncated"));
        assert!(result.len() < 200);
    }

    #[test]
    fn redact_returns_label() {
        assert_eq!(redact("prompt"), "[REDACTED:prompt]");
    }
}
