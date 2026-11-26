// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Mutual TLS between cluster nodes (`spec.cluster.tls`)
//!
//! When `spec.cluster.tls` is present and enabled, the cluster gRPC server
//! presents the node's certificate and requires a client certificate signed
//! by the cluster CA, and every node-to-node client (peer probes, the
//! controller client, remote storage) presents the same certificate and
//! verifies the server against the same CA. Each node's certificate names
//! the address its peers dial (its private-network IP) as a subject
//! alternative name.
//!
//! mTLS authenticates the channel; the SEAL node envelope still
//! authenticates every request on it (see `node_envelope`).

use anyhow::{bail, Context, Result};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, ServerTlsConfig};

use crate::domain::node_config::ClusterTlsConfig;

/// The node's certificate, key, and the cluster CA, read once at start.
#[derive(Clone)]
pub struct ClusterTls {
    identity: Identity,
    ca: Certificate,
}

impl ClusterTls {
    /// `None` when the block is absent or disabled: the cluster then speaks
    /// plaintext, which the config validator allows only on a loopback bind
    /// or with `allow_insecure_bind`.
    pub fn load(config: Option<&ClusterTlsConfig>) -> Result<Option<Self>> {
        let Some(config) = config.filter(|c| c.enabled) else {
            return Ok(None);
        };
        // ring and aws-lc are both in the build, so rustls cannot pick a
        // process-wide provider by itself; tonic is built with `tls-ring`.
        // Losing the race to another installer is fine: one is installed.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let read = |path: &str, what: &str| {
            std::fs::read(path).with_context(|| format!("reading cluster TLS {what} at {path}"))
        };
        Ok(Some(Self {
            identity: Identity::from_pem(
                read(&config.cert_path, "certificate")?,
                read(&config.key_path, "private key")?,
            ),
            ca: Certificate::from_pem(read(&config.ca_cert, "CA certificate")?),
        }))
    }

    /// The server side: present this node's certificate, require a client
    /// certificate the cluster CA signed.
    pub fn server(&self) -> ServerTlsConfig {
        ServerTlsConfig::new()
            .identity(self.identity.clone())
            .client_ca_root(self.ca.clone())
    }

    /// The client side: present this node's certificate, trust only the
    /// cluster CA.
    pub fn client(&self) -> ClientTlsConfig {
        ClientTlsConfig::new()
            .identity(self.identity.clone())
            .ca_certificate(self.ca.clone())
    }
}

/// The URI to dial `address` (`host:port`, or a full URI) at: `https://`
/// under TLS, `http://` without. A plaintext URI under TLS is refused rather
/// than dialled in the clear.
pub fn endpoint_uri(address: &str, tls: Option<&ClusterTls>) -> Result<String> {
    if let Some(rest) = address.strip_prefix("http://") {
        if tls.is_some() {
            bail!("cluster address http://{rest} is plaintext but spec.cluster.tls is enabled; use https://");
        }
        return Ok(address.to_string());
    }
    if address.contains("://") {
        return Ok(address.to_string());
    }
    let scheme = if tls.is_some() { "https" } else { "http" };
    Ok(format!("{scheme}://{address}"))
}

/// A tonic endpoint for a peer at `address`, presenting this node's
/// certificate under TLS.
pub fn endpoint(address: &str, tls: Option<&ClusterTls>) -> Result<Endpoint> {
    let endpoint =
        Endpoint::from_shared(endpoint_uri(address, tls)?).context("invalid cluster address")?;
    match tls {
        Some(tls) => endpoint
            .tls_config(tls.client())
            .context("configuring cluster TLS"),
        None => Ok(endpoint),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair,
    };
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server;
    use tonic_health::pb::{health_client::HealthClient, HealthCheckRequest};

    use super::*;

    fn cluster_ca() -> CertifiedIssuer<'static, KeyPair> {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
    }

    /// A node certificate for 127.0.0.1 signed by `signer`, trusting `trust`,
    /// written under `dir` as `name`.
    fn node(
        dir: &Path,
        name: &str,
        signer: &CertifiedIssuer<'static, KeyPair>,
        trust: &CertifiedIssuer<'static, KeyPair>,
    ) -> ClusterTls {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let cert = params.signed_by(&key, signer).unwrap();
        let path = |what: &str| dir.join(format!("{name}-{what}.pem"));
        std::fs::write(path("cert"), cert.pem()).unwrap();
        std::fs::write(path("key"), key.serialize_pem()).unwrap();
        std::fs::write(path("ca"), trust.pem()).unwrap();
        let config = ClusterTlsConfig {
            enabled: true,
            cert_path: path("cert").display().to_string(),
            key_path: path("key").display().to_string(),
            ca_cert: path("ca").display().to_string(),
        };
        ClusterTls::load(Some(&config)).unwrap().unwrap()
    }

    /// Serve the gRPC health service under `tls`; returns `127.0.0.1:port`.
    async fn serve(tls: &ClusterTls) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (_reporter, health) = tonic_health::server::health_reporter();
        let router = Server::builder()
            .tls_config(tls.server())
            .unwrap()
            .add_service(health);
        tokio::spawn(router.serve_with_incoming(TcpListenerStream::new(listener)));
        address
    }

    async fn health_check(address: &str, tls: Option<&ClusterTls>) -> Result<()> {
        let channel = endpoint(address, tls)?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await?;
        HealthClient::new(channel)
            .check(HealthCheckRequest {
                service: String::new(),
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn only_a_peer_the_cluster_ca_signed_gets_through() {
        let dir = tempfile::tempdir().unwrap();
        let ca = cluster_ca();
        let server = node(dir.path(), "server", &ca, &ca);
        let address = serve(&server).await;

        let peer = node(dir.path(), "peer", &ca, &ca);
        health_check(&address, Some(&peer))
            .await
            .expect("a peer with a cluster certificate is served");

        let stranger_ca = cluster_ca();
        let stranger = node(dir.path(), "stranger", &stranger_ca, &ca);
        assert!(
            health_check(&address, Some(&stranger)).await.is_err(),
            "a client certificate from another CA is refused"
        );

        assert!(
            health_check(&address, None).await.is_err(),
            "a plaintext client is refused"
        );

        let misled = node(dir.path(), "misled", &ca, &stranger_ca);
        assert!(
            health_check(&address, Some(&misled)).await.is_err(),
            "a client that does not trust the server's CA refuses it"
        );
    }

    #[test]
    fn a_plaintext_uri_under_tls_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ca = cluster_ca();
        let tls = node(dir.path(), "n", &ca, &ca);
        assert!(endpoint_uri("http://10.0.0.2:50056", Some(&tls)).is_err());
        assert_eq!(
            endpoint_uri("10.0.0.2:50056", Some(&tls)).unwrap(),
            "https://10.0.0.2:50056"
        );
        assert_eq!(
            endpoint_uri("10.0.0.2:50056", None).unwrap(),
            "http://10.0.0.2:50056"
        );
    }

    #[test]
    fn an_absent_or_disabled_block_means_plaintext() {
        assert!(ClusterTls::load(None).unwrap().is_none());
        let disabled = ClusterTlsConfig {
            enabled: false,
            cert_path: "/nonexistent".into(),
            key_path: "/nonexistent".into(),
            ca_cert: "/nonexistent".into(),
        };
        assert!(ClusterTls::load(Some(&disabled)).unwrap().is_none());
    }
}
