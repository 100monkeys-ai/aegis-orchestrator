// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The IMAP half of the mailbox check (RFC 9051, RFC 3501): greeting,
//! optional `STARTTLS`, `LOGIN` (or SASL `AUTHENTICATE XOAUTH2` for an
//! OAuth mailbox), `SELECT INBOX`, `LOGOUT`.

use super::wire::{failure, Wire};
use super::{AdmittedTarget, MailAuth, MailConnector, MailProtocol, MailboxCheckFailure};
use crate::domain::credential::{MailSecurity, MailboxSettings};
use crate::domain::secrets::SensitiveString;
use base64::{engine::general_purpose::STANDARD, Engine as _};

/// Run the IMAP session with `settings`, authenticating by `auth`, over
/// the endpoint the connector admitted.
pub async fn check(
    connector: &dyn MailConnector,
    admitted: &AdmittedTarget,
    settings: &MailboxSettings,
    auth: &MailAuth,
) -> Result<(), MailboxCheckFailure> {
    let mut wire = open(connector, admitted, settings, auth).await?;
    wire.send(b"A2 SELECT INBOX\r\n").await?;
    expect_ok(&mut wire, "A2").await?;
    // The check has passed; a server that answers LOGOUT badly does not
    // undo that.
    if wire.send(b"A3 LOGOUT\r\n").await.is_ok() {
        let _ = tagged(&mut wire, "A3").await;
    }
    Ok(())
}

/// Open an authenticated IMAP session: connect, read the greeting, upgrade
/// by `STARTTLS` when the security is `starttls`, and authenticate unless
/// the server pre-authenticated: `LOGIN` with a password, `AUTHENTICATE
/// XOAUTH2` with a token. Shared by the check and the mail tools' sessions.
pub(super) async fn open(
    connector: &dyn MailConnector,
    admitted: &AdmittedTarget,
    settings: &MailboxSettings,
    auth: &MailAuth,
) -> Result<Wire, MailboxCheckFailure> {
    let host = settings.imap_host.as_str();
    let secrets = auth.secret_forms(&settings.username);
    let stream = connector.connect(admitted).await.map_err(|e| {
        failure(
            MailProtocol::Imap,
            &format!("connect to {host}:{} failed: {e}", settings.imap_port),
            &secrets,
        )
    })?;
    let mut wire = Wire::new(stream, MailProtocol::Imap, secrets.clone());

    let greeting = wire.line().await?;
    let preauth = greeting.starts_with("* PREAUTH");
    if !(greeting.starts_with("* OK") || preauth) {
        return Err(wire.fail(greeting));
    }

    if settings.imap_security == MailSecurity::Starttls {
        if preauth {
            // Authenticated before TLS: the session cannot be secured.
            return Err(wire.fail(format!(
                "the server pre-authenticated the plaintext session: {greeting}"
            )));
        }
        wire.send(b"A0 STARTTLS\r\n").await?;
        expect_ok(&mut wire, "A0").await?;
        let stream = wire.into_stream()?;
        let stream = connector.start_tls(stream, host).await.map_err(|e| {
            failure(
                MailProtocol::Imap,
                &format!("TLS handshake with {host} failed: {e}"),
                &secrets,
            )
        })?;
        wire = Wire::new(stream, MailProtocol::Imap, secrets);
    }

    if !preauth {
        match auth {
            MailAuth::Password(password) => login(&mut wire, &settings.username, password).await?,
            MailAuth::XOAuth2(_) => {
                let response = auth
                    .xoauth2_response(&settings.username)
                    .expect("an XOAUTH2 auth has a response");
                authenticate_xoauth2(&mut wire, &response).await?
            }
        }
    }
    Ok(wire)
}

/// SASL `XOAUTH2` (Google's XOAUTH2 protocol, RFC 3501 `AUTHENTICATE`):
/// `A1 AUTHENTICATE XOAUTH2`, then, after the server's `+`, the base64
/// response on its own line, so no `SASL-IR` capability is needed. A server
/// that refuses the token sends a `+` challenge holding a base64 JSON
/// status; the client answers it with an empty line and the server ends
/// the exchange with a tagged `NO`. The failure carries that reply and the
/// decoded challenge (AEGIS ADR-125's Update of 2026-10-07 clause 10, 8c).
pub(super) async fn authenticate_xoauth2(
    wire: &mut Wire,
    response: &SensitiveString,
) -> Result<(), MailboxCheckFailure> {
    wire.send(b"A1 AUTHENTICATE XOAUTH2\r\n").await?;
    let ready = wire.line().await?;
    if !ready.starts_with('+') {
        return Err(wire.fail(strip_tag(&ready, "A1")));
    }
    let mut line = response.expose().as_bytes().to_vec();
    line.extend_from_slice(b"\r\n");
    wire.send(&line).await?;
    let next = wire.line().await?;
    let status_line = if let Some(challenge) = next.strip_prefix('+') {
        let challenge = decode_challenge(challenge.trim());
        wire.send(b"\r\n").await?;
        let refused = tagged(wire, "A1").await?;
        return Err(wire.fail(format!("{} {challenge}", strip_tag(&refused, "A1"))));
    } else if next.starts_with("A1 ") {
        next
    } else if next.starts_with("* BYE") {
        return Err(wire.fail(next));
    } else {
        tagged(wire, "A1").await?
    };
    let status = strip_tag(&status_line, "A1");
    if status.len() >= 2 && status[..2].eq_ignore_ascii_case("OK") {
        Ok(())
    } else {
        Err(wire.fail(status))
    }
}

/// A SASL challenge as text: its base64 decoded when it is base64 (Google
/// sends a JSON status), else as sent.
pub(super) fn decode_challenge(challenge: &str) -> String {
    match STANDARD.decode(challenge) {
        Ok(bytes) if !challenge.is_empty() => String::from_utf8_lossy(&bytes).to_string(),
        _ => challenge.to_string(),
    }
}

/// `A1 LOGIN <user> <password>`, each argument a quoted string, or a
/// literal sent after the server's `+` continuation when it holds a byte a
/// quoted string cannot (CR, LF, NUL or an 8-bit byte).
pub(super) async fn login(
    wire: &mut Wire,
    username: &str,
    password: &SensitiveString,
) -> Result<(), MailboxCheckFailure> {
    let mut pending = b"A1 LOGIN ".to_vec();
    for (i, arg) in [username, password.expose()].into_iter().enumerate() {
        if i == 1 {
            pending.push(b' ');
        }
        if quotable(arg) {
            pending.push(b'"');
            for b in arg.bytes() {
                if b == b'"' || b == b'\\' {
                    pending.push(b'\\');
                }
                pending.push(b);
            }
            pending.push(b'"');
        } else {
            pending.extend_from_slice(format!("{{{}}}\r\n", arg.len()).as_bytes());
            wire.send(&pending).await?;
            pending.clear();
            let next = wire.line().await?;
            if !next.starts_with('+') {
                return Err(wire.fail(strip_tag(&next, "A1")));
            }
            pending.extend_from_slice(arg.as_bytes());
        }
    }
    pending.extend_from_slice(b"\r\n");
    wire.send(&pending).await?;
    expect_ok(wire, "A1").await
}

fn quotable(s: &str) -> bool {
    s.bytes()
        .all(|b| (0x01..=0x7f).contains(&b) && b != b'\r' && b != b'\n')
}

/// Read to the tagged response for `tag` and require `OK`.
pub(super) async fn expect_ok(wire: &mut Wire, tag: &str) -> Result<(), MailboxCheckFailure> {
    let line = tagged(wire, tag).await?;
    let status = strip_tag(&line, tag);
    if status.len() >= 2 && status[..2].eq_ignore_ascii_case("OK") {
        Ok(())
    } else {
        Err(wire.fail(status))
    }
}

/// Read lines until the one tagged `tag`, skipping untagged responses and
/// any literal they carry. A `* BYE` before it is the server's refusal.
pub(super) async fn tagged(wire: &mut Wire, tag: &str) -> Result<String, MailboxCheckFailure> {
    loop {
        let line = wire.line().await?;
        if line.starts_with(&format!("{tag} ")) {
            return Ok(line);
        }
        if line.starts_with("* BYE") {
            return Err(wire.fail(line));
        }
        if let Some(n) = trailing_literal(&line) {
            wire.skip(n).await?;
        }
    }
}

pub(super) fn strip_tag<'l>(line: &'l str, tag: &str) -> &'l str {
    line.strip_prefix(tag).map(str::trim_start).unwrap_or(line)
}

/// The size of a `{n}` literal announced at the end of `line`.
pub(super) fn trailing_literal(line: &str) -> Option<u64> {
    let body = line.strip_suffix('}')?;
    let open = body.rfind('{')?;
    body[open + 1..].trim_end_matches('+').parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_with_a_quote_or_backslash_is_quoted_and_one_with_8_bit_bytes_is_a_literal() {
        assert!(quotable(r#"pa"ss\word"#));
        assert!(!quotable("pässword"));
        assert!(!quotable("line\r\nbreak"));
    }

    #[test]
    fn a_trailing_literal_is_read() {
        assert_eq!(trailing_literal("* 1 FETCH (BODY[] {12}"), Some(12));
        assert_eq!(trailing_literal("* OK done"), None);
    }
}
