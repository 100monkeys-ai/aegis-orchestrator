// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Line reading and reply sanitising shared by the IMAP and SMTP sessions.

use super::{AdmissionError, BoxedMailStream, CheckFailureKind, MailProtocol, MailboxCheckFailure};
use crate::domain::secrets::SensitiveString;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// The longest line either session reads; a server sending more is refused.
const MAX_LINE: u64 = 16 * 1024;
/// The longest reply text kept for the user, in characters.
pub(super) const MAX_REPLY: usize = 512;

/// One session's buffered stream, with the protocol it speaks and the
/// password to redact from anything the server says.
pub(super) struct Wire<'a> {
    reader: BufReader<BoxedMailStream>,
    protocol: MailProtocol,
    password: &'a SensitiveString,
}

impl<'a> Wire<'a> {
    pub(super) fn new(
        stream: BoxedMailStream,
        protocol: MailProtocol,
        password: &'a SensitiveString,
    ) -> Self {
        Self {
            reader: BufReader::new(stream),
            protocol,
            password,
        }
    }

    /// A failure of this session carrying `reply`, sanitised.
    pub(super) fn fail(&self, reply: impl AsRef<str>) -> MailboxCheckFailure {
        failure(self.protocol, reply.as_ref(), self.password)
    }

    /// Write `bytes` and flush.
    pub(super) async fn send(&mut self, bytes: &[u8]) -> Result<(), MailboxCheckFailure> {
        let stream = self.reader.get_mut();
        stream
            .write_all(bytes)
            .await
            .map_err(|e| failure(self.protocol, &format!("write failed: {e}"), self.password))?;
        stream
            .flush()
            .await
            .map_err(|e| failure(self.protocol, &format!("write failed: {e}"), self.password))
    }

    /// Read one line, without its line ending. A closed connection or an
    /// over-long line is a failure.
    pub(super) async fn line(&mut self) -> Result<String, MailboxCheckFailure> {
        let mut buf = Vec::new();
        let n = (&mut self.reader)
            .take(MAX_LINE)
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| self.fail(format!("read failed: {e}")))?;
        if n == 0 {
            return Err(self.fail("the server closed the connection"));
        }
        if !buf.ends_with(b"\n") {
            return Err(self.fail("the server sent a line longer than 16 KiB"));
        }
        let text = String::from_utf8_lossy(&buf);
        Ok(text.trim_end_matches(['\r', '\n']).to_string())
    }

    /// Discard `n` bytes of an IMAP literal the server sent.
    pub(super) async fn skip(&mut self, n: u64) -> Result<(), MailboxCheckFailure> {
        if n > 1024 * 1024 {
            return Err(self.fail("the server sent a literal larger than 1 MiB"));
        }
        let mut sink = tokio::io::sink();
        let copied = tokio::io::copy(&mut (&mut self.reader).take(n), &mut sink)
            .await
            .map_err(|e| self.fail(format!("read failed: {e}")))?;
        if copied != n {
            return Err(self.fail("the server closed the connection"));
        }
        Ok(())
    }

    /// Read the `n` bytes of an IMAP literal the server sent, refusing one
    /// larger than `max`.
    pub(super) async fn literal(
        &mut self,
        n: u64,
        max: u64,
    ) -> Result<Vec<u8>, MailboxCheckFailure> {
        if n > max {
            return Err(self.fail(format!("the server sent a literal larger than {max} bytes")));
        }
        let mut buf = Vec::with_capacity(n as usize);
        let read = (&mut self.reader)
            .take(n)
            .read_to_end(&mut buf)
            .await
            .map_err(|e| self.fail(format!("read failed: {e}")))?;
        if read as u64 != n {
            return Err(self.fail("the server closed the connection"));
        }
        Ok(buf)
    }

    /// The stream back, for a STARTTLS upgrade. Refused when the server sent
    /// bytes after accepting STARTTLS and before the handshake, which would
    /// otherwise be read as if they had come over TLS (response injection).
    pub(super) fn into_stream(self) -> Result<BoxedMailStream, MailboxCheckFailure> {
        if !self.reader.buffer().is_empty() {
            return Err(failure(
                self.protocol,
                "the server sent data after accepting STARTTLS and before the TLS handshake",
                self.password,
            ));
        }
        Ok(self.reader.into_inner())
    }
}

/// A sanitised failure: control characters removed, the password redacted,
/// at most [`MAX_REPLY`] characters.
pub(super) fn failure(
    protocol: MailProtocol,
    reply: &str,
    password: &SensitiveString,
) -> MailboxCheckFailure {
    MailboxCheckFailure {
        protocol,
        reply: sanitise(reply, password),
        kind: CheckFailureKind::Unreachable,
    }
}

/// The failure for an endpoint the connector did not admit.
pub(super) fn failure_of(
    protocol: MailProtocol,
    error: AdmissionError,
    password: &SensitiveString,
) -> MailboxCheckFailure {
    match error {
        AdmissionError::NotAllowed { field, reason } => MailboxCheckFailure {
            protocol,
            reply: sanitise(&reason, password),
            kind: CheckFailureKind::HostNotAllowed { field },
        },
        AdmissionError::Unresolvable(reason) => failure(protocol, &reason, password),
    }
}

fn sanitise(reply: &str, password: &SensitiveString) -> String {
    let mut text: String = reply.chars().filter(|c| !c.is_control()).collect();
    if !password.is_empty() {
        text = text.replace(password.expose(), "[REDACTED]");
    }
    text.trim().chars().take(MAX_REPLY).collect()
}
