// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! SMTP submission for the outbound mail tools (RFC 5321, RFC 6409; AEGIS
//! ADR-125 D4, its Update of 2026-10-07 (3) clause 14).
//!
//! The session is opened as the mailbox check opens it ([`super::smtp`]):
//! the endpoint is admitted by the connector (the production connector
//! applies the [`super::guard`]), then greeting, `EHLO`, `STARTTLS` when
//! asked, and `AUTH XOAUTH2` for an OAuth mailbox, else `AUTH PLAIN`, or
//! `AUTH LOGIN` when PLAIN is not offered. Then `MAIL FROM`, one `RCPT TO`
//! per recipient, `DATA` with the message dot-stuffed, and `QUIT`.
//!
//! A recipient the server refuses sends nothing: the client answers with
//! `RSET` and `QUIT` before any `DATA`, and the failure names the address.
//! Every failure passes through the redacting wire, so no form of the
//! secret reaches it.

use super::smtp::{expect, open, quit, render, reply};
use super::wire::failure_of;
use super::{MailAuth, MailConnector, MailProtocol, MailTarget, MailboxCheckFailure};
use crate::domain::credential::MailboxSettings;

/// Submit `message` from `from` to `recipients` through the mailbox's SMTP
/// server, authenticating by `auth`.
pub async fn submit(
    connector: &dyn MailConnector,
    settings: &MailboxSettings,
    auth: &MailAuth,
    from: &str,
    recipients: &[String],
    message: &[u8],
) -> Result<(), MailboxCheckFailure> {
    let admitted = connector
        .admit(MailTarget {
            protocol: MailProtocol::Smtp,
            host: settings.smtp_host.clone(),
            port: settings.smtp_port,
            security: settings.smtp_security.clone(),
        })
        .await
        .map_err(|e| {
            failure_of(
                MailProtocol::Smtp,
                e,
                &auth.secret_forms(&settings.username),
            )
        })?;
    let mut wire = open(connector, &admitted, settings, auth).await?;

    wire.send(format!("MAIL FROM:<{from}>\r\n").as_bytes())
        .await?;
    expect(&mut wire, 250).await?;
    for recipient in recipients {
        wire.send(format!("RCPT TO:<{recipient}>\r\n").as_bytes())
            .await?;
        let (code, lines) = reply(&mut wire).await?;
        if code != 250 && code != 251 {
            // Nothing has been sent: drop the envelope and leave.
            if wire.send(b"RSET\r\n").await.is_ok() {
                let _ = reply(&mut wire).await;
            }
            quit(&mut wire).await;
            return Err(wire.fail(format!(
                "the server refused the recipient {recipient}, so nothing was sent: {}",
                render(code, &lines)
            )));
        }
    }
    wire.send(b"DATA\r\n").await?;
    expect(&mut wire, 354).await?;
    wire.send(&dot_stuffed(message)).await?;
    expect(&mut wire, 250).await?;
    quit(&mut wire).await;
    Ok(())
}

/// `message` as `DATA` carries it: CRLF line endings, each line beginning
/// with `.` given a second one, ended by `.` on a line of its own. It works
/// on bytes, so a forwarded message's bytes that are not UTF-8 go out as
/// they are.
fn dot_stuffed(message: &[u8]) -> Vec<u8> {
    let mut lines: Vec<&[u8]> = message
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut out = Vec::with_capacity(message.len() + 8);
    for line in lines {
        if line.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_beginning_with_a_dot_is_stuffed_and_the_data_ends_with_a_lone_dot() {
        assert_eq!(
            dot_stuffed(b"Subject: x\r\n\r\n.\r\n..two\r\nend\r\n"),
            b"Subject: x\r\n\r\n..\r\n...two\r\nend\r\n.\r\n".to_vec()
        );
    }

    /// A forwarded message's bytes go out as they are: a byte that is not
    /// UTF-8 (0xE9, `é` in Latin-1) is carried through unchanged, beside a
    /// line beginning with a dot.
    #[test]
    fn a_byte_that_is_not_utf8_is_carried_through_unchanged() {
        assert_eq!(
            dot_stuffed(b"Subject: caf\xe9\r\n\r\n.caf\xe9\r\n\xe9t\xe9\r\n"),
            b"Subject: caf\xe9\r\n\r\n..caf\xe9\r\n\xe9t\xe9\r\n.\r\n".to_vec(),
            "the 0xE9 bytes did not go out unchanged"
        );
    }
}
