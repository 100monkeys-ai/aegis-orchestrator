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

/// Build a `tonic::transport::Channel` to the controller. Mirrors the daemon
/// edge_lifecycle endpoint normalisation: bare host:port becomes `http://…`.
pub async fn connect_controller(endpoint: &str) -> Result<tonic::transport::Channel> {
    let uri = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    };
    tonic::transport::Channel::from_shared(uri.clone())
        .with_context(|| format!("invalid controller endpoint URI: {uri}"))?
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
