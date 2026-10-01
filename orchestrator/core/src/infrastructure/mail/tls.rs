// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The production [`MailConnector`]: TCP, and TLS by rustls (the `ring`
//! provider, TLS 1.2 and 1.3) verified against the Mozilla root store that
//! `webpki-roots` carries, so the check does not depend on the image's
//! certificate bundle and links no libssl.

use super::{BoxedMailStream, MailConnector};
use crate::domain::credential::MailSecurity;
use async_trait::async_trait;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct RustlsMailConnector {
    tls: TlsConnector,
}

impl RustlsMailConnector {
    pub fn new() -> Self {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Self {
            tls: TlsConnector::from(Arc::new(config)),
        }
    }

    async fn handshake<S>(&self, stream: S, host: &str) -> io::Result<BoxedMailStream>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let name = ServerName::try_from(host.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let stream = self.tls.connect(name, stream).await?;
        Ok(Box::new(stream))
    }
}

impl Default for RustlsMailConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MailConnector for RustlsMailConnector {
    async fn connect(
        &self,
        host: &str,
        port: u16,
        security: MailSecurity,
    ) -> io::Result<BoxedMailStream> {
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
        match security {
            MailSecurity::Tls => self.handshake(tcp, host).await,
            MailSecurity::Starttls => Ok(Box::new(tcp)),
        }
    }

    async fn start_tls(&self, stream: BoxedMailStream, host: &str) -> io::Result<BoxedMailStream> {
        self.handshake(stream, host).await
    }
}
