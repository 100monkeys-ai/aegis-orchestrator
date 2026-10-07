// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The IMAP half of the mailbox check (RFC 9051, RFC 3501): greeting,
//! optional `STARTTLS`, `LOGIN`, `SELECT INBOX`, `LOGOUT`.

use super::wire::{failure, Wire};
use super::{AdmittedTarget, MailConnector, MailProtocol, MailboxCheckFailure};
use crate::domain::credential::{MailSecurity, MailboxSettings};
use crate::domain::secrets::SensitiveString;

/// Run the IMAP session with `settings` and `password`, over the
/// endpoint the connector admitted.
pub async fn check(
    connector: &dyn MailConnector,
    admitted: &AdmittedTarget,
    settings: &MailboxSettings,
    password: &SensitiveString,
) -> Result<(), MailboxCheckFailure> {
    let mut wire = open(connector, admitted, settings, password).await?;
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
/// by `STARTTLS` when the security is `starttls`, and `LOGIN` unless the
/// server pre-authenticated. Shared by the check and the mail tools'
/// sessions.
pub(super) async fn open<'p>(
    connector: &dyn MailConnector,
    admitted: &AdmittedTarget,
    settings: &MailboxSettings,
    password: &'p SensitiveString,
) -> Result<Wire<'p>, MailboxCheckFailure> {
    let host = settings.imap_host.as_str();
    let stream = connector.connect(admitted).await.map_err(|e| {
        failure(
            MailProtocol::Imap,
            &format!("connect to {host}:{} failed: {e}", settings.imap_port),
            password,
        )
    })?;
    let mut wire = Wire::new(stream, MailProtocol::Imap, password);

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
                password,
            )
        })?;
        wire = Wire::new(stream, MailProtocol::Imap, password);
    }

    if !preauth {
        login(&mut wire, &settings.username, password).await?;
    }
    Ok(wire)
}

/// `A1 LOGIN <user> <password>`, each argument a quoted string, or a
/// literal sent after the server's `+` continuation when it holds a byte a
/// quoted string cannot (CR, LF, NUL or an 8-bit byte).
pub(super) async fn login(
    wire: &mut Wire<'_>,
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
pub(super) async fn expect_ok(wire: &mut Wire<'_>, tag: &str) -> Result<(), MailboxCheckFailure> {
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
pub(super) async fn tagged(wire: &mut Wire<'_>, tag: &str) -> Result<String, MailboxCheckFailure> {
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
