// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Shared HTTP client helper for `aegis edge *` operator subcommands.
//!
//! Resolves the orchestrator base URL from (in order):
//!   * `AEGIS_API_URL` env var,
//!   * `AEGIS_CONTROLLER_ENDPOINT` env var,
//!   * the active profile's `env` field (from `~/.aegis/auth.json`),
//!   * the local default `http://127.0.0.1:8080`.
//!
//! Authentication header (`Authorization: Bearer <access_key>`) is supplied by
//! the active profile when present. The orchestrator's `tenant_context_middleware`
//! reads the canonical `X-Tenant-Id` header (per ADR-100/-111); the helper
//! injects it from `AEGIS_EFFECTIVE_TENANT` (kept as the env var name for
//! operator ergonomics) or fails with a typed error so the caller can surface
//! a useful message.

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::auth::load_store;

/// Canonical tenant header consumed by `tenant_context_middleware` per
/// ADR-100/-111. Centralized so the value cannot drift between the request
/// builder and the regression suite.
pub(crate) const TENANT_HEADER: &str = "x-tenant-id";

/// Insert the canonical tenant header into a `HeaderMap`. Extracted so the
/// regression test can assert the exact header name without standing up the
/// full `from_env` constructor.
pub(crate) fn insert_tenant_header(headers: &mut HeaderMap, tenant: &str) -> Result<()> {
    let name = HeaderName::from_static(TENANT_HEADER);
    headers.insert(
        name,
        HeaderValue::from_str(tenant).context("invalid AEGIS_EFFECTIVE_TENANT")?,
    );
    Ok(())
}

pub struct EdgeApiClient {
    base_url: String,
    inner: reqwest::Client,
    headers: HeaderMap,
}

impl EdgeApiClient {
    /// Test-only constructor: build a client pointed at an arbitrary base
    /// URL with no auth/tenant headers. Used by the `aegis edge logout`
    /// regression suite to drive an in-process mock orchestrator without
    /// depending on env vars or the operator auth store.
    #[cfg(test)]
    pub(crate) fn new_for_test(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            inner: reqwest::Client::new(),
            headers: HeaderMap::new(),
        }
    }

    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("AEGIS_API_URL")
            .or_else(|_| std::env::var("AEGIS_CONTROLLER_ENDPOINT"))
            .or_else(|_| {
                load_store()
                    .ok()
                    .and_then(|s| s.profiles.get(&s.active_profile).cloned())
                    .map(|p| {
                        if p.env.starts_with("http") {
                            p.env
                        } else {
                            format!("https://{}", p.env)
                        }
                    })
                    .ok_or(std::env::VarError::NotPresent)
            })
            .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());

        let mut headers = HeaderMap::new();
        if let Ok(store) = load_store() {
            if let Some(profile) = store.profiles.get(&store.active_profile) {
                if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", profile.access_key))
                {
                    headers.insert(AUTHORIZATION, value);
                }
            }
        }

        let tenant = std::env::var("AEGIS_EFFECTIVE_TENANT").map_err(|_| {
            anyhow!("AEGIS_EFFECTIVE_TENANT env var is required for /v1/edge/* calls")
        })?;
        insert_tenant_header(&mut headers, &tenant)?;

        Ok(Self {
            base_url,
            inner: reqwest::Client::new(),
            headers,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .inner
            .get(self.url(path))
            .headers(self.headers.clone())
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        Self::decode(resp).await
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let resp = self
            .inner
            .post(self.url(path))
            .headers(self.headers.clone())
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        Self::decode(resp).await
    }

    pub async fn patch<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let resp = self
            .inner
            .patch(self.url(path))
            .headers(self.headers.clone())
            .json(body)
            .send()
            .await
            .with_context(|| format!("PATCH {path}"))?;
        Self::decode(resp).await
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        let resp = self
            .inner
            .delete(self.url(path))
            .headers(self.headers.clone())
            .send()
            .await
            .with_context(|| format!("DELETE {path}"))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| String::from("<unreadable response body>"));
            Err(anyhow!("{status}: {body}"))
        }
    }

    pub async fn post_streamed(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response> {
        self.inner
            .post(self.url(path))
            .headers(self.headers.clone())
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))
    }

    async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let bytes = resp.bytes().await.context("read body")?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            return Err(anyhow!("{status}: {body}"));
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("decode response: {}", String::from_utf8_lossy(&bytes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the CLI used to send `X-Effective-Tenant`, which the
    /// orchestrator's `tenant_context_middleware` does not read — every
    /// `aegis edge ls`/`tag` call produced a 400 from the missing-tenant
    /// rejection path. The canonical contract per ADR-100/-111 is
    /// `X-Tenant-Id`. This test pins the constant and the insertion helper
    /// so a future "rename" cannot silently regress edge ops.
    #[test]
    fn tenant_header_uses_canonical_x_tenant_id() {
        assert_eq!(TENANT_HEADER, "x-tenant-id");

        let mut headers = HeaderMap::new();
        insert_tenant_header(&mut headers, "t-consumer").expect("inject tenant header");

        assert_eq!(
            headers.get("x-tenant-id").and_then(|v| v.to_str().ok()),
            Some("t-consumer"),
            "must inject the canonical X-Tenant-Id header consumed by \
             tenant_context_middleware"
        );
        assert!(
            headers.get("x-effective-tenant").is_none(),
            "must NOT emit the obsolete X-Effective-Tenant header — the \
             orchestrator middleware ignores it and this header drift was \
             the original bug"
        );
    }
}
