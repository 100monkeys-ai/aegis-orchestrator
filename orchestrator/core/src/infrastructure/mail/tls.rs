// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The production [`MailConnector`].
//!
//! Admission applies the [`guard`]: the mail ports only; the
//! host resolved once (or read as an IP literal) and refused when any
//! address is not public unicast. Connection dials only the addresses that
//! were admitted, in order, and wraps the stream in TLS by rustls (the
//! `ring` provider, TLS 1.2 and 1.3) verified against the Mozilla root store
//! that `webpki-roots` carries, so the check does not depend on the image's
//! certificate bundle and links no libssl. The certificate is verified for
//! the host name the user gave, not for the address.

use super::guard;
use super::{
    AdmissionError, AdmittedTarget, BoxedMailStream, MailConnector, MailDialer, MailResolver,
    MailTarget,
};
use crate::domain::credential::MailSecurity;
use async_trait::async_trait;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// [`MailResolver`] over the system resolver (`getaddrinfo`).
pub struct SystemResolver;

#[async_trait]
impl MailResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}

/// [`MailDialer`] opening a TCP connection, bounded by a connect timeout.
pub struct TcpDialer;

#[async_trait]
impl MailDialer for TcpDialer {
    async fn dial(&self, addr: SocketAddr) -> io::Result<BoxedMailStream> {
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
        Ok(Box::new(tcp))
    }
}

pub struct RustlsMailConnector {
    tls: TlsConnector,
    resolver: Arc<dyn MailResolver>,
    dialer: Arc<dyn MailDialer>,
}

impl RustlsMailConnector {
    /// The connector the daemon uses: the system resolver and TCP.
    pub fn new() -> Self {
        Self::with_network(Arc::new(SystemResolver), Arc::new(TcpDialer))
    }

    /// The production connector over another resolver and dialer; the guard
    /// and TLS are the same. Tests record resolutions and dials with it.
    pub fn with_network(resolver: Arc<dyn MailResolver>, dialer: Arc<dyn MailDialer>) -> Self {
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
            resolver,
            dialer,
        }
    }

    async fn handshake<S>(&self, stream: S, host: &str) -> io::Result<BoxedMailStream>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let name = match guard::ip_literal(host) {
            Some(ip) => ServerName::IpAddress(ip.into()),
            None => ServerName::try_from(host.to_string())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
        };
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
    async fn admit(&self, target: MailTarget) -> Result<AdmittedTarget, AdmissionError> {
        let protocol = target.protocol;
        guard::check_port(protocol, target.port).map_err(|reason| AdmissionError::NotAllowed {
            field: guard::field(protocol, false),
            reason,
        })?;
        let addrs: Vec<SocketAddr> = match guard::ip_literal(&target.host) {
            Some(ip) => vec![SocketAddr::new(ip, target.port)],
            None => self
                .resolver
                .resolve(target.host.trim(), target.port)
                .await
                .map_err(|e| {
                    AdmissionError::Unresolvable(format!(
                        "{} {} could not be resolved: {e}",
                        guard::field(protocol, true),
                        target.host
                    ))
                })?,
        };
        if addrs.is_empty() {
            return Err(AdmissionError::Unresolvable(format!(
                "{} {} resolves to no address",
                guard::field(protocol, true),
                target.host
            )));
        }
        let ips: Vec<IpAddr> = addrs.iter().map(SocketAddr::ip).collect();
        guard::check_addresses(protocol, target.host.trim(), &ips).map_err(|reason| {
            AdmissionError::NotAllowed {
                field: guard::field(protocol, true),
                reason,
            }
        })?;
        Ok(AdmittedTarget { target, addrs })
    }

    async fn connect(&self, admitted: &AdmittedTarget) -> io::Result<BoxedMailStream> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, "no admitted address");
        for addr in &admitted.addrs {
            match self.dialer.dial(*addr).await {
                Ok(stream) => {
                    return match admitted.target.security {
                        MailSecurity::Tls => self.handshake(stream, &admitted.target.host).await,
                        MailSecurity::Starttls => Ok(stream),
                    };
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    async fn start_tls(&self, stream: BoxedMailStream, host: &str) -> io::Result<BoxedMailStream> {
        self.handshake(stream, host).await
    }
}
