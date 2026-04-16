// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Billing Handlers (BC-12 — Stripe Integration)
//!
//! Endpoints for Stripe-powered subscription management. All handlers return
//! `501 Not Implemented` when `BillingConfig` is absent from the node config,
//! making the billing feature fully optional.
//!
//! | Method | Path | Description |
//! |--------|------|-------------|
//! | `GET`  | `/v1/billing/prices` | List all active prices from Stripe (public) |
//! | `POST` | `/v1/billing/checkout` | Create a Stripe Checkout session |
//! | `POST` | `/v1/billing/portal` | Create a Stripe Customer Portal session |
//! | `GET`  | `/v1/billing/subscription` | Get current subscription details |
//! | `GET`  | `/v1/billing/invoices` | List invoices from Stripe |

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use tracing::{info, warn};

use aegis_orchestrator_core::domain::billing::{SubscriptionStatus, TenantSubscription};
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::node_config::{resolve_env_value, BillingConfig};
use aegis_orchestrator_core::domain::tenancy::TenantTier;
use aegis_orchestrator_core::infrastructure::repositories::BillingRepository;

use crate::daemon::state::AppState;

// ── Stripe client helper ────────────────────────────────────────────────────

/// Build a Stripe client from the resolved `BillingConfig`.
fn stripe_client_from_config(billing: &BillingConfig) -> Option<stripe::Client> {
    let key = resolve_env_value(&billing.stripe_secret_key).ok()?;
    if key.is_empty() {
        return None;
    }
    Some(stripe::Client::new(key))
}

fn not_implemented() -> axum::response::Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "billing_not_configured", "message": "Stripe billing is not configured"})),
    )
        .into_response()
}

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
struct PriceInfo {
    price_id: String,
    amount: i64,
    currency: String,
}

#[derive(Debug, serde::Serialize)]
struct TierPricing {
    tier: String,
    product_id: String,
    name: String,
    description: Option<String>,
    included_seats: u32,
    monthly: Option<PriceInfo>,
    annual: Option<PriceInfo>,
    seat_monthly: Option<PriceInfo>,
    seat_annual: Option<PriceInfo>,
}

#[derive(Debug, serde::Serialize)]
struct PricesResponse {
    tiers: Vec<TierPricing>,
}

// ── Request types ────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub(crate) struct CheckoutRequest {
    /// Stripe Price ID for the base plan (from the prices endpoint).
    pub price_id: String,
    /// Optional Stripe Price ID for extra seat add-on.
    #[serde(default)]
    pub seat_price_id: Option<String>,
    /// Number of extra seats beyond the included count.
    #[serde(default)]
    pub seats: u32,
    /// URL to redirect after successful checkout.
    pub success_url: String,
    /// URL to redirect on cancellation.
    pub cancel_url: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct PortalRequest {
    /// URL to redirect back to after portal session.
    pub return_url: String,
}

// ── Tier mapping helpers ────────────────────────────────────────────────────

/// Map a Stripe product name to a (tier, included_seats) tuple.
/// Returns `None` for products that are seat add-ons.
fn product_name_to_tier(name: &str) -> Option<(&'static str, u32)> {
    // Skip seat add-on products — they are matched separately
    if name.contains("Extra Seat") {
        return None;
    }
    if name.starts_with("Zaru Pro") {
        Some(("pro", 3))
    } else if name.starts_with("Zaru Business") {
        Some(("business", 5))
    } else if name.starts_with("Zaru Enterprise") {
        Some(("enterprise", 10))
    } else {
        None
    }
}

/// Extract the tier prefix from a seat add-on product name.
/// e.g. "Zaru Pro - Extra Seat" -> "pro"
fn seat_product_tier(name: &str) -> Option<&'static str> {
    if !name.contains("Extra Seat") {
        return None;
    }
    if name.starts_with("Zaru Pro") {
        Some("pro")
    } else if name.starts_with("Zaru Business") {
        Some("business")
    } else if name.starts_with("Zaru Enterprise") {
        Some("enterprise")
    } else {
        None
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// `GET /v1/billing/prices` — List all active Zaru prices from Stripe.
///
/// This endpoint is **public** (no auth required) — it returns pricing information
/// that is displayed on the public pricing page.
pub(crate) async fn list_prices_handler(
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    let billing = match &state.billing_config {
        Some(c) => c,
        None => return not_implemented(),
    };
    let client = match stripe_client_from_config(billing) {
        Some(c) => c,
        None => return not_implemented(),
    };

    // 1. List all active products
    let mut product_params = stripe::ListProducts::new();
    product_params.active = Some(true);
    product_params.limit = Some(100);

    let products = match stripe::Product::list(&client, &product_params).await {
        Ok(list) => list.data,
        Err(e) => {
            warn!(error = %e, "Failed to list Stripe products");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("Failed to list products from Stripe: {e}")})),
            )
                .into_response();
        }
    };

    // Filter to Zaru products only
    let zaru_products: Vec<_> = products
        .iter()
        .filter(|p| {
            p.name
                .as_deref()
                .map(|n| n.starts_with("Zaru"))
                .unwrap_or(false)
        })
        .collect();

    // 2. Build a map of tier -> TierPricing
    use std::collections::HashMap;
    let mut tier_map: HashMap<&str, TierPricing> = HashMap::new();

    // Initialize base plan entries
    for product in &zaru_products {
        let name = match product.name.as_deref() {
            Some(n) => n,
            None => continue,
        };
        if let Some((tier, included_seats)) = product_name_to_tier(name) {
            tier_map.entry(tier).or_insert_with(|| TierPricing {
                tier: tier.to_string(),
                product_id: product.id.to_string(),
                name: name.to_string(),
                description: product.description.clone(),
                included_seats,
                monthly: None,
                annual: None,
                seat_monthly: None,
                seat_annual: None,
            });
        }
    }

    // 3. For each Zaru product, list active prices and assign to tiers
    for product in &zaru_products {
        let name = match product.name.as_deref() {
            Some(n) => n,
            None => continue,
        };

        let product_id_str = product.id.to_string();
        let mut price_params = stripe::ListPrices::new();
        price_params.product = Some(stripe::IdOrCreate::Id(&product_id_str));
        price_params.active = Some(true);
        price_params.limit = Some(50);

        let prices = match stripe::Price::list(&client, &price_params).await {
            Ok(list) => list.data,
            Err(e) => {
                warn!(error = %e, product_id = %product.id, "Failed to list prices for product");
                continue;
            }
        };

        let is_seat_addon = name.contains("Extra Seat");
        let tier_key = if is_seat_addon {
            seat_product_tier(name)
        } else {
            product_name_to_tier(name).map(|(t, _)| t)
        };

        let tier_key = match tier_key {
            Some(t) => t,
            None => continue,
        };

        let tier_entry = match tier_map.get_mut(tier_key) {
            Some(e) => e,
            None => continue,
        };

        for price in &prices {
            let amount = price.unit_amount.unwrap_or(0);
            let currency = price
                .currency
                .map(|c| c.to_string())
                .unwrap_or_else(|| "usd".to_string());

            let info = PriceInfo {
                price_id: price.id.to_string(),
                amount,
                currency,
            };

            let interval = price
                .recurring
                .as_ref()
                .map(|r| r.interval)
                .unwrap_or(stripe::RecurringInterval::Month);

            match (is_seat_addon, interval) {
                (false, stripe::RecurringInterval::Month) => {
                    tier_entry.monthly = Some(info);
                }
                (false, stripe::RecurringInterval::Year) => {
                    tier_entry.annual = Some(info);
                }
                (true, stripe::RecurringInterval::Month) => {
                    tier_entry.seat_monthly = Some(info);
                }
                (true, stripe::RecurringInterval::Year) => {
                    tier_entry.seat_annual = Some(info);
                }
                _ => {}
            }
        }
    }

    // 4. Build ordered response
    let tier_order = ["pro", "business", "enterprise"];
    let tiers: Vec<TierPricing> = tier_order
        .iter()
        .filter_map(|t| tier_map.remove(t))
        .collect();

    (StatusCode::OK, Json(PricesResponse { tiers })).into_response()
}

/// `POST /v1/billing/checkout` — Create a Stripe Checkout Session.
///
/// The client sends the exact `price_id` (obtained from `GET /v1/billing/prices`)
/// along with optional seat add-on details.
pub(crate) async fn create_checkout_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Json(body): Json<CheckoutRequest>,
) -> axum::response::Response {
    let billing = match &state.billing_config {
        Some(c) => c,
        None => return not_implemented(),
    };
    let stripe = match stripe_client_from_config(billing) {
        Some(c) => c,
        None => return not_implemented(),
    };

    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    // Validate price_id format
    let price_id_parsed: stripe::PriceId = match body.price_id.parse() {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid price_id: {e}")})),
            )
                .into_response();
        }
    };

    let tenant_id = crate::daemon::handlers::tenant_id_from_identity(Some(&identity));

    // Look up or create Stripe customer
    let billing_repo = match &state.billing_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Billing repository not configured"})),
            )
                .into_response();
        }
    };

    let existing_sub = match billing_repo.get_subscription(&tenant_id).await {
        Ok(sub) => sub,
        Err(e) => {
            warn!(error = %e, "Failed to look up subscription");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let customer_id = if let Some(ref sub) = existing_sub {
        // Sync name/email to Stripe on every checkout in case they changed
        let name = identity
            .raw_claims
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let email = identity.email.as_deref().unwrap_or("");
        if let Ok(cid) = sub.stripe_customer_id.parse::<stripe::CustomerId>() {
            let mut update = stripe::UpdateCustomer::new();
            if !name.is_empty() {
                update.name = Some(name);
            }
            if !email.is_empty() {
                update.email = Some(email);
            }
            if let Err(e) = stripe::Customer::update(&stripe, &cid, update).await {
                warn!(error = %e, "Failed to sync customer name/email to Stripe");
            }
        }
        sub.stripe_customer_id.clone()
    } else {
        // Create a new Stripe customer
        let email = identity.email.as_deref().unwrap_or("");
        let name = identity
            .raw_claims
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mut params = stripe::CreateCustomer::new();
        params.email = Some(email);
        if !name.is_empty() {
            params.name = Some(name);
        }
        params.metadata = Some(
            [("tenant_id".to_string(), tenant_id.as_str().to_string())]
                .into_iter()
                .collect(),
        );

        match stripe::Customer::create(&stripe, params).await {
            Ok(customer) => {
                let cust_id = customer.id.to_string();
                // Persist the customer mapping
                let now = chrono::Utc::now();
                let new_sub = TenantSubscription {
                    tenant_id: tenant_id.clone(),
                    stripe_customer_id: cust_id.clone(),
                    stripe_subscription_id: None,
                    tier: TenantTier::Free,
                    status: SubscriptionStatus::None,
                    current_period_end: None,
                    cancel_at_period_end: false,
                    created_at: now,
                    updated_at: now,
                };
                if let Err(e) = billing_repo.upsert_subscription(&new_sub).await {
                    warn!(error = %e, "Failed to persist new Stripe customer mapping");
                }
                cust_id
            }
            Err(e) => {
                warn!(error = %e, "Failed to create Stripe customer");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("Failed to create Stripe customer: {e}")})),
                )
                    .into_response();
            }
        }
    };

    // Create Checkout Session
    let customer_id_parsed: stripe::CustomerId = match customer_id.parse() {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Invalid customer ID: {e}")})),
            )
                .into_response();
        }
    };

    // Build line items: base plan + optional seat add-on
    let mut line_items = vec![stripe::CreateCheckoutSessionLineItems {
        price: Some(price_id_parsed.to_string()),
        quantity: Some(1),
        ..Default::default()
    }];

    if let Some(ref seat_price_id) = body.seat_price_id {
        if body.seats > 0 {
            line_items.push(stripe::CreateCheckoutSessionLineItems {
                price: Some(seat_price_id.clone()),
                quantity: Some(body.seats as u64),
                ..Default::default()
            });
        }
    }

    let tenant_meta: std::collections::HashMap<String, String> =
        [("tenant_id".to_string(), tenant_id.as_str().to_string())]
            .into_iter()
            .collect();

    let mut params = stripe::CreateCheckoutSession::new();
    params.customer = Some(customer_id_parsed);
    params.mode = Some(stripe::CheckoutSessionMode::Subscription);
    params.success_url = Some(&body.success_url);
    params.cancel_url = Some(&body.cancel_url);
    params.line_items = Some(line_items);
    // Set metadata on both the session and the subscription so the webhook
    // handler can find tenant_id regardless of which object Stripe sends.
    params.metadata = Some(tenant_meta.clone());
    params.subscription_data = Some(stripe::CreateCheckoutSessionSubscriptionData {
        metadata: Some(tenant_meta),
        ..Default::default()
    });

    match stripe::CheckoutSession::create(&stripe, params).await {
        Ok(session) => {
            let url = session.url.unwrap_or_default();
            info!(tenant_id = %tenant_id, price_id = %body.price_id, "Checkout session created");
            (StatusCode::OK, Json(json!({"url": url}))).into_response()
        }
        Err(e) => {
            warn!(error = %e, "Failed to create Checkout session");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to create checkout session: {e}")})),
            )
                .into_response()
        }
    }
}

/// `POST /v1/billing/portal` — Create a Stripe Customer Portal session.
pub(crate) async fn create_portal_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Json(body): Json<PortalRequest>,
) -> axum::response::Response {
    let billing = match &state.billing_config {
        Some(c) => c,
        None => return not_implemented(),
    };
    let stripe = match stripe_client_from_config(billing) {
        Some(c) => c,
        None => return not_implemented(),
    };

    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    let tenant_id = crate::daemon::handlers::tenant_id_from_identity(Some(&identity));

    let billing_repo = match &state.billing_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Billing repository not configured"})),
            )
                .into_response();
        }
    };

    let sub = match billing_repo.get_subscription(&tenant_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "No billing account found. Please subscribe first."})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let customer_id: stripe::CustomerId = match sub.stripe_customer_id.parse() {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Invalid customer ID: {e}")})),
            )
                .into_response();
        }
    };

    let mut params = stripe::CreateBillingPortalSession::new(customer_id);
    params.return_url = Some(&body.return_url);

    match stripe::BillingPortalSession::create(&stripe, params).await {
        Ok(session) => (StatusCode::OK, Json(json!({"url": session.url}))).into_response(),
        Err(e) => {
            warn!(error = %e, "Failed to create portal session");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to create portal session: {e}")})),
            )
                .into_response()
        }
    }
}

/// `GET /v1/billing/subscription` — Get current subscription details.
pub(crate) async fn get_subscription_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
) -> axum::response::Response {
    if state.billing_config.is_none() {
        return not_implemented();
    }

    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    let tenant_id = crate::daemon::handlers::tenant_id_from_identity(Some(&identity));

    let billing_repo = match &state.billing_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Billing repository not configured"})),
            )
                .into_response();
        }
    };

    match billing_repo.get_subscription(&tenant_id).await {
        Ok(Some(sub)) => (
            StatusCode::OK,
            Json(json!({
                "tenant_id": sub.tenant_id.as_str(),
                "tier": tier_to_str(&sub.tier),
                "status": sub.status.as_str(),
                "stripe_subscription_id": sub.stripe_subscription_id,
                "current_period_end": sub.current_period_end,
                "cancel_at_period_end": sub.cancel_at_period_end,
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::OK,
            Json(json!({
                "tenant_id": tenant_id.as_str(),
                "tier": "free",
                "status": "none",
                "stripe_subscription_id": null,
                "current_period_end": null,
                "cancel_at_period_end": false,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/billing/invoices` — List invoices from Stripe.
pub(crate) async fn list_invoices_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
) -> axum::response::Response {
    let billing = match &state.billing_config {
        Some(c) => c,
        None => return not_implemented(),
    };
    let stripe = match stripe_client_from_config(billing) {
        Some(c) => c,
        None => return not_implemented(),
    };

    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    let tenant_id = crate::daemon::handlers::tenant_id_from_identity(Some(&identity));

    let billing_repo = match &state.billing_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Billing repository not configured"})),
            )
                .into_response();
        }
    };

    let sub = match billing_repo.get_subscription(&tenant_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (StatusCode::OK, Json(json!({"invoices": [], "count": 0}))).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let customer_id: stripe::CustomerId = match sub.stripe_customer_id.parse() {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Invalid customer ID: {e}")})),
            )
                .into_response();
        }
    };

    let mut params = stripe::ListInvoices::new();
    params.customer = Some(customer_id);

    match stripe::Invoice::list(&stripe, &params).await {
        Ok(list) => {
            let invoices: Vec<serde_json::Value> = list
                .data
                .iter()
                .map(|inv| {
                    json!({
                        "id": inv.id.to_string(),
                        "amount_due": inv.amount_due,
                        "amount_paid": inv.amount_paid,
                        "currency": inv.currency.map(|c| c.to_string()),
                        "status": inv.status.map(|s| format!("{:?}", s)),
                        "created": inv.created,
                        "hosted_invoice_url": inv.hosted_invoice_url,
                        "invoice_pdf": inv.invoice_pdf,
                    })
                })
                .collect();
            let count = invoices.len();
            (
                StatusCode::OK,
                Json(json!({"invoices": invoices, "count": count})),
            )
                .into_response()
        }
        Err(e) => {
            warn!(error = %e, "Failed to list invoices");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list invoices: {e}")})),
            )
                .into_response()
        }
    }
}

// ── Stripe webhook processing ────────────────────────────────────────────────

/// Process a Stripe webhook event. Called from the stimulus/webhook handler
/// when the source is `"stripe"`.
///
/// Handles:
/// - `checkout.session.completed` — link tenant to subscription, upgrade tier
/// - `customer.subscription.updated` — sync tier/status/period_end
/// - `customer.subscription.deleted` — downgrade to Free
/// - `invoice.payment_failed` — mark subscription as past_due
pub(crate) async fn process_stripe_event(
    state: &AppState,
    event_type: &str,
    payload: &serde_json::Value,
) {
    let billing_repo = match &state.billing_repo {
        Some(r) => r.clone(),
        None => {
            warn!("Stripe webhook received but billing repository not configured");
            return;
        }
    };

    match event_type {
        "checkout.session.completed" => {
            handle_checkout_completed(state, &*billing_repo, payload).await;
        }
        "customer.subscription.updated" => {
            handle_subscription_updated(&*billing_repo, payload).await;
        }
        "customer.subscription.deleted" => {
            handle_subscription_deleted(state, &*billing_repo, payload).await;
        }
        "invoice.payment_failed" => {
            handle_payment_failed(&*billing_repo, payload).await;
        }
        _ => {
            info!(event_type, "Ignoring unhandled Stripe event type");
        }
    }
}

async fn handle_checkout_completed(
    state: &AppState,
    billing_repo: &dyn BillingRepository,
    payload: &serde_json::Value,
) {
    // payload is already event.data.object (the session object itself),
    // extracted by the stimulus handler before calling process_stripe_event.
    let obj = payload;

    let customer_id = obj
        .get("customer")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let subscription_id = obj
        .get("subscription")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    // Tenant ID from metadata (set during checkout creation)
    let tenant_id_str = obj
        .get("metadata")
        .and_then(|m| m.get("tenant_id"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            // Also check subscription_data.metadata
            obj.get("subscription_data")
                .and_then(|sd| sd.get("metadata"))
                .and_then(|m| m.get("tenant_id"))
                .and_then(|v| v.as_str())
        });

    let tenant_id_str = match tenant_id_str {
        Some(s) => s,
        None => {
            warn!("checkout.session.completed: no tenant_id in metadata");
            return;
        }
    };

    let tenant_id = match aegis_orchestrator_core::domain::tenant::TenantId::from_string(
        tenant_id_str,
    ) {
        Ok(id) => id,
        Err(e) => {
            warn!(error = %e, tenant_id = tenant_id_str, "Invalid tenant_id in checkout metadata");
            return;
        }
    };

    let tier_str = obj
        .get("metadata")
        .and_then(|m| m.get("tier"))
        .and_then(|v| v.as_str())
        .unwrap_or("pro");

    let tier = str_to_tier(tier_str);
    let now = chrono::Utc::now();

    let sub = TenantSubscription {
        tenant_id: tenant_id.clone(),
        stripe_customer_id: customer_id.to_string(),
        stripe_subscription_id: Some(subscription_id.to_string()),
        tier: tier.clone(),
        status: SubscriptionStatus::Active,
        current_period_end: None,
        cancel_at_period_end: false,
        created_at: now,
        updated_at: now,
    };

    if let Err(e) = billing_repo.upsert_subscription(&sub).await {
        warn!(error = %e, "Failed to upsert subscription after checkout");
        return;
    }

    // Sync tier to Keycloak
    sync_tier_to_keycloak(state, &tenant_id, &tier).await;

    info!(
        tenant_id = %tenant_id,
        tier = tier_str,
        "Checkout completed — subscription activated"
    );
}

async fn handle_subscription_updated(
    billing_repo: &dyn BillingRepository,
    payload: &serde_json::Value,
) {
    let obj = payload;

    let customer_id = obj
        .get("customer")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let sub = match billing_repo.get_subscription_by_customer(customer_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            warn!(
                customer_id,
                "subscription.updated: no local subscription found"
            );
            return;
        }
        Err(e) => {
            warn!(error = %e, "Failed to look up subscription by customer");
            return;
        }
    };

    let status_str = obj.get("status").and_then(|v| v.as_str()).unwrap_or("none");
    let status = SubscriptionStatus::from_stripe(status_str);

    let period_end = obj
        .get("current_period_end")
        .and_then(|v| v.as_i64())
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0));

    // Determine tier from price metadata or existing
    let tier = extract_tier_from_subscription(obj).unwrap_or(sub.tier.clone());

    if let Err(e) = billing_repo
        .update_tier(&sub.tenant_id, &tier, &status, period_end)
        .await
    {
        warn!(error = %e, "Failed to update subscription tier");
    }

    info!(
        tenant_id = %sub.tenant_id,
        status = status_str,
        "Subscription updated"
    );
}

async fn handle_subscription_deleted(
    state: &AppState,
    billing_repo: &dyn BillingRepository,
    payload: &serde_json::Value,
) {
    let obj = payload;

    let customer_id = obj
        .get("customer")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let sub = match billing_repo.get_subscription_by_customer(customer_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            warn!(error = %e, "Failed to look up subscription for deletion");
            return;
        }
    };

    // Downgrade to Free
    if let Err(e) = billing_repo
        .update_tier(
            &sub.tenant_id,
            &TenantTier::Free,
            &SubscriptionStatus::Canceled,
            None,
        )
        .await
    {
        warn!(error = %e, "Failed to downgrade subscription");
        return;
    }

    sync_tier_to_keycloak(state, &sub.tenant_id, &TenantTier::Free).await;

    info!(
        tenant_id = %sub.tenant_id,
        "Subscription deleted — downgraded to free"
    );
}

async fn handle_payment_failed(billing_repo: &dyn BillingRepository, payload: &serde_json::Value) {
    let obj = payload;

    let customer_id = obj
        .get("customer")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let sub = match billing_repo.get_subscription_by_customer(customer_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            warn!(error = %e, "Failed to look up subscription for payment failure");
            return;
        }
    };

    if let Err(e) = billing_repo
        .update_tier(
            &sub.tenant_id,
            &sub.tier,
            &SubscriptionStatus::PastDue,
            sub.current_period_end,
        )
        .await
    {
        warn!(error = %e, "Failed to mark subscription as past_due");
    }

    info!(
        tenant_id = %sub.tenant_id,
        "Invoice payment failed — marked past_due"
    );
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn tier_to_str(tier: &TenantTier) -> &'static str {
    match tier {
        TenantTier::Free => "free",
        TenantTier::Pro => "pro",
        TenantTier::Business => "business",
        TenantTier::Enterprise => "enterprise",
        TenantTier::System => "system",
    }
}

fn str_to_tier(s: &str) -> TenantTier {
    match s {
        "pro" => TenantTier::Pro,
        "business" => TenantTier::Business,
        "enterprise" => TenantTier::Enterprise,
        "system" => TenantTier::System,
        _ => TenantTier::Free,
    }
}

/// Try to extract the tier from a Stripe subscription object's plan/price metadata.
fn extract_tier_from_subscription(obj: &serde_json::Value) -> Option<TenantTier> {
    // Check metadata first
    obj.get("metadata")
        .and_then(|m| m.get("tier"))
        .and_then(|v| v.as_str())
        .map(str_to_tier)
        .or_else(|| {
            // Check items.data[0].price.metadata.tier
            obj.get("items")
                .and_then(|items| items.get("data"))
                .and_then(|data| data.as_array())
                .and_then(|arr| arr.first())
                .and_then(|item| item.get("price"))
                .and_then(|price| price.get("metadata"))
                .and_then(|m| m.get("tier"))
                .and_then(|v| v.as_str())
                .map(str_to_tier)
        })
}

/// Sync the billing tier to Keycloak's `zaru_tier` user attribute.
async fn sync_tier_to_keycloak(
    state: &AppState,
    tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
    tier: &TenantTier,
) {
    let kc = match &state.keycloak_admin {
        Some(c) => c.clone(),
        None => {
            warn!("Cannot sync tier to Keycloak: admin client not configured");
            return;
        }
    };

    let tier_value = tier_to_str(tier);

    // For consumer tenants, the realm is "zaru-consumer".
    // For enterprise tenants, the realm is "tenant-{slug}".
    let realm = if tenant_id.is_consumer() {
        "zaru-consumer".to_string()
    } else {
        format!("tenant-{}", tenant_id.as_str())
    };

    // List all users in the realm and update their zaru_tier attribute.
    // In practice, consumer tenants have a single user per subscription.
    match kc.list_realm_users(&realm).await {
        Ok(users) => {
            for user in users {
                if let Err(e) = kc
                    .set_user_attribute(&realm, &user.id, "zaru_tier", tier_value)
                    .await
                {
                    warn!(
                        error = %e,
                        user_id = %user.id,
                        "Failed to sync zaru_tier to Keycloak"
                    );
                }
            }
        }
        Err(e) => {
            warn!(error = %e, realm = %realm, "Failed to list users for tier sync");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_name_to_tier_maps_base_plans() {
        assert_eq!(product_name_to_tier("Zaru Pro"), Some(("pro", 3)));
        assert_eq!(product_name_to_tier("Zaru Business"), Some(("business", 5)));
        assert_eq!(
            product_name_to_tier("Zaru Enterprise"),
            Some(("enterprise", 10))
        );
    }

    #[test]
    fn product_name_to_tier_excludes_seat_addons() {
        assert_eq!(product_name_to_tier("Zaru Pro - Extra Seat"), None);
        assert_eq!(product_name_to_tier("Zaru Business - Extra Seat"), None);
    }

    #[test]
    fn product_name_to_tier_excludes_non_zaru() {
        assert_eq!(product_name_to_tier("Some Other Product"), None);
    }

    #[test]
    fn seat_product_tier_maps_correctly() {
        assert_eq!(seat_product_tier("Zaru Pro - Extra Seat"), Some("pro"));
        assert_eq!(
            seat_product_tier("Zaru Business - Extra Seat"),
            Some("business")
        );
        assert_eq!(
            seat_product_tier("Zaru Enterprise - Extra Seat"),
            Some("enterprise")
        );
    }

    #[test]
    fn seat_product_tier_rejects_non_seat_products() {
        assert_eq!(seat_product_tier("Zaru Pro"), None);
        assert_eq!(seat_product_tier("Zaru Business"), None);
    }

    #[test]
    fn str_to_tier_mapping() {
        assert_eq!(str_to_tier("pro"), TenantTier::Pro);
        assert_eq!(str_to_tier("business"), TenantTier::Business);
        assert_eq!(str_to_tier("enterprise"), TenantTier::Enterprise);
        assert_eq!(str_to_tier("system"), TenantTier::System);
        assert_eq!(str_to_tier("unknown"), TenantTier::Free);
    }

    #[test]
    fn tier_to_str_mapping() {
        assert_eq!(tier_to_str(&TenantTier::Free), "free");
        assert_eq!(tier_to_str(&TenantTier::Pro), "pro");
        assert_eq!(tier_to_str(&TenantTier::Business), "business");
        assert_eq!(tier_to_str(&TenantTier::Enterprise), "enterprise");
        assert_eq!(tier_to_str(&TenantTier::System), "system");
    }

    #[test]
    fn extract_tier_from_metadata() {
        let obj = serde_json::json!({
            "metadata": {"tier": "business"}
        });
        assert_eq!(
            extract_tier_from_subscription(&obj),
            Some(TenantTier::Business)
        );
    }

    #[test]
    fn extract_tier_from_price_metadata() {
        let obj = serde_json::json!({
            "items": {
                "data": [{
                    "price": {
                        "metadata": {"tier": "pro"}
                    }
                }]
            }
        });
        assert_eq!(extract_tier_from_subscription(&obj), Some(TenantTier::Pro));
    }

    #[test]
    fn extract_tier_returns_none_when_absent() {
        let obj = serde_json::json!({});
        assert_eq!(extract_tier_from_subscription(&obj), None);
    }
}
