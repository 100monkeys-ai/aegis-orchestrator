// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The SMTP half of the mailbox check (RFC 5321, RFC 3207, RFC 4954):
//! greeting, `EHLO`, optional `STARTTLS` and `EHLO` again, `AUTH`, `QUIT`.
//! No `MAIL FROM`, `RCPT TO` or `DATA` is sent.

use super::wire::{failure, Wire};
use super::{AdmittedTarget, MailConnector, MailProtocol, MailboxCheckFailure};
use crate::domain::credential::{MailSecurity, MailboxSettings};
use crate::domain::secrets::SensitiveString;
use base64::{engine::general_purpose::STANDARD, Engine as _};

/// The name the client gives in `EHLO`.
const EHLO_NAME: &str = "localhost";

/// Run the SMTP session with `settings` and `password`, over the
/// endpoint the connector admitted.
pub async fn check(
    connector: &dyn MailConnector,
    admitted: &AdmittedTarget,
    settings: &MailboxSettings,
    password: &SensitiveString,
) -> Result<(), MailboxCheckFailure> {
    let host = settings.smtp_host.as_str();
    let stream = connector.connect(admitted).await.map_err(|e| {
        failure(
            MailProtocol::Smtp,
            &format!("connect to {host}:{} failed: {e}", settings.smtp_port),
            password,
        )
    })?;
    let mut wire = Wire::new(stream, MailProtocol::Smtp, password);

    expect(&mut wire, 220).await?;
    let mut capabilities = ehlo(&mut wire).await?;

    if settings.smtp_security == MailSecurity::Starttls {
        if !capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("STARTTLS"))
        {
            return Err(wire.fail("the server does not offer STARTTLS"));
        }
        wire.send(b"STARTTLS\r\n").await?;
        expect(&mut wire, 220).await?;
        let stream = wire.into_stream()?;
        let stream = connector.start_tls(stream, host).await.map_err(|e| {
            failure(
                MailProtocol::Smtp,
                &format!("TLS handshake with {host} failed: {e}"),
                password,
            )
        })?;
        wire = Wire::new(stream, MailProtocol::Smtp, password);
        capabilities = ehlo(&mut wire).await?;
    }

    let mechanisms: Vec<String> = capabilities
        .iter()
        .filter_map(|c| {
            let mut words = c.split_whitespace();
            match words.next() {
                Some(w) if w.eq_ignore_ascii_case("AUTH") => {
                    Some(words.map(|m| m.to_ascii_uppercase()).collect::<Vec<_>>())
                }
                _ => None,
            }
        })
        .flatten()
        .collect();

    let username = settings.username.as_str();
    if mechanisms.iter().any(|m| m == "PLAIN") {
        let token = STANDARD.encode(format!("\0{username}\0{}", password.expose()));
        wire.send(format!("AUTH PLAIN {token}\r\n").as_bytes())
            .await?;
        expect(&mut wire, 235).await?;
    } else if mechanisms.iter().any(|m| m == "LOGIN") {
        wire.send(b"AUTH LOGIN\r\n").await?;
        expect(&mut wire, 334).await?;
        wire.send(format!("{}\r\n", STANDARD.encode(username)).as_bytes())
            .await?;
        expect(&mut wire, 334).await?;
        wire.send(format!("{}\r\n", STANDARD.encode(password.expose())).as_bytes())
            .await?;
        expect(&mut wire, 235).await?;
    } else {
        return Err(wire.fail(format!(
            "the server offers neither AUTH PLAIN nor AUTH LOGIN (offered: {})",
            if mechanisms.is_empty() {
                "none".to_string()
            } else {
                mechanisms.join(" ")
            }
        )));
    }

    // Authenticated: the check has passed whatever QUIT answers.
    if wire.send(b"QUIT\r\n").await.is_ok() {
        let _ = reply(&mut wire).await;
    }
    Ok(())
}

/// `EHLO`, answering the capability lines after the first.
async fn ehlo(wire: &mut Wire<'_>) -> Result<Vec<String>, MailboxCheckFailure> {
    wire.send(format!("EHLO {EHLO_NAME}\r\n").as_bytes())
        .await?;
    let (code, lines) = reply(wire).await?;
    if code != 250 {
        return Err(wire.fail(render(code, &lines)));
    }
    Ok(lines.into_iter().skip(1).collect())
}

/// Read one reply and require `code`.
async fn expect(wire: &mut Wire<'_>, code: u16) -> Result<(), MailboxCheckFailure> {
    let (got, lines) = reply(wire).await?;
    if got == code {
        Ok(())
    } else {
        Err(wire.fail(render(got, &lines)))
    }
}

/// One reply, possibly multi-line (`250-…` then `250 …`): its code and the
/// text of each line.
async fn reply(wire: &mut Wire<'_>) -> Result<(u16, Vec<String>), MailboxCheckFailure> {
    let mut lines = Vec::new();
    loop {
        let line = wire.line().await?;
        let code = line
            .get(..3)
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| wire.fail(format!("not an SMTP reply: {line}")))?;
        let more = line.as_bytes().get(3) == Some(&b'-');
        lines.push(line.get(4..).unwrap_or("").to_string());
        if !more {
            return Ok((code, lines));
        }
        if lines.len() > 256 {
            return Err(wire.fail("the server sent a reply of more than 256 lines"));
        }
    }
}

fn render(code: u16, lines: &[String]) -> String {
    format!("{code} {}", lines.join(" / "))
}
