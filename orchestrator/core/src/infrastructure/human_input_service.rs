// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Human Input Service - Infrastructure for human-in-the-loop workflows
//!
//! Manages human approval gates, collects feedback, and handles timeouts
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for human input service

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::domain::execution::ExecutionId;
use crate::domain::tenant::TenantId;

/// Status of a human input request
#[derive(Debug, Clone)]
pub enum HumanInputStatus {
    /// Waiting for human response
    Pending,
    /// Human approved
    Approved {
        feedback: Option<String>,
        approved_at: DateTime<Utc>,
        approved_by: Option<String>,
    },
    /// Human rejected
    Rejected {
        reason: String,
        rejected_at: DateTime<Utc>,
        rejected_by: Option<String>,
    },
    /// Request timed out
    TimedOut { timeout_at: DateTime<Utc> },
}

/// A pending human input request
#[derive(Debug)]
struct HumanInputRequest {
    /// Unique ID for this request
    id: Uuid,
    /// Associated workflow execution
    execution_id: ExecutionId,
    /// Owning tenant — the tenant of the workflow execution that emitted
    /// the approval request. Non-operator callers MUST only see/approve/
    /// reject requests in their own tenant (ADR-097). Operators see all.
    tenant_id: TenantId,
    /// Prompt to display to human
    prompt: String,
    /// When the request was created
    created_at: DateTime<Utc>,
    /// Timeout duration in seconds
    timeout_seconds: u64,
    /// Channel to send response back
    response_tx: oneshot::Sender<HumanInputStatus>,
}

/// Human Input Service for managing approval gates
pub struct HumanInputService {
    /// Pending requests indexed by ID
    pending_requests: Arc<RwLock<HashMap<Uuid, HumanInputRequest>>>,
}

impl HumanInputService {
    pub fn new() -> Self {
        Self {
            pending_requests: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Request human input and wait for response (with timeout).
    ///
    /// `tenant_id` is the owning tenant of the workflow execution that
    /// emitted this approval request. It is propagated into the stored
    /// `HumanInputRequest` so non-operator callers can be scoped to their
    /// own tenant (ADR-097).
    pub async fn request_input(
        &self,
        tenant_id: TenantId,
        execution_id: ExecutionId,
        prompt: String,
        timeout_seconds: u64,
    ) -> Result<HumanInputStatus> {
        let request_id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();

        let request = HumanInputRequest {
            id: request_id,
            execution_id,
            tenant_id,
            prompt: prompt.clone(),
            created_at: Utc::now(),
            timeout_seconds,
            response_tx: tx,
        };

        // Store the request
        {
            let mut requests = self.pending_requests.write().await;
            requests.insert(request_id, request);
        }

        info!(
            request_id = %request_id,
            execution_id = %execution_id,
            timeout_seconds = timeout_seconds,
            "Human input requested"
        );

        // Spawn timeout task
        let pending_requests = self.pending_requests.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(timeout_seconds)).await;

            // Check if request is still pending
            let mut requests = pending_requests.write().await;
            if let Some(request) = requests.remove(&request_id) {
                warn!(
                    request_id = %request_id,
                    "Human input request timed out"
                );
                let _ = request.response_tx.send(HumanInputStatus::TimedOut {
                    timeout_at: Utc::now(),
                });
            }
        });

        // Wait for response or timeout
        match rx.await {
            Ok(status) => Ok(status),
            Err(_) => {
                // Channel closed - shouldn't happen but handle gracefully
                Ok(HumanInputStatus::TimedOut {
                    timeout_at: Utc::now(),
                })
            }
        }
    }

    /// Submit approval for a pending request.
    ///
    /// Pass `tenant_filter = None` for operator callers (cross-tenant); pass
    /// `Some(tenant_id)` for non-operator callers — non-matching tenants
    /// surface "not found" rather than the cross-tenant request.
    pub async fn submit_approval_for_tenant(
        &self,
        tenant_filter: Option<&TenantId>,
        request_id: Uuid,
        feedback: Option<String>,
        approved_by: Option<String>,
    ) -> Result<()> {
        let mut requests = self.pending_requests.write().await;

        // Tenant gate: peek before removing so a tenant-mismatch leaves the
        // request in place for the legitimate owner.
        if let Some(tenant) = tenant_filter {
            let belongs = requests
                .get(&request_id)
                .map(|r| &r.tenant_id == tenant)
                .unwrap_or(false);
            if !belongs {
                anyhow::bail!("Request {request_id} not found or already completed");
            }
        }

        if let Some(request) = requests.remove(&request_id) {
            info!(
                request_id = %request_id,
                approved_by = ?approved_by,
                "Human input approved"
            );

            let status = HumanInputStatus::Approved {
                feedback,
                approved_at: Utc::now(),
                approved_by,
            };

            // Send response (ignore error if receiver dropped)
            let _ = request.response_tx.send(status);
            Ok(())
        } else {
            anyhow::bail!("Request {request_id} not found or already completed")
        }
    }

    /// Backwards-compatible wrapper used by tests; production callers use
    /// `submit_approval_for_tenant` so tenant gating is explicit.
    pub async fn submit_approval(
        &self,
        request_id: Uuid,
        feedback: Option<String>,
        approved_by: Option<String>,
    ) -> Result<()> {
        self.submit_approval_for_tenant(None, request_id, feedback, approved_by)
            .await
    }

    /// Submit rejection for a pending request. See `submit_approval_for_tenant`
    /// for tenant-filter semantics.
    pub async fn submit_rejection_for_tenant(
        &self,
        tenant_filter: Option<&TenantId>,
        request_id: Uuid,
        reason: String,
        rejected_by: Option<String>,
    ) -> Result<()> {
        let mut requests = self.pending_requests.write().await;

        if let Some(tenant) = tenant_filter {
            let belongs = requests
                .get(&request_id)
                .map(|r| &r.tenant_id == tenant)
                .unwrap_or(false);
            if !belongs {
                anyhow::bail!("Request {request_id} not found or already completed");
            }
        }

        if let Some(request) = requests.remove(&request_id) {
            info!(
                request_id = %request_id,
                rejected_by = ?rejected_by,
                reason = %reason,
                "Human input rejected"
            );

            let status = HumanInputStatus::Rejected {
                reason,
                rejected_at: Utc::now(),
                rejected_by,
            };

            // Send response (ignore error if receiver dropped)
            let _ = request.response_tx.send(status);
            Ok(())
        } else {
            anyhow::bail!("Request {request_id} not found or already completed")
        }
    }

    /// Backwards-compatible rejection wrapper for tests.
    pub async fn submit_rejection(
        &self,
        request_id: Uuid,
        reason: String,
        rejected_by: Option<String>,
    ) -> Result<()> {
        self.submit_rejection_for_tenant(None, request_id, reason, rejected_by)
            .await
    }

    /// Get list of pending requests across every tenant (operator-only —
    /// callers MUST verify operator privilege). Per-tenant callers MUST
    /// use [`Self::list_pending_requests_for_tenant`].
    pub async fn list_pending_requests(&self) -> Vec<PendingRequestInfo> {
        let requests = self.pending_requests.read().await;

        requests
            .values()
            .map(|req| PendingRequestInfo {
                id: req.id,
                execution_id: req.execution_id,
                tenant_id: req.tenant_id.clone(),
                prompt: req.prompt.clone(),
                created_at: req.created_at,
                timeout_seconds: req.timeout_seconds,
            })
            .collect()
    }

    /// List pending requests scoped to a single tenant (ADR-097).
    pub async fn list_pending_requests_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Vec<PendingRequestInfo> {
        let requests = self.pending_requests.read().await;
        requests
            .values()
            .filter(|req| &req.tenant_id == tenant_id)
            .map(|req| PendingRequestInfo {
                id: req.id,
                execution_id: req.execution_id,
                tenant_id: req.tenant_id.clone(),
                prompt: req.prompt.clone(),
                created_at: req.created_at,
                timeout_seconds: req.timeout_seconds,
            })
            .collect()
    }

    /// Fetch a specific pending request, optionally scoped to a tenant
    /// (`None` for operator callers).
    pub async fn get_pending_request_for_tenant(
        &self,
        tenant_filter: Option<&TenantId>,
        request_id: Uuid,
    ) -> Option<PendingRequestInfo> {
        let requests = self.pending_requests.read().await;
        let req = requests.get(&request_id)?;
        if let Some(tenant) = tenant_filter {
            if &req.tenant_id != tenant {
                return None;
            }
        }
        Some(PendingRequestInfo {
            id: req.id,
            execution_id: req.execution_id,
            tenant_id: req.tenant_id.clone(),
            prompt: req.prompt.clone(),
            created_at: req.created_at,
            timeout_seconds: req.timeout_seconds,
        })
    }

    /// Backwards-compatible cross-tenant fetch for tests.
    pub async fn get_pending_request(&self, request_id: Uuid) -> Option<PendingRequestInfo> {
        self.get_pending_request_for_tenant(None, request_id).await
    }

    /// Cancel a pending request
    pub async fn cancel_request(&self, request_id: Uuid) -> Result<()> {
        let mut requests = self.pending_requests.write().await;

        if let Some(request) = requests.remove(&request_id) {
            debug!(request_id = %request_id, "Human input request cancelled");

            // Send timeout status
            let _ = request.response_tx.send(HumanInputStatus::TimedOut {
                timeout_at: Utc::now(),
            });
            Ok(())
        } else {
            anyhow::bail!("Request {request_id} not found")
        }
    }
}

impl Default for HumanInputService {
    fn default() -> Self {
        Self::new()
    }
}

/// Information about a pending request (for serialization/API).
///
/// `tenant_id` is surfaced so operator-cross-tenant projections can label
/// which tenant each request belongs to (ADR-097). Non-operator callers
/// only ever see requests in their own tenant.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingRequestInfo {
    pub id: Uuid,
    pub execution_id: ExecutionId,
    pub tenant_id: TenantId,
    pub prompt: String,
    pub created_at: DateTime<Utc>,
    pub timeout_seconds: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_approval_flow() {
        let service = HumanInputService::new();
        let execution_id = ExecutionId::new();

        // Spawn task to approve after 100ms
        let service_clone = Arc::new(service);
        let service_for_approval = service_clone.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            // Get the actual request ID
            let pending = service_for_approval.list_pending_requests().await;
            if let Some(req) = pending.first() {
                service_for_approval
                    .submit_approval(
                        req.id,
                        Some("Looks good!".to_string()),
                        Some("alice".to_string()),
                    )
                    .await
                    .unwrap();
            }
        });

        // Request input
        let result = service_clone
            .request_input(
                TenantId::system(),
                execution_id,
                "Approve deployment?".to_string(),
                5,
            )
            .await
            .unwrap();

        assert!(
            matches!(result, HumanInputStatus::Approved { .. }),
            "Expected approval result, got {result:?}"
        );
        let HumanInputStatus::Approved {
            feedback,
            approved_by,
            ..
        } = result
        else {
            return;
        };
        assert_eq!(feedback, Some("Looks good!".to_string()));
        assert_eq!(approved_by, Some("alice".to_string()));
    }

    #[tokio::test]
    async fn test_rejection_flow() {
        let service = Arc::new(HumanInputService::new());
        let execution_id = ExecutionId::new();

        // Spawn task to reject after 100ms
        let service_for_rejection = service.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            let pending = service_for_rejection.list_pending_requests().await;
            if let Some(req) = pending.first() {
                service_for_rejection
                    .submit_rejection(
                        req.id,
                        "Security concerns".to_string(),
                        Some("bob".to_string()),
                    )
                    .await
                    .unwrap();
            }
        });

        // Request input
        let result = service
            .request_input(
                TenantId::system(),
                execution_id,
                "Deploy to production?".to_string(),
                5,
            )
            .await
            .unwrap();

        assert!(
            matches!(result, HumanInputStatus::Rejected { .. }),
            "Expected rejection result, got {result:?}"
        );
        let HumanInputStatus::Rejected {
            reason,
            rejected_by,
            ..
        } = result
        else {
            return;
        };
        assert_eq!(reason, "Security concerns");
        assert_eq!(rejected_by, Some("bob".to_string()));
    }

    #[tokio::test]
    async fn test_timeout_flow() {
        let service = HumanInputService::new();
        let execution_id = ExecutionId::new();

        // Request input with 1 second timeout (no response)
        let result = service
            .request_input(
                TenantId::system(),
                execution_id,
                "Quick approval needed".to_string(),
                1,
            )
            .await
            .unwrap();

        assert!(
            matches!(result, HumanInputStatus::TimedOut { .. }),
            "Expected timeout result, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_list_pending_requests() {
        let service = Arc::new(HumanInputService::new());
        let execution_id = ExecutionId::new();

        // Spawn request (it will timeout after 5 seconds)
        let service_clone = service.clone();
        tokio::spawn(async move {
            let _ = service_clone
                .request_input(
                    TenantId::system(),
                    execution_id,
                    "Test request".to_string(),
                    5,
                )
                .await;
        });

        // Wait a bit for request to be registered
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let pending = service.list_pending_requests().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].prompt, "Test request");
    }

    /// Regression: non-operator approval listing must not leak requests
    /// from other tenants (ADR-097 §approvals).
    #[tokio::test]
    async fn list_pending_approvals_non_operator_does_not_leak_other_tenants() {
        let service = Arc::new(HumanInputService::new());
        let t_a = TenantId::new("t-a".to_string()).unwrap();
        let t_b = TenantId::new("t-b".to_string()).unwrap();

        let svc_a = service.clone();
        let t_a_for_task = t_a.clone();
        tokio::spawn(async move {
            let _ = svc_a
                .request_input(t_a_for_task, ExecutionId::new(), "tenant a".into(), 5)
                .await;
        });
        let svc_b = service.clone();
        let t_b_for_task = t_b.clone();
        tokio::spawn(async move {
            let _ = svc_b
                .request_input(t_b_for_task, ExecutionId::new(), "tenant b".into(), 5)
                .await;
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let cross = service.list_pending_requests().await;
        assert_eq!(cross.len(), 2, "operator path must see both tenants");

        let only_a = service.list_pending_requests_for_tenant(&t_a).await;
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].tenant_id, t_a);
        let only_b = service.list_pending_requests_for_tenant(&t_b).await;
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].tenant_id, t_b);
    }
}
