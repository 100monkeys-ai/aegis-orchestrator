// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Strict parser for OpenBao Transit signature outputs.
//!
//! Vault Transit returns signatures formatted as `vault:v<digits>:<base64sig>`.
//! Earlier code used a permissive `rsplit_once(':')` fallthrough that, on a
//! malformed response, silently returned the entire raw string and proceeded
//! to base64-decode garbage. This module enforces the documented format and
//! returns a typed error otherwise.
//!
//! Per ADR-117 audit pass 3 (SEV-2-C / Copilot B2).
//!
//! Errors deliberately omit the signature payload itself to avoid log leakage;
//! we surface only the version-prefix shape that was observed.
//!
//! Example success cases: `vault:v1:abc==`, `vault:v42:zzz`.
//! Example failure cases: `justbase64`, `vault::abc`, `vault:notv:abc`,
//! `vault:v:abc`, `vault:v1:` (empty sig).
//!
//! The function returns a borrowed slice into the input so callers can avoid
//! an allocation when feasible.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransitParseError {
    #[error(
        "unexpected transit_sign format; expected 'vault:v<version>:<base64sig>', got prefix={prefix:?}"
    )]
    Malformed {
        /// Version prefix that was observed (or `None` if absent). The
        /// signature payload is intentionally omitted to avoid log leakage.
        prefix: Option<String>,
    },
}

/// Parse a Vault Transit signature envelope of the form
/// `vault:v<digits>:<base64sig>` and return the signature segment.
pub fn parse_vault_signature(raw: &str) -> Result<&str, TransitParseError> {
    let mut parts = raw.splitn(3, ':');
    let prefix = parts.next();
    let version = parts.next();
    let sig = parts.next();
    match (prefix, version, sig) {
        (Some("vault"), Some(v), Some(s))
            if !s.is_empty()
                && v.starts_with('v')
                && v.len() > 1
                && v[1..].chars().all(|ch| ch.is_ascii_digit()) =>
        {
            Ok(s)
        }
        _ => Err(TransitParseError::Malformed {
            prefix: version.map(|v| format!("vault:{v}")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_v1() {
        assert_eq!(parse_vault_signature("vault:v1:abc==").unwrap(), "abc==");
    }

    #[test]
    fn parses_multi_digit_version() {
        assert_eq!(parse_vault_signature("vault:v42:zzz").unwrap(), "zzz");
    }

    #[test]
    fn rejects_no_separator() {
        assert!(matches!(
            parse_vault_signature("justbase64"),
            Err(TransitParseError::Malformed { .. })
        ));
    }

    #[test]
    fn rejects_empty_version() {
        assert!(parse_vault_signature("vault::abc").is_err());
    }

    #[test]
    fn rejects_non_v_version() {
        assert!(parse_vault_signature("vault:notv:abc").is_err());
    }

    #[test]
    fn rejects_empty_signature() {
        assert!(parse_vault_signature("vault:v1:").is_err());
    }

    #[test]
    fn rejects_v_only() {
        assert!(parse_vault_signature("vault:v:abc").is_err());
    }

    #[test]
    fn signature_with_colons_preserved() {
        // splitn(3) means the third segment retains any further colons.
        assert_eq!(
            parse_vault_signature("vault:v1:abc:def").unwrap(),
            "abc:def"
        );
    }
}
