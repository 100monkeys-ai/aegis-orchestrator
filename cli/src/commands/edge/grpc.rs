// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Shared gRPC + on-disk-state helpers for `aegis edge` operator subcommands
//! that talk directly to the cluster controller via the AEGIS cluster proto
//! (ADR-117). Used by `keys rotate` and `token refresh`; future
//! `aegis edge fleet keys rotate` will reuse the same primitives.
//!
//! Disk layout (per `aegis-config.yaml` `cluster.edge.*` and
//! `cluster.node_keypair_path`):
//!
//! ```text
//! ~/.aegis/edge/
//!   node.key             32-byte Ed25519 secret (mode 0600)
//!   node.key.pub         32-byte Ed25519 public (informational)
//!   node.token           NodeSecurityToken (RS256 JWT)
//!   enrollment.jwt       one-time enrollment JWT (consumed at bootstrap)
//!   aegis-config.yaml    NodeConfig manifest (controller endpoint lives here)
//!   archive/             rotated-out keypairs awaiting --keep-old expiry
//! ```

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use prost::Message;
use rand_core::OsRng;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    node_cluster_service_client::NodeClusterServiceClient, RotateEdgeKeyInner,
    RotateEdgeKeyRequest, RotateEdgeKeyResponse, SealNodeEnvelope,
};

/// Default state directory: `~/.aegis/edge`.
pub fn default_state_dir() -> PathBuf {
    if let Some(home) = dirs_next::home_dir() {
        home.join(".aegis").join("edge")
    } else {
        PathBuf::from(".aegis/edge")
    }
}

/// Load the daemon's Ed25519 keypair from `node.key` (32 raw secret bytes).
pub fn load_signing_key(state_dir: &Path) -> Result<SigningKey> {
    let path = state_dir.join("node.key");
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "no edge identity present at {}; run `aegis edge enroll` first.",
            path.display()
        )
    })?;
    let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        anyhow!(
            "node.key at {} is not 32 bytes (Ed25519 seed)",
            path.display()
        )
    })?;
    Ok(SigningKey::from_bytes(&arr))
}

/// Read the persisted NodeSecurityToken (RS256 JWT) from `node.token`.
pub fn load_node_security_token(state_dir: &Path) -> Result<String> {
    let path = state_dir.join("node.token");
    let s = fs::read_to_string(&path).with_context(|| {
        format!(
            "no NodeSecurityToken at {}; run `aegis edge enroll` first.",
            path.display()
        )
    })?;
    Ok(s.trim().to_string())
}

/// Extract the `sub` claim (node UUID) from a NodeSecurityToken JWT.
pub fn node_id_from_token(jwt: &str) -> Result<String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        anyhow::bail!("NodeSecurityToken is not a well-formed JWT");
    }
    let claims_b = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .context("decode NodeSecurityToken claims")?;
    #[derive(serde::Deserialize)]
    struct C {
        sub: String,
    }
    let c: C = serde_json::from_slice(&claims_b).context("parse NodeSecurityToken claims")?;
    Ok(c.sub)
}

/// Read the daemon's persisted `node_id` (UUID) from
/// `aegis-config.yaml` `spec.node.id`. The CLI mints this UUID at bootstrap
/// time and writes it into the config; subsequent enrollments and the
/// AttestNode/ChallengeNode wire flow read it back here.
///
/// Errors if the config is missing, malformed, or the field is empty / not a
/// well-formed UUID. The strict-UUID check matches the server's
/// `NodeId::from_string` contract — surfacing a malformed value here gives
/// the operator a clearer error than "Invalid NodeId" from the server.
pub fn load_node_id_from_config(state_dir: &Path) -> Result<String> {
    let cfg_path = state_dir.join("aegis-config.yaml");
    let body =
        fs::read_to_string(&cfg_path).with_context(|| format!("read {}", cfg_path.display()))?;
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&body).with_context(|| format!("parse {}", cfg_path.display()))?;
    let id = doc
        .get("spec")
        .and_then(|v| v.get("node"))
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() {
        anyhow::bail!(
            "{} `spec.node.id` is empty; bootstrap must mint a UUID before handshake",
            cfg_path.display()
        );
    }
    uuid::Uuid::parse_str(&id).with_context(|| {
        format!(
            "{} `spec.node.id` ('{}') is not a valid UUID",
            cfg_path.display(),
            id
        )
    })?;
    Ok(id)
}

/// Resolve the controller endpoint from the daemon's `aegis-config.yaml`.
/// Falls back to the `cep` claim of `enrollment.jwt` if config is missing.
pub fn load_controller_endpoint(state_dir: &Path) -> Result<String> {
    let cfg_path = state_dir.join("aegis-config.yaml");
    if cfg_path.exists() {
        let body = fs::read_to_string(&cfg_path)
            .with_context(|| format!("read {}", cfg_path.display()))?;
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&body).with_context(|| format!("parse {}", cfg_path.display()))?;
        if let Some(ep) = doc
            .get("spec")
            .and_then(|v| v.get("cluster"))
            .and_then(|v| v.get("controller"))
            .and_then(|v| v.get("endpoint"))
            .and_then(|v| v.as_str())
        {
            if !ep.is_empty() && !ep.contains("{{") {
                return Ok(ep.to_string());
            }
        }
    }
    let enrollment = state_dir.join("enrollment.jwt");
    if enrollment.exists() {
        let jwt = fs::read_to_string(&enrollment)?;
        let parts: Vec<&str> = jwt.trim().split('.').collect();
        if parts.len() == 3 {
            let claims_b = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[1])
                .context("decode enrollment claims")?;
            #[derive(serde::Deserialize)]
            struct C {
                cep: String,
            }
            if let Ok(c) = serde_json::from_slice::<C>(&claims_b) {
                return Ok(c.cep);
            }
        }
    }
    Err(anyhow!(
        "could not resolve controller endpoint from {} or enrollment.jwt",
        cfg_path.display()
    ))
}

/// Promote a user-supplied controller endpoint to a fully-qualified URI,
/// defaulting scheme-less endpoints to TLS for public hostnames and to
/// plaintext for loopback / explicit non-443 ports.
///
/// The SaaS topology terminates TLS at Caddy on `relay.myzaru.com:443` and
/// h2c-proxies upstream to the relay's gRPC port (ADR-117). Edge daemons
/// therefore MUST dial `https://` against public endpoints; OSS deployments
/// commonly dial `localhost:50056` plaintext.
///
/// Rule (applied only when the endpoint is scheme-less):
///
/// - port missing OR port == 443 → `https://`
/// - any other explicit port → `http://`
///
/// Endpoints that already carry a `http://` or `https://` scheme are
/// returned unchanged — explicit user choice always wins.
pub fn promote_endpoint_uri(endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }

    // Find the host:port boundary. `host_part` is everything up to the first
    // `/`, `?`, or `#`; the port lives after the LAST `:` in the host segment
    // (to keep IPv6 literals like `[::1]:50056` working).
    let host_part = endpoint
        .split(|c| c == '/' || c == '?' || c == '#')
        .next()
        .unwrap_or(endpoint);
    let port_str: Option<&str> = if let Some(rest) = host_part.strip_prefix('[') {
        // Bracketed IPv6: `[addr]:port` — split on `]:`.
        rest.split_once("]:").map(|(_, p)| p)
    } else {
        host_part.rsplit_once(':').map(|(_, p)| p)
    };

    let use_tls = match port_str {
        None => true,
        Some(p) => p.parse::<u16>().map(|n| n == 443).unwrap_or(false),
    };
    if use_tls {
        format!("https://{endpoint}")
    } else {
        format!("http://{endpoint}")
    }
}

/// Build a `tonic::transport::Channel` to the controller. Scheme-less
/// endpoints are promoted via [`promote_endpoint_uri`]; `https://` endpoints
/// get a `ClientTlsConfig` with native roots so the dial completes the TLS
/// handshake before tonic starts speaking HTTP/2.
pub async fn connect_controller(endpoint: &str) -> Result<tonic::transport::Channel> {
    let uri = promote_endpoint_uri(endpoint);
    let mut channel = tonic::transport::Channel::from_shared(uri.clone())
        .with_context(|| format!("invalid controller endpoint URI: {uri}"))?;
    if uri.starts_with("https://") {
        let tls = tonic::transport::ClientTlsConfig::new().with_native_roots();
        channel = channel
            .tls_config(tls)
            .with_context(|| format!("configure TLS for controller {uri}"))?;
    }
    channel
        .connect()
        .await
        .with_context(|| format!("connect to controller {uri}"))
}

/// Compute the deterministic ADR-117 §"Key & token rotation" challenge:
/// `sha256("aegis-edge-rotate" || node_id || new_public_key || nonce)`.
///
/// MUST match `RotateEdgeKeyService::rotation_challenge` in
/// `orchestrator/core/src/application/edge/rotate_edge_key.rs`. The
/// orchestrator-side helper is `pub` but lives in the `aegis-orchestrator-core`
/// crate which the CLI binary already depends on; we duplicate the byte-level
/// construction here to keep the CLI free of the application layer's heavy
/// transitive dep graph (postgres, openbao, etc.). The two implementations
/// MUST stay in lockstep.
pub fn rotation_challenge(node_id_str: &str, new_public_key: &[u8], nonce: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"aegis-edge-rotate");
    // Orchestrator hashes `NodeId.0.as_bytes()` where `NodeId.0: Uuid`. Uuid's
    // `as_bytes()` returns the 16-byte big-endian representation. We accept
    // the canonical hyphenated string here and convert.
    let uuid = uuid::Uuid::parse_str(node_id_str).expect("node_id must be a valid UUID");
    h.update(uuid.as_bytes());
    h.update(new_public_key);
    h.update(nonce);
    h.finalize().to_vec()
}

/// Build a fully-formed `RotateEdgeKeyRequest`. When `new_signing_key` is
/// `None`, the same key is used as both old and new — the degenerate
/// "token refresh" path (T5).
pub fn build_rotate_request(
    old_signing_key: &SigningKey,
    new_signing_key: &SigningKey,
    node_id_str: &str,
) -> Result<RotateEdgeKeyRequest> {
    use rand_core::RngCore;
    let mut nonce = vec![0u8; 32];
    OsRng.fill_bytes(&mut nonce);
    let new_pub = new_signing_key.verifying_key().to_bytes().to_vec();
    let inner = RotateEdgeKeyInner {
        node_id: node_id_str.to_string(),
        new_public_key: new_pub.clone(),
        nonce: nonce.clone(),
    };
    let mut payload = Vec::with_capacity(inner.encoded_len());
    inner.encode(&mut payload).context("encode inner")?;
    let outer_sig = old_signing_key.sign(&payload).to_bytes().to_vec();
    let challenge = rotation_challenge(node_id_str, &new_pub, &nonce);
    let new_sig = new_signing_key.sign(&challenge).to_bytes().to_vec();
    Ok(RotateEdgeKeyRequest {
        current_envelope: Some(SealNodeEnvelope {
            node_security_token: String::new(),
            signature: outer_sig,
            payload,
        }),
        new_public_key: new_pub,
        signature_with_new_key: new_sig,
    })
}

/// Generate a fresh Ed25519 keypair via `OsRng`. Matches the precedent in
/// `cli/src/commands/node.rs` and `cli/src/commands/edge/bootstrap.rs`.
pub fn generate_signing_key() -> SigningKey {
    let mut csprng = OsRng;
    SigningKey::generate(&mut csprng)
}

/// Atomically write `bytes` to `path` with mode 0600. Same-directory tempfile
/// + `rename` guarantees there is never a window where neither file exists.
pub fn atomic_write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("aegis"),
        std::process::id()
    ));
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    set_file_perms(&tmp, 0o600)?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(unix)]
pub fn set_file_perms(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
pub fn set_file_perms(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Parse a duration like "24h", "30m", "7d", "120s". A small, dependency-free
/// parser since `humantime` is not in the workspace.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration");
    }
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    if num.is_empty() {
        anyhow::bail!("duration missing numeric component: {s}");
    }
    let n: u64 = num.parse().with_context(|| format!("parse {num}"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        other => anyhow::bail!("unknown duration unit '{other}'; use s|m|h|d"),
    };
    Ok(Duration::from_secs(secs))
}

/// Best-effort detection of a running edge daemon. Checks the user-mode
/// systemd unit name we ship in `cli/templates`. Returns `Ok(true)` when
/// active, `Ok(false)` otherwise (including when systemctl is unavailable).
pub fn edge_daemon_appears_running() -> bool {
    use std::process::Command;
    let out = Command::new("systemctl")
        .args(["--user", "is-active", "aegis-edge.service"])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim() == "active"
        }
        Err(_) => false,
    }
}

/// Issue a `RotateEdgeKey` RPC against the controller and return the response.
pub async fn call_rotate(
    endpoint: &str,
    req: RotateEdgeKeyRequest,
) -> Result<RotateEdgeKeyResponse> {
    let channel = connect_controller(endpoint).await?;
    let mut client = NodeClusterServiceClient::new(channel);
    let resp = client
        .rotate_edge_key(tonic::Request::new(req))
        .await
        .context("RotateEdgeKey RPC failed")?
        .into_inner();
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_supports_common_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("nope").is_err());
        assert!(parse_duration("10y").is_err());
    }

    #[test]
    fn promote_endpoint_uri_defaults_to_tls_for_public_hostnames() {
        // Regression: connect_controller used to hardcode http:// for any
        // scheme-less endpoint, causing plaintext h2c dials against
        // relay.myzaru.com (which terminates TLS at Caddy on :443) and
        // returning `h2 protocol error: http2 error`.
        assert_eq!(
            promote_endpoint_uri("relay.myzaru.com"),
            "https://relay.myzaru.com",
            "scheme-less, port-less public hostname must default to TLS"
        );
        assert_eq!(
            promote_endpoint_uri("relay.myzaru.com:443"),
            "https://relay.myzaru.com:443",
            "scheme-less host:443 must default to TLS"
        );
    }

    #[test]
    fn promote_endpoint_uri_keeps_loopback_and_nonstandard_ports_plaintext() {
        // OSS / dev deployments dial scheme-less localhost over plaintext;
        // operators dialing a non-443 port have explicitly opted out of the
        // standard TLS port and must not be silently TLS-promoted.
        assert_eq!(
            promote_endpoint_uri("localhost:50056"),
            "http://localhost:50056"
        );
        assert_eq!(
            promote_endpoint_uri("127.0.0.1:50056"),
            "http://127.0.0.1:50056"
        );
        assert_eq!(
            promote_endpoint_uri("[::1]:50056"),
            "http://[::1]:50056",
            "IPv6 loopback with explicit non-443 port stays plaintext"
        );
        assert_eq!(
            promote_endpoint_uri("relay.myzaru.com:50056"),
            "http://relay.myzaru.com:50056",
            "non-443 port on a public hostname stays plaintext (operator opt-out)"
        );
    }

    #[test]
    fn promote_endpoint_uri_preserves_explicit_schemes() {
        // Explicit user choice always wins — never rewrite a scheme.
        assert_eq!(
            promote_endpoint_uri("http://localhost:50056"),
            "http://localhost:50056"
        );
        assert_eq!(
            promote_endpoint_uri("https://relay.myzaru.com"),
            "https://relay.myzaru.com"
        );
        assert_eq!(
            promote_endpoint_uri("http://relay.myzaru.com"),
            "http://relay.myzaru.com",
            "explicit http:// against a public host stays plaintext"
        );
    }

    #[test]
    fn rotation_challenge_matches_orchestrator_construction() {
        // Cross-check against the orchestrator-side helper to keep the two
        // implementations in lockstep.
        use aegis_orchestrator_core::application::edge::rotate_edge_key::RotateEdgeKeyService;
        use aegis_orchestrator_core::domain::shared_kernel::NodeId;
        let uuid = uuid::Uuid::new_v4();
        let pk = [3u8; 32];
        let nonce = [9u8; 32];
        let lhs = rotation_challenge(&uuid.to_string(), &pk, &nonce);
        let rhs = RotateEdgeKeyService::rotation_challenge(&NodeId(uuid), &pk, &nonce);
        assert_eq!(lhs, rhs);
    }
}
