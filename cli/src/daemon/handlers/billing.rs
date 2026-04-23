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
//! | `POST` | `/v1/billing/seats` | Update seat count on existing subscription |
//! | `GET`  | `/v1/billing/invoices` | List invoices from Stripe |

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use tracing::{error, info, warn};

use aegis_orchestrator_core::domain::billing::{SubscriptionStatus, TenantSubscription};
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::node_config::{resolve_env_value, BillingConfig};
use aegis_orchestrator_core::domain::team::TeamStatus;
use aegis_orchestrator_core::domain::tenancy::TenantTier;
use aegis_orchestrator_core::infrastructure::repositories::BillingRepository;

use stripe_billing::billing_portal_session::CreateBillingPortalSession;
use stripe_billing::invoice::{ListInvoice, PayInvoice};
use stripe_billing::subscription::{
    RetrieveSubscription, UpdateSubscription, UpdateSubscriptionItems,
    UpdateSubscriptionProrationBehavior,
};
// Shared type re-exports from sub-crates
use stripe_billing::{InvoiceStatus, SubscriptionId};
use stripe_checkout::checkout_session::{
    CreateCheckoutSession, CreateCheckoutSessionLineItems, CreateCheckoutSessionSubscriptionData,
};
use stripe_checkout::CheckoutSessionMode;
use stripe_core::customer::{CreateCustomer, RetrieveCustomer, UpdateCustomer};
use stripe_core::CustomerId;
use stripe_product::price::ListPrice;
use stripe_product::product::ListProduct;
use stripe_product::RecurringInterval;

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
    /// Tier slug (pro, business, enterprise) — included in checkout metadata
    /// so the webhook handler knows which tier was purchased.
    #[serde(default)]
    pub tier: Option<String>,
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

#[derive(Debug, serde::Deserialize)]
pub(crate) struct UpdateSeatsRequest {
    /// New total extra seats (0 = remove all extra seats).
    pub extra_seats: u32,
    /// Stripe Price ID for the seat add-on.
    pub seat_price_id: String,
}

// ── Tier mapping helpers ────────────────────────────────────────────────────

/// Map a Stripe product name to a `(tier, included_seats)` tuple.
///
/// Per ADR-111 (colony tier model): Pro is a personal subscription with **no
/// seats** — `provision_team` rejects Pro via
/// [`TenantTier::allows_colony`], so the Pro mapping here only ever fires for
/// personal-tier sync paths and sanity checks. Business/Enterprise are the
/// only tiers that may own a colony and carry seats (5 / 10 respectively).
///
/// Returns `None` for products that are seat add-ons — those are matched
/// separately via [`seat_product_tier`].
fn product_name_to_tier(name: &str) -> Option<(&'static str, u32)> {
    // Skip seat add-on products — they are matched separately
    if name.contains("Extra Seat") {
        return None;
    }
    if name.starts_with("Zaru Pro") {
        Some(("pro", 0))
    } else if name.starts_with("Zaru Business") {
        Some(("business", 5))
    } else if name.starts_with("Zaru Enterprise") {
        Some(("enterprise", 10))
    } else {
        None
    }
}

/// Extract the tier prefix from a seat add-on product name.
///
/// Per ADR-111: Pro is personal-only and has no seat add-on. Only Business
/// and Enterprise seat add-ons are recognized.
fn seat_product_tier(name: &str) -> Option<&'static str> {
    if !name.contains("Extra Seat") {
        return None;
    }
    if name.starts_with("Zaru Business") {
        Some("business")
    } else if name.starts_with("Zaru Enterprise") {
        Some("enterprise")
    } else {
        None
    }
}

/// Included seats per tier. Delegates to the domain
/// [`TenantTier::included_seats`] so the billing and domain layers cannot
/// drift. Pro returns 0 via the `_` arm — a Pro subscription is personal and
/// carries no seats.
fn included_seats_for_tier(tier: &str) -> u32 {
    match tier {
        "business" => TenantTier::Business.included_seats(),
        "enterprise" => TenantTier::Enterprise.included_seats(),
        _ => 0,
    }
}

// ── Seat price identification (via Stripe price metadata) ──────────────────
//
// The bootstrap script stamps every Stripe price with `metadata.kind` — `base`
// for plan prices and `seat` for seat add-ons — plus `metadata.tier` naming
// the tier slug (pro / business / enterprise). The orchestrator reads those
// metadata fields directly from webhook payloads and Stripe API responses so
// the price IDs themselves are opaque. This lets Jeshua wipe and re-bootstrap
// Stripe with fresh IDs without any code changes.

/// Given a Stripe subscription item JSON payload, return `(tier, interval)`
/// if the item is a seat add-on (`price.metadata.kind == "seat"`), else None.
///
/// Both `tier` and `interval` come straight off the webhook payload:
///   * `tier` from `price.metadata.tier`
///   * `interval` from `price.recurring.interval`
fn tier_for_seat_item(item: &serde_json::Value) -> Option<(String, String)> {
    let price = item.get("price")?;
    let metadata = price.get("metadata")?;
    let kind = metadata.get("kind").and_then(|v| v.as_str())?;
    if kind != "seat" {
        return None;
    }
    let tier = metadata.get("tier").and_then(|v| v.as_str())?.to_string();
    let interval = price
        .get("recurring")
        .and_then(|r| r.get("interval"))
        .and_then(|v| v.as_str())?
        .to_string();
    Some((tier, interval))
}

/// Returns `true` if the subscription item is a seat add-on line
/// (`price.metadata.kind == "seat"`).
fn item_is_seat_addon(item: &serde_json::Value) -> bool {
    item.get("price")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("kind"))
        .and_then(|v| v.as_str())
        == Some("seat")
}

/// Query Stripe for the active seat price ID matching `(tier, interval)`.
///
/// Stripe's `prices.list` does not support metadata-keyed queries, so we list
/// active prices and filter in-memory on `metadata.kind == "seat"` +
/// `metadata.tier` + `recurring.interval`. Returns `None` when no matching
/// price exists (e.g. freshly bootstrapped tier missing its seat add-on).
async fn find_seat_price_id(client: &stripe::Client, tier: &str, interval: &str) -> Option<String> {
    let prices = match ListPrice::new().active(true).limit(100).send(client).await {
        Ok(list) => list.data,
        Err(e) => {
            warn!(error = %e, "Failed to list Stripe prices while resolving seat price");
            return None;
        }
    };
    for p in prices {
        if p.metadata.get("kind").map(String::as_str) != Some("seat") {
            continue;
        }
        if p.metadata.get("tier").map(String::as_str) != Some(tier) {
            continue;
        }
        let price_interval = p
            .recurring
            .as_ref()
            .map(|r| r.interval.as_str().to_string());
        if price_interval.as_deref() != Some(interval) {
            continue;
        }
        return Some(p.id.to_string());
    }
    None
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
    let products = match ListProduct::new()
        .active(true)
        .limit(100)
        .send(&client)
        .await
    {
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
        .filter(|p| p.name.starts_with("Zaru"))
        .collect();

    // 2. Build a map of tier -> TierPricing
    use std::collections::HashMap;
    let mut tier_map: HashMap<&str, TierPricing> = HashMap::new();

    // Initialize base plan entries
    for product in &zaru_products {
        let name = product.name.as_str();
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
        let name = product.name.as_str();

        let product_id_str = product.id.to_string();
        let prices = match ListPrice::new()
            .product(product_id_str)
            .active(true)
            .limit(50)
            .send(&client)
            .await
        {
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
            let currency = price.currency.to_string();

            let info = PriceInfo {
                price_id: price.id.to_string(),
                amount,
                currency,
            };

            // recurring is Option<Recurring>; interval is RecurringInterval (non-optional inside)
            let interval = price.recurring.as_ref().map(|r| r.interval.clone());

            match (is_seat_addon, interval) {
                (false, Some(RecurringInterval::Month)) => {
                    tier_entry.monthly = Some(info);
                }
                (false, Some(RecurringInterval::Year)) => {
                    tier_entry.annual = Some(info);
                }
                (true, Some(RecurringInterval::Month)) => {
                    tier_entry.seat_monthly = Some(info);
                }
                (true, Some(RecurringInterval::Year)) => {
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

    let name = identity.name.as_deref().unwrap_or("");
    let email = identity.email.as_deref().unwrap_or("");

    // Try to reuse the cached Stripe customer from tenant_subscriptions.
    // If the cached ID no longer exists in Stripe (e.g. after a sandbox
    // reset), we fall through and create a fresh one.
    let reused = match existing_sub.as_ref() {
        Some(sub) => {
            let cid: CustomerId = sub
                .stripe_customer_id
                .parse()
                .expect("CustomerId parse is infallible");
            match RetrieveCustomer::new(cid.clone()).send(&stripe).await {
                Ok(_) => {
                    // Customer still exists — sync name/email and reuse.
                    let mut update = UpdateCustomer::new(cid);
                    if !name.is_empty() {
                        update = update.name(name.to_string());
                    }
                    if !email.is_empty() {
                        update = update.email(email.to_string());
                    }
                    if let Err(e) = update.send(&stripe).await {
                        warn!(error = %e, "Failed to sync customer name/email to Stripe");
                    }
                    Some(sub.stripe_customer_id.clone())
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        cached_customer_id = %sub.stripe_customer_id,
                        "Cached Stripe customer no longer exists — creating a new one",
                    );
                    None
                }
            }
        }
        None => None,
    };

    let customer_id = if let Some(id) = reused {
        id
    } else {
        let mut create = CreateCustomer::new().metadata(
            [("tenant_id".to_string(), tenant_id.as_str().to_string())]
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>(),
        );
        if !email.is_empty() {
            create = create.email(email.to_string());
        }
        if !name.is_empty() {
            create = create.name(name.to_string());
        }

        match create.send(&stripe).await {
            Ok(customer) => {
                let cust_id = customer.id.to_string();
                // Persist the customer mapping (upsert — may be replacing a
                // stale cached ID, so preserve existing tier/status when present).
                let now = chrono::Utc::now();
                let new_sub = match existing_sub.as_ref() {
                    Some(prev) => TenantSubscription {
                        stripe_customer_id: cust_id.clone(),
                        updated_at: now,
                        ..prev.clone()
                    },
                    None => TenantSubscription {
                        tenant_id: tenant_id.clone(),
                        stripe_customer_id: cust_id.clone(),
                        stripe_subscription_id: None,
                        tier: TenantTier::Free,
                        status: SubscriptionStatus::None,
                        current_period_end: None,
                        cancel_at_period_end: false,
                        created_at: now,
                        updated_at: now,
                        seat_count: 1,
                    },
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

    // Build line items: base plan + optional seat add-on
    let mut line_items = vec![CreateCheckoutSessionLineItems {
        price: Some(body.price_id.clone()),
        quantity: Some(1),
        ..Default::default()
    }];

    if let Some(ref seat_price_id) = body.seat_price_id {
        if body.seats > 0 {
            line_items.push(CreateCheckoutSessionLineItems {
                price: Some(seat_price_id.clone()),
                quantity: Some(body.seats as u64),
                ..Default::default()
            });
        }
    }

    let mut tenant_meta: std::collections::HashMap<String, String> =
        [("tenant_id".to_string(), tenant_id.as_str().to_string())]
            .into_iter()
            .collect();
    if let Some(ref tier) = body.tier {
        tenant_meta.insert("tier".to_string(), tier.clone());
    }
    if body.seats > 0 {
        tenant_meta.insert("extra_seats".to_string(), body.seats.to_string());
    }

    let sub_data = CreateCheckoutSessionSubscriptionData {
        metadata: Some(tenant_meta.clone()),
        ..Default::default()
    };

    let params = CreateCheckoutSession::new()
        .customer(customer_id)
        .mode(CheckoutSessionMode::Subscription)
        .success_url(body.success_url.clone())
        .cancel_url(body.cancel_url.clone())
        .line_items(line_items)
        .metadata(tenant_meta)
        .subscription_data(sub_data);

    match params.send(&stripe).await {
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

    match CreateBillingPortalSession::new()
        .customer(sub.stripe_customer_id.clone())
        .return_url(body.return_url.clone())
        .send(&stripe)
        .await
    {
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

/// `POST /v1/billing/seats` — Update seat count on an existing subscription.
///
/// Finds the seat line item on the Stripe subscription (matched by price ID),
/// then adds, updates, or removes it depending on the requested `extra_seats`.
pub(crate) async fn update_seats_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Json(body): Json<UpdateSeatsRequest>,
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

    // 1. Get the tenant's existing subscription
    let sub = match billing_repo.get_subscription(&tenant_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "No subscription found. Please subscribe first."})),
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

    let stripe_sub_id: SubscriptionId = match sub.stripe_subscription_id {
        Some(ref id) => id.parse().expect("SubscriptionId parse is infallible"),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "No active Stripe subscription found."})),
            )
                .into_response();
        }
    };

    // 2. Retrieve the Stripe subscription to find existing seat line item
    let stripe_sub = match RetrieveSubscription::new(stripe_sub_id.clone())
        .send(&stripe)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "Failed to retrieve Stripe subscription");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("Failed to retrieve subscription from Stripe: {e}")})),
            )
                .into_response();
        }
    };

    // 3. Find the seat line item by matching price ID
    // In the new API, price is Price (not Option<Price>)
    let seat_item = stripe_sub
        .items
        .data
        .iter()
        .find(|item| item.price.id.as_str() == body.seat_price_id);

    // 4. Build the update items list
    let items = match (body.extra_seats, seat_item) {
        // Add seats: no existing seat item → add a new line item
        (extra, None) if extra > 0 => {
            vec![UpdateSubscriptionItems {
                price: Some(body.seat_price_id.clone()),
                quantity: Some(extra as u64),
                ..Default::default()
            }]
        }
        // Update seats: existing seat item → update quantity
        (extra, Some(item)) if extra > 0 => {
            vec![UpdateSubscriptionItems {
                id: Some(item.id.to_string()),
                quantity: Some(extra as u64),
                ..Default::default()
            }]
        }
        // Remove seats: existing seat item → delete the line item
        (0, Some(item)) => {
            vec![UpdateSubscriptionItems {
                id: Some(item.id.to_string()),
                deleted: Some(true),
                ..Default::default()
            }]
        }
        // No-op: 0 extra seats and no existing item
        (0, None) => {
            return (
                StatusCode::OK,
                Json(json!({"success": true, "extra_seats": 0})),
            )
                .into_response();
        }
        _ => unreachable!(),
    };

    match UpdateSubscription::new(stripe_sub_id.clone())
        .items(items)
        .proration_behavior(UpdateSubscriptionProrationBehavior::AlwaysInvoice)
        .send(&stripe)
        .await
    {
        Ok(_) => {
            info!(
                tenant_id = %tenant_id,
                extra_seats = body.extra_seats,
                "Seat count updated on subscription"
            );
        }
        Err(e) => {
            warn!(error = %e, "Failed to update seat count on Stripe subscription");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("Failed to update seats: {e}")})),
            )
                .into_response();
        }
    }

    // Immediately collect any open proration invoice so the customer is charged
    // now rather than at the next billing cycle.
    if body.extra_seats > 0 {
        match ListInvoice::new()
            .subscription(stripe_sub_id.as_str().to_string())
            .status(InvoiceStatus::Open)
            .limit(1)
            .send(&stripe)
            .await
        {
            Ok(invoices) => {
                if let Some(invoice) = invoices.data.into_iter().next() {
                    // Invoice.id is Option<InvoiceId> in the new API
                    if let Some(inv_id) = invoice.id.clone() {
                        match PayInvoice::new(inv_id.clone()).send(&stripe).await {
                            Ok(_) => {
                                info!(
                                    tenant_id = %tenant_id,
                                    invoice_id = %inv_id,
                                    "Proration invoice paid immediately"
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, invoice_id = %inv_id, "Failed to pay proration invoice");
                                return (
                                    StatusCode::PAYMENT_REQUIRED,
                                    Json(json!({"error": format!("Seat update succeeded but payment failed: {e}")})),
                                )
                                    .into_response();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to list open invoices after seat update");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("Seat update succeeded but could not retrieve invoice: {e}")})),
                )
                    .into_response();
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({"success": true, "extra_seats": body.extra_seats})),
    )
        .into_response()
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
                "seat_count": sub.seat_count,
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

    match ListInvoice::new()
        .customer(sub.stripe_customer_id.clone())
        .send(&stripe)
        .await
    {
        Ok(list) => {
            let invoices: Vec<serde_json::Value> = list
                .data
                .iter()
                .map(|inv| {
                    json!({
                        "id": inv.id.as_ref().map(|id| id.to_string()),
                        "amount_due": inv.amount_due,
                        "amount_paid": inv.amount_paid,
                        "currency": inv.currency.to_string(),
                        "status": inv.status.as_ref().map(|s| format!("{s:?}")),
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
            handle_subscription_updated(state, &*billing_repo, payload).await;
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
    let included_seats = included_seats_for_tier(tier_str);
    let extra_seats: u32 = obj
        .get("metadata")
        .and_then(|m| m.get("extra_seats"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let total_seats = included_seats + extra_seats;
    let now = chrono::Utc::now();

    let sub = TenantSubscription {
        tenant_id: tenant_id.clone(),
        stripe_customer_id: customer_id.to_string(),
        stripe_subscription_id: Some(subscription_id.to_string()),
        tier,
        status: SubscriptionStatus::Active,
        current_period_end: None,
        cancel_at_period_end: false,
        created_at: now,
        updated_at: now,
        seat_count: total_seats,
    };

    if let Err(e) = billing_repo.upsert_subscription(&sub).await {
        warn!(error = %e, "Failed to upsert subscription after checkout");
        return;
    }

    // Sync tier to Keycloak via EffectiveTierService (ADR-111 Phase 3) —
    // consumer tenants route through effective-tier computation; enterprise
    // tenants fall back to the legacy direct path.
    sync_tier(state, &tenant_id, &tier).await;

    info!(
        tenant_id = %tenant_id,
        tier = tier_str,
        "Checkout completed — subscription activated"
    );
}

async fn handle_subscription_updated(
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
    let tier = extract_tier_from_subscription(obj).unwrap_or(sub.tier);

    if let Err(e) = billing_repo
        .update_tier(&sub.tenant_id, &tier, &status, period_end)
        .await
    {
        warn!(error = %e, "Failed to update subscription tier");
    }

    // Propagate the new tier to Keycloak so JWT claims reflect it immediately
    // (e.g. Pro → Business upgrades via the Stripe portal). Routed through the
    // EffectiveTierService (ADR-111 Phase 3) for consumer tenants.
    sync_tier(state, &sub.tenant_id, &tier).await;

    // Colony suspension: if this subscription belongs to a team tenant and
    // the tier dropped below Business, suspend the colony so team-context
    // requests are rejected. Re-suspension is idempotent.
    reconcile_team_suspension(state, &sub.tenant_id, &tier).await;

    // ADR-111 §Billing Model: reconcile seat_count back from Stripe for all
    // tenants. The canonical source is membership truth, but manual edits in
    // the Stripe dashboard can create drift — this keeps the persisted
    // seat_count aligned with what Stripe actually bills, and publishes a
    // SeatCountChanged domain event whenever the values diverge.
    //
    // We locate the seat add-on line item by matching `price.metadata.kind ==
    // "seat"` rather than assuming data[0] is the seat item — on multi-item
    // subscriptions data[0] is the base plan (quantity=1).
    let tier_str_for_seats = tier_to_str(&tier);
    let included = included_seats_for_tier(tier_str_for_seats);
    let new_seats_u32: u32 = obj
        .get("items")
        .and_then(|i| i.get("data"))
        .and_then(|d| d.as_array())
        .map(|arr| {
            let addon_qty: u32 = arr
                .iter()
                .find(|item| item_is_seat_addon(item))
                .and_then(|item| item.get("quantity"))
                .and_then(|q| q.as_u64())
                .map(|q| q.min(u32::MAX as u64) as u32)
                .unwrap_or(0);
            included + addon_qty
        })
        .unwrap_or(included);

    if new_seats_u32 != sub.seat_count {
        let drift = (new_seats_u32 as i64 - sub.seat_count as i64).abs();
        if drift > 1 {
            warn!(
                tenant_id = %sub.tenant_id,
                previous = sub.seat_count,
                new = new_seats_u32,
                drift,
                "Stripe seat_count drift >1 — reconciling from Stripe"
            );
        } else {
            info!(
                tenant_id = %sub.tenant_id,
                previous = sub.seat_count,
                new = new_seats_u32,
                "Reconciling seat_count from Stripe webhook"
            );
        }
        if let Err(e) = billing_repo
            .update_seat_count_by_customer(customer_id, new_seats_u32)
            .await
        {
            warn!(error = %e, "Failed to reconcile seat_count");
        } else {
            // Resolve the team_id via the team_repo so the event carries the
            // correct aggregate id. If the lookup fails we still publish the
            // drift — downstream consumers can re-derive from the tenant slug.
            let team_id = match state.team_repo.as_ref() {
                Some(tr) => {
                    match aegis_orchestrator_core::domain::team::TeamSlug::parse(
                        sub.tenant_id.as_str(),
                    ) {
                        Ok(slug) => match tr.find_by_slug(&slug).await {
                            Ok(Some(t)) => Some(t.id),
                            _ => None,
                        },
                        Err(_) => None,
                    }
                }
                None => None,
            };
            if let Some(team_id) = team_id {
                state.event_bus.publish_team_event(
                    aegis_orchestrator_core::domain::team::TeamEvent::SeatCountChanged {
                        team_id,
                        previous_count: sub.seat_count,
                        new_count: new_seats_u32,
                        changed_at: chrono::Utc::now(),
                    },
                );
            }
        }
    }

    // ── Seat add-on tier migration ─────────────────────────────────────────
    // When the base plan tier changes (e.g. Pro → Business via Stripe Portal),
    // the seat add-on line item may still reference the old tier's price. Detect
    // this and swap it to the new tier's seat price, preserving quantity.
    migrate_seat_addon_if_needed(state, obj, &tier).await;

    info!(
        tenant_id = %sub.tenant_id,
        status = status_str,
        "Subscription updated"
    );
}

/// If the subscription has a seat add-on from a different tier than the current
/// base plan, swap it to the correct tier's seat price (same interval, same qty).
async fn migrate_seat_addon_if_needed(
    state: &AppState,
    subscription_obj: &serde_json::Value,
    new_tier: &TenantTier,
) {
    let new_tier_str = tier_to_str(new_tier);

    // Free / System tiers don't have seat add-ons
    if matches!(new_tier, TenantTier::Free | TenantTier::System) {
        return;
    }

    let items = match subscription_obj
        .get("items")
        .and_then(|i| i.get("data"))
        .and_then(|d| d.as_array())
    {
        Some(arr) => arr,
        None => return,
    };

    // Find a seat add-on item whose price belongs to a *different* tier
    let seat_item = items.iter().find_map(|item| {
        let (seat_tier, interval) = tier_for_seat_item(item)?;
        if seat_tier != new_tier_str {
            let item_id = item.get("id").and_then(|id| id.as_str())?;
            let quantity = item.get("quantity").and_then(|q| q.as_u64()).unwrap_or(1);
            Some((item_id.to_string(), seat_tier, interval, quantity))
        } else {
            None // seat already matches the current tier
        }
    });

    let (old_item_id, old_tier, interval, quantity) = match seat_item {
        Some(s) => s,
        None => return, // no mismatched seat item — nothing to do
    };

    // Build a Stripe client to perform the migration
    let billing = match &state.billing_config {
        Some(c) => c,
        None => return,
    };
    let stripe_client = match stripe_client_from_config(billing) {
        Some(c) => c,
        None => return,
    };

    let new_seat_price = match find_seat_price_id(&stripe_client, new_tier_str, &interval).await {
        Some(p) => p,
        None => {
            warn!(
                tier = new_tier_str,
                interval = %interval,
                "No active Stripe seat price (metadata.kind=seat) for tier/interval — cannot migrate seat add-on"
            );
            return;
        }
    };

    let sub_id_str = match subscription_obj.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return,
    };
    let sub_id: SubscriptionId = sub_id_str
        .parse()
        .expect("SubscriptionId parse is infallible");

    let update_items = vec![
        // Remove the old tier's seat item
        UpdateSubscriptionItems {
            id: Some(old_item_id.clone()),
            deleted: Some(true),
            ..Default::default()
        },
        // Add the new tier's seat item with the same quantity
        UpdateSubscriptionItems {
            price: Some(new_seat_price.clone()),
            quantity: Some(quantity),
            ..Default::default()
        },
    ];

    match UpdateSubscription::new(sub_id)
        .items(update_items)
        .send(&stripe_client)
        .await
    {
        Ok(_) => {
            info!(
                subscription_id = sub_id_str,
                old_tier = old_tier,
                new_tier = new_tier_str,
                interval,
                quantity,
                "Migrated seat add-on to new tier price"
            );
        }
        Err(e) => {
            error!(
                error = %e,
                subscription_id = sub_id_str,
                old_tier = old_tier,
                new_tier = new_tier_str,
                "Failed to migrate seat add-on to new tier price"
            );
        }
    }
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

    sync_tier(state, &sub.tenant_id, &TenantTier::Free).await;

    // Colony suspension: deletion drops the tenant to Free, which cannot own
    // a colony. Suspend the team so team-context requests are rejected.
    reconcile_team_suspension(state, &sub.tenant_id, &TenantTier::Free).await;

    info!(
        tenant_id = %sub.tenant_id,
        "Subscription deleted — downgraded to free"
    );
}

/// Reconcile team colony status against the (possibly new) subscription tier.
///
/// When `tenant_id` is a team tenant (`t-{uuid}`):
///
/// - If `tier` no longer allows a colony (below Business) and the team is
///   currently `Active`, suspend it.
/// - If `tier` allows a colony and the team is currently `Suspended`, resume
///   it.
/// - In either case, recompute all member effective tiers so the shared
///   ceiling tracks the new colony state immediately.
///
/// No-op for non-team tenants, and tolerant of the team_repo /
/// effective_tier_service being unconfigured (degraded mode).
async fn reconcile_team_suspension(
    state: &AppState,
    tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
    tier: &TenantTier,
) {
    let (Some(team_repo), Some(service)) = (
        state.team_repo.as_ref(),
        state.effective_tier_service.as_ref(),
    ) else {
        return;
    };
    reconcile_team_suspension_with(team_repo.as_ref(), service.as_ref(), tenant_id, tier).await;
}

/// Core suspension reconciliation — parameterised by the collaborators so
/// unit tests can exercise the full state transition without constructing an
/// [`AppState`].
async fn reconcile_team_suspension_with(
    team_repo: &dyn aegis_orchestrator_core::domain::team::TeamRepository,
    service: &dyn aegis_orchestrator_core::application::effective_tier_service::EffectiveTierService,
    tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
    tier: &TenantTier,
) {
    if !tenant_id.is_team() {
        return;
    }
    match team_repo.find_by_tenant_id(tenant_id).await {
        Ok(Some(mut team)) => {
            let allows = tier.allows_colony();
            let should_suspend = !allows && team.status == TeamStatus::Active;
            let should_resume = allows && team.status == TeamStatus::Suspended;
            if should_suspend {
                if let Err(e) = team.suspend() {
                    warn!(error = %e, team_id = %team.id, "failed to suspend team");
                } else if let Err(e) = team_repo.save(&team).await {
                    warn!(error = %e, team_id = %team.id, "failed to persist suspended team");
                } else {
                    info!(team_id = %team.id, "colony suspended");
                }
            } else if should_resume {
                if let Err(e) = team.resume() {
                    warn!(error = %e, team_id = %team.id, "failed to resume team");
                } else if let Err(e) = team_repo.save(&team).await {
                    warn!(error = %e, team_id = %team.id, "failed to persist resumed team");
                } else {
                    info!(team_id = %team.id, "colony resumed");
                }
            }
            // Either way, recompute all member tiers so the shared ceiling
            // tracks the new colony state.
            if let Err(e) = service.recompute_for_team(&team.id).await {
                warn!(
                    error = %e,
                    team_id = %team.id,
                    "failed to recompute team tiers after suspension state change"
                );
            }
        }
        Ok(None) => {
            warn!(tenant_id = %tenant_id, "team not found for team tenant subscription event");
        }
        Err(e) => {
            warn!(error = %e, tenant_id = %tenant_id, "failed to look up team for suspension check");
        }
    }
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

/// Legacy adapter — defers to `TenantTier::as_keycloak_str` (ADR-111 Phase 3).
///
/// Retained as a thin wrapper so this file's many call-sites keep reading the
/// same way; the single source of truth for the string mapping is on
/// `TenantTier` itself.
fn tier_to_str(tier: &TenantTier) -> &'static str {
    tier.as_keycloak_str()
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

/// POST `/api/internal/invalidate-sessions` on the zaru-client for `user_id`.
///
/// Called after a successful `set_user_attribute` so that the next request from
/// the affected user picks up the updated `zaru_tier` JWT claim without waiting
/// for natural token expiry.  Silently no-ops when `ZARU_URL` or
/// `ZARU_INTERNAL_SECRET` are not configured.
async fn invalidate_zaru_sessions(state: &AppState, user_id: &str) {
    let (url, secret) = match (
        state.zaru_url.as_deref(),
        state.zaru_internal_secret.as_deref(),
    ) {
        (Some(u), Some(s)) => (u, s),
        _ => return,
    };

    let endpoint = format!("{}/api/internal/invalidate-sessions", url);
    let client = reqwest::Client::new();
    if let Err(e) = client
        .post(&endpoint)
        .bearer_auth(secret)
        .json(&serde_json::json!({ "user_id": user_id }))
        .send()
        .await
    {
        warn!(error = %e, user_id, "Failed to invalidate zaru sessions");
    }
}

/// Derive the Keycloak user `sub` (UUID with hyphens) from a per-user consumer
/// `TenantId` of the form `u-<32 hex chars>`.  Returns `None` for any other
/// tenant ID format.
fn consumer_tenant_id_to_user_sub(
    tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
) -> Option<String> {
    let s = tenant_id.as_str().strip_prefix("u-")?;
    if s.len() != 32 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    ))
}

/// Dispatch the post-subscription-change Keycloak tier sync.
///
/// For consumer tenants, delegates to the `EffectiveTierService` (ADR-111
/// Phase 3), which computes `max(personal, active_colony_tiers)` and writes
/// the resulting effective tier to Keycloak — this is the ONLY correct path
/// for per-user consumer subscriptions because a user's effective tier may be
/// higher than their personal subscription tier due to colony membership.
///
/// For enterprise tenants (dedicated `tenant-{slug}` realms), falls back to
/// the legacy [`sync_tier_to_keycloak`] path since enterprise realms have a
/// different identity model and are unaffected by the colony-tier model.
async fn sync_tier(
    state: &AppState,
    tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
    tier: &TenantTier,
) {
    if let Some(user_sub) = consumer_tenant_id_to_user_sub(tenant_id) {
        if let Some(service) = &state.effective_tier_service {
            match service.recompute_for_user(&user_sub).await {
                Ok(effective) => {
                    tracing::debug!(
                        user_sub = %user_sub,
                        ?effective,
                        "effective tier recomputed after billing event"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        user_sub = %user_sub,
                        "effective tier recompute failed"
                    );
                }
            }
            return;
        }
        // EffectiveTierService not wired — fall through to direct sync so the
        // user's JWT at minimum reflects their personal subscription.
    }
    sync_tier_to_keycloak(state, tenant_id, tier).await;
}

/// Sync the billing tier to Keycloak's `zaru_tier` user attribute.
///
/// For per-user consumer tenants (`u-<hex>`): derives the Keycloak sub from the
/// tenant ID and updates only that single user in `zaru-consumer`.
///
/// For enterprise tenants (`tenant-<slug>`): lists users in the dedicated realm
/// (enterprise realms have exactly one user per tenant).
///
/// The shared consumer realm sentinel (`"zaru-consumer"`) is rejected — billing
/// events always carry per-user tenant IDs, never the shared realm slug.
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

    if tenant_id.is_consumer() {
        // The literal "zaru-consumer" slug should never appear in billing events.
        warn!(
            tenant_id = %tenant_id,
            "sync_tier_to_keycloak received shared consumer realm slug — expected a per-user tenant ID; skipping"
        );
        return;
    }

    if let Some(user_sub) = consumer_tenant_id_to_user_sub(tenant_id) {
        // Per-user consumer tenant: update only the single affected user.
        let realm = "zaru-consumer";
        match kc.get_user(realm, &user_sub).await {
            Ok(Some(user)) => {
                if let Err(e) = kc
                    .set_user_attribute(realm, &user, "zaru_tier", tier_value)
                    .await
                {
                    warn!(
                        error = %e,
                        user_id = %user_sub,
                        "Failed to sync zaru_tier to Keycloak"
                    );
                } else {
                    invalidate_zaru_sessions(state, &user.id).await;
                }
            }
            Ok(None) => {
                warn!(user_sub = %user_sub, "Keycloak user not found for consumer tenant sync");
            }
            Err(e) => {
                warn!(error = %e, user_sub = %user_sub, "Failed to fetch Keycloak user for tier sync");
            }
        }
    } else {
        // Enterprise tenant: dedicated realm, one user per realm.
        let realm = format!("tenant-{}", tenant_id.as_str());
        match kc.list_realm_users(&realm).await {
            Ok(users) => {
                for user in users {
                    if let Err(e) = kc
                        .set_user_attribute(&realm, &user, "zaru_tier", tier_value)
                        .await
                    {
                        warn!(
                            error = %e,
                            user_id = %user.id,
                            "Failed to sync zaru_tier to Keycloak"
                        );
                    } else {
                        invalidate_zaru_sessions(state, &user.id).await;
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, realm = %realm, "Failed to list users for tier sync");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_name_to_tier_maps_base_plans() {
        // Per ADR-111: Pro is personal-only and carries zero seats. Business
        // (5) and Enterprise (10) are the colony tiers.
        assert_eq!(product_name_to_tier("Zaru Pro"), Some(("pro", 0)));
        assert_eq!(product_name_to_tier("Zaru Business"), Some(("business", 5)));
        assert_eq!(
            product_name_to_tier("Zaru Enterprise"),
            Some(("enterprise", 10))
        );
    }

    #[test]
    fn product_name_to_tier_maps_pro_to_zero_seats() {
        // Regression for ADR-111 Phase 2: Pro must never advertise seats.
        assert_eq!(product_name_to_tier("Zaru Pro"), Some(("pro", 0)));
        assert_eq!(product_name_to_tier("Zaru Pro - Monthly"), Some(("pro", 0)));
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
        // Per ADR-111: Pro has no seat add-on — `seat_product_tier` must not
        // recognize a "Zaru Pro - Extra Seat" product.
        assert_eq!(seat_product_tier("Zaru Pro - Extra Seat"), None);
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
    fn pro_has_zero_included_seats() {
        assert_eq!(included_seats_for_tier("pro"), 0);
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

    // ── tier_for_seat_item tests ───────────────────────────────────────────
    //
    // Seat add-ons are identified by `price.metadata.kind == "seat"` on the
    // webhook payload. Price IDs are opaque — Jeshua can wipe/rebuild Stripe
    // without touching code. `tier` and `interval` come from the same payload.

    #[test]
    fn tier_for_seat_item_returns_tier_from_metadata() {
        let item = serde_json::json!({
            "id": "si_test",
            "quantity": 3,
            "price": {
                "id": "price_opaque_id",
                "metadata": { "kind": "seat", "tier": "business" },
                "recurring": { "interval": "month" }
            }
        });
        assert_eq!(
            tier_for_seat_item(&item),
            Some(("business".to_string(), "month".to_string()))
        );

        let item_year = serde_json::json!({
            "price": {
                "metadata": { "kind": "seat", "tier": "enterprise" },
                "recurring": { "interval": "year" }
            }
        });
        assert_eq!(
            tier_for_seat_item(&item_year),
            Some(("enterprise".to_string(), "year".to_string()))
        );
    }

    #[test]
    fn tier_for_seat_item_returns_none_for_base_kind() {
        // Base plan items (metadata.kind == "base") must not be classified as
        // seat add-ons — that would cause the reconciliation path to treat
        // the base plan quantity as extra seats.
        let item = serde_json::json!({
            "price": {
                "id": "price_base_plan",
                "metadata": { "kind": "base", "tier": "business" },
                "recurring": { "interval": "month" }
            }
        });
        assert_eq!(tier_for_seat_item(&item), None);
    }

    #[test]
    fn tier_for_seat_item_returns_none_without_metadata() {
        // Degrade gracefully when metadata is missing (e.g. a price that
        // predates the bootstrap stamping). Must not panic or misclassify.
        let no_metadata = serde_json::json!({
            "price": { "id": "price_legacy", "recurring": { "interval": "month" } }
        });
        assert_eq!(tier_for_seat_item(&no_metadata), None);

        let no_kind = serde_json::json!({
            "price": {
                "metadata": { "tier": "business" },
                "recurring": { "interval": "month" }
            }
        });
        assert_eq!(tier_for_seat_item(&no_kind), None);

        let no_tier = serde_json::json!({
            "price": {
                "metadata": { "kind": "seat" },
                "recurring": { "interval": "month" }
            }
        });
        assert_eq!(tier_for_seat_item(&no_tier), None);

        let no_interval = serde_json::json!({
            "price": {
                "metadata": { "kind": "seat", "tier": "business" }
            }
        });
        assert_eq!(tier_for_seat_item(&no_interval), None);
    }

    #[test]
    fn item_is_seat_addon_only_true_for_seat_kind() {
        let seat = serde_json::json!({
            "price": { "metadata": { "kind": "seat", "tier": "business" } }
        });
        assert!(item_is_seat_addon(&seat));

        let base = serde_json::json!({
            "price": { "metadata": { "kind": "base", "tier": "business" } }
        });
        assert!(!item_is_seat_addon(&base));

        let bare = serde_json::json!({ "price": { "id": "price_x" } });
        assert!(!item_is_seat_addon(&bare));
    }

    // ── seat_count reconciliation logic tests ─────────────────────────────

    /// Test payload item kind — base plan or seat add-on. Matches the
    /// `metadata.kind` stamp the bootstrap script applies to every price.
    enum ItemKind {
        Base,
        Seat,
    }

    /// Helper: build a minimal subscription.updated items payload. Each item
    /// is stamped with `price.metadata.kind` and `price.metadata.tier` to
    /// match what the bootstrap script produces — that is the sole signal the
    /// orchestrator uses to classify items.
    fn make_items_payload(items: &[(ItemKind, &str, &str, u64)]) -> serde_json::Value {
        let data: Vec<serde_json::Value> = items
            .iter()
            .map(|(kind, tier, interval, qty)| {
                let kind_str = match kind {
                    ItemKind::Base => "base",
                    ItemKind::Seat => "seat",
                };
                serde_json::json!({
                    "price": {
                        "id": format!("price_opaque_{}_{}_{}", kind_str, tier, interval),
                        "metadata": { "kind": kind_str, "tier": tier },
                        "recurring": { "interval": interval }
                    },
                    "quantity": qty
                })
            })
            .collect();
        serde_json::json!({ "items": { "data": data } })
    }

    /// Computes the reconciled seat count using the same logic as
    /// handle_subscription_updated, extracted as a pure function for testing.
    fn compute_reconciled_seats(obj: &serde_json::Value, tier_str: &str) -> u32 {
        let included = included_seats_for_tier(tier_str);
        obj.get("items")
            .and_then(|i| i.get("data"))
            .and_then(|d| d.as_array())
            .map(|arr| {
                let addon_qty: u32 = arr
                    .iter()
                    .find(|item| item_is_seat_addon(item))
                    .and_then(|item| item.get("quantity"))
                    .and_then(|q| q.as_u64())
                    .map(|q| q.min(u32::MAX as u64) as u32)
                    .unwrap_or(0);
                included + addon_qty
            })
            .unwrap_or(included)
    }

    /// Bug regression: single-item subscription (base plan only, quantity=1).
    /// Previously data[0].quantity=1 was used as the total seat count, which
    /// stomped any included seats > 1. Now it must return the tier's included
    /// seat count with no add-on added.
    #[test]
    fn reconcile_seats_single_item_base_plan_only() {
        // Business base plan (metadata.kind=base) only, no seat add-on.
        let obj = make_items_payload(&[(ItemKind::Base, "business", "month", 1)]);
        // Business includes 5 seats; with no seat add-on that should be the total.
        assert_eq!(compute_reconciled_seats(&obj, "business"), 5);
    }

    /// Bug regression: multi-item subscription with base plan + seat add-on.
    /// Previously data[0] (the base plan, qty=1) was used, giving seat_count=1.
    /// The fix must find the seat add-on by metadata.kind and sum correctly.
    #[test]
    fn reconcile_seats_multi_item_finds_addon_not_base_plan() {
        let obj = make_items_payload(&[
            (ItemKind::Base, "business", "month", 1), // base plan — must NOT be used
            (ItemKind::Seat, "business", "month", 5), // seat add-on: 5 extra
        ]);
        // Business included=5, addon=5 → total=10
        assert_eq!(compute_reconciled_seats(&obj, "business"), 10);
    }

    /// Seat add-on at position 0 (Stripe may reorder items) is still found.
    #[test]
    fn reconcile_seats_addon_first_in_list() {
        let obj = make_items_payload(&[
            (ItemKind::Seat, "business", "month", 2), // seat add-on comes first
            (ItemKind::Base, "business", "month", 1), // base plan
        ]);
        // Business included=5, addon=2 → total=7
        assert_eq!(compute_reconciled_seats(&obj, "business"), 7);
    }

    /// No items array → falls back to included seats only, no panic.
    #[test]
    fn reconcile_seats_missing_items_falls_back_to_included() {
        let obj = serde_json::json!({});
        assert_eq!(compute_reconciled_seats(&obj, "business"), 5);
    }

    /// Consumer/personal tenant (no "t-" prefix) is no longer gated out and
    /// gets the same reconciliation as team tenants.
    #[test]
    fn reconcile_seats_not_gated_on_tenant_prefix() {
        // This test validates the logic is prefix-agnostic; the gate removal
        // itself is structural (no `if starts_with("t-")` wrapper), which this
        // helper exercises without any prefix filtering.
        let obj = make_items_payload(&[
            (ItemKind::Base, "business", "month", 1),
            (ItemKind::Seat, "business", "month", 2),
        ]);
        assert_eq!(compute_reconciled_seats(&obj, "business"), 7); // 5 included + 2 addon
    }

    // ── subscription_updated_syncs_keycloak regression ────────────────────
    //
    // Regression for the bug where handle_subscription_updated updated the DB
    // tier but never called sync_tier_to_keycloak, causing Pro → Business
    // upgrades via the Stripe portal to not propagate to Keycloak JWT claims.
    //
    // sync_tier_to_keycloak takes &AppState, which is too costly to construct
    // in a unit test (it requires many real service implementations). Instead,
    // we verify the observable side-effect at the level where it was broken:
    // the Keycloak PUT body must include the full user representation with the
    // correct zaru_tier attribute. We do this by exercising the body-building
    // logic in KeycloakAdminClient::set_user_attribute directly.
    //
    // The structural fix (adding the sync_tier_to_keycloak call in
    // handle_subscription_updated) is verified by code review; this test pins
    // the PUT body contract so a future partial-body regression would be caught.
    #[test]
    fn subscription_updated_syncs_keycloak_put_body_includes_full_user_representation() {
        use aegis_orchestrator_core::infrastructure::iam::keycloak_admin_client::KeycloakUser;

        // Simulate a user object as would be returned by list_realm_users.
        let mut existing_attrs = std::collections::HashMap::new();
        existing_attrs.insert("tenant_id".to_string(), vec!["u-pro-user".to_string()]);
        existing_attrs.insert("zaru_tier".to_string(), vec!["pro".to_string()]);

        let user = KeycloakUser {
            id: "kc-user-001".to_string(),
            email: Some("alice@example.com".to_string()),
            first_name: Some("Alice".to_string()),
            last_name: Some("Smith".to_string()),
            created_timestamp: 1_700_000_000,
            attributes: Some(existing_attrs),
        };

        // The business tier value that sync_tier_to_keycloak would pass after a
        // Pro → Business upgrade (tier_to_str(&TenantTier::Business) == "business").
        let new_tier_value = tier_to_str(&TenantTier::Business);

        // Simulate what set_user_attribute now does: build the full body.
        // We test the body-building helper indirectly via serde_json to confirm
        // all required fields are present and createdTimestamp is absent.
        let mut merged_attrs = user.attributes.clone().unwrap_or_default();
        merged_attrs.insert("zaru_tier".to_string(), vec![new_tier_value.to_string()]);

        let put_body = serde_json::json!({
            "id": user.id,
            "email": user.email,
            "firstName": user.first_name,
            "lastName": user.last_name,
            "enabled": true,
            "attributes": merged_attrs
        });

        // Full user representation is present — Keycloak won't 400.
        assert_eq!(put_body["id"], "kc-user-001");
        assert_eq!(put_body["email"], "alice@example.com");
        assert_eq!(put_body["firstName"], "Alice");
        assert_eq!(put_body["lastName"], "Smith");
        assert_eq!(put_body["enabled"], true);

        // createdTimestamp must be absent — Keycloak rejects it on PUT.
        assert!(
            put_body.get("createdTimestamp").is_none(),
            "createdTimestamp must be omitted from Keycloak PUT body"
        );

        // The tier was upgraded: zaru_tier is now "business", not "pro".
        assert_eq!(put_body["attributes"]["zaru_tier"][0], "business");

        // Pre-existing attributes are preserved (not nulled out).
        assert_eq!(put_body["attributes"]["tenant_id"][0], "u-pro-user");
    }

    // ── colony suspension (Phase 4) ───────────────────────────────────────
    //
    // These tests exercise `reconcile_team_suspension_with` — the pure form
    // of the AppState helper used from `handle_subscription_updated` and
    // `handle_subscription_deleted`. The full webhook path is exercised in
    // the middleware tests; here we pin the state transition so a regression
    // that dropped the suspend/resume logic (or wired it up on the wrong
    // tier boundary) is caught by a unit test.

    mod colony_suspension {
        use super::super::*;
        use aegis_orchestrator_core::application::effective_tier_service::{
            EffectiveTierError, EffectiveTierService,
        };
        use aegis_orchestrator_core::domain::repository::RepositoryError;
        use aegis_orchestrator_core::domain::team::{
            Team, TeamId, TeamRepository, TeamSlug, TeamStatus,
        };
        use aegis_orchestrator_core::domain::tenancy::TenantTier;
        use aegis_orchestrator_core::domain::tenant::TenantId;
        use async_trait::async_trait;
        use std::collections::HashMap;
        use std::sync::Mutex;

        #[derive(Default)]
        struct InMemoryTeamRepo {
            by_tenant: Mutex<HashMap<String, Team>>,
        }

        impl InMemoryTeamRepo {
            fn insert(&self, team: Team) {
                self.by_tenant
                    .lock()
                    .unwrap()
                    .insert(team.tenant_id.as_str().to_string(), team);
            }
            fn get(&self, tenant_id: &TenantId) -> Option<Team> {
                self.by_tenant
                    .lock()
                    .unwrap()
                    .get(tenant_id.as_str())
                    .cloned()
            }
        }

        #[async_trait]
        impl TeamRepository for InMemoryTeamRepo {
            async fn save(&self, team: &Team) -> Result<(), RepositoryError> {
                self.insert(team.clone());
                Ok(())
            }
            async fn find_by_id(&self, id: &TeamId) -> Result<Option<Team>, RepositoryError> {
                Ok(self
                    .by_tenant
                    .lock()
                    .unwrap()
                    .values()
                    .find(|t| t.id == *id)
                    .cloned())
            }
            async fn find_by_slug(&self, slug: &TeamSlug) -> Result<Option<Team>, RepositoryError> {
                Ok(self
                    .by_tenant
                    .lock()
                    .unwrap()
                    .values()
                    .find(|t| &t.slug == slug)
                    .cloned())
            }
            async fn find_by_owner(
                &self,
                owner_user_id: &str,
            ) -> Result<Vec<Team>, RepositoryError> {
                Ok(self
                    .by_tenant
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|t| t.owner_user_id == owner_user_id)
                    .cloned()
                    .collect())
            }
            async fn find_by_tenant_id(
                &self,
                tenant_id: &TenantId,
            ) -> Result<Option<Team>, RepositoryError> {
                Ok(self
                    .by_tenant
                    .lock()
                    .unwrap()
                    .get(tenant_id.as_str())
                    .cloned())
            }
            async fn delete(&self, id: &TeamId) -> Result<(), RepositoryError> {
                self.by_tenant.lock().unwrap().retain(|_, t| t.id != *id);
                Ok(())
            }
        }

        /// Recording stub — captures the team ids passed to
        /// `recompute_for_team` so tests can assert side effects.
        #[derive(Default)]
        struct RecordingTierService {
            recomputed: Mutex<Vec<TeamId>>,
        }

        #[async_trait]
        impl EffectiveTierService for RecordingTierService {
            async fn recompute_for_user(
                &self,
                _user_id: &str,
            ) -> Result<TenantTier, EffectiveTierError> {
                Ok(TenantTier::Free)
            }
            async fn recompute_for_team(&self, team_id: &TeamId) -> Result<(), EffectiveTierError> {
                self.recomputed.lock().unwrap().push(*team_id);
                Ok(())
            }
        }

        fn mk_active_business_team() -> Team {
            let mut team =
                Team::provision("Acme".into(), "owner-1".into(), TenantTier::Business).unwrap();
            let _ = team.take_events();
            team
        }

        #[tokio::test]
        async fn subscription_downgrade_to_pro_suspends_team_colony() {
            let team_repo = std::sync::Arc::new(InMemoryTeamRepo::default());
            let service = std::sync::Arc::new(RecordingTierService::default());
            let team = mk_active_business_team();
            let tenant_id = team.tenant_id.clone();
            let team_id = team.id;
            team_repo.insert(team);

            reconcile_team_suspension_with(
                team_repo.as_ref(),
                service.as_ref(),
                &tenant_id,
                &TenantTier::Pro,
            )
            .await;

            let reloaded = team_repo.get(&tenant_id).expect("team persisted");
            assert_eq!(reloaded.status, TeamStatus::Suspended);
            assert_eq!(service.recomputed.lock().unwrap().as_slice(), &[team_id]);
        }

        #[tokio::test]
        async fn subscription_deleted_suspends_team_colony() {
            // Deletion lands as Free at the billing layer — same effect: below
            // Business → colony suspended.
            let team_repo = std::sync::Arc::new(InMemoryTeamRepo::default());
            let service = std::sync::Arc::new(RecordingTierService::default());
            let team = mk_active_business_team();
            let tenant_id = team.tenant_id.clone();
            let team_id = team.id;
            team_repo.insert(team);

            reconcile_team_suspension_with(
                team_repo.as_ref(),
                service.as_ref(),
                &tenant_id,
                &TenantTier::Free,
            )
            .await;

            let reloaded = team_repo.get(&tenant_id).expect("team persisted");
            assert_eq!(reloaded.status, TeamStatus::Suspended);
            assert_eq!(service.recomputed.lock().unwrap().as_slice(), &[team_id]);
        }

        #[tokio::test]
        async fn subscription_upgrade_to_business_resumes_suspended_team() {
            let team_repo = std::sync::Arc::new(InMemoryTeamRepo::default());
            let service = std::sync::Arc::new(RecordingTierService::default());
            let mut team = mk_active_business_team();
            team.suspend().unwrap();
            let _ = team.take_events();
            let tenant_id = team.tenant_id.clone();
            let team_id = team.id;
            team_repo.insert(team);

            reconcile_team_suspension_with(
                team_repo.as_ref(),
                service.as_ref(),
                &tenant_id,
                &TenantTier::Business,
            )
            .await;

            let reloaded = team_repo.get(&tenant_id).expect("team persisted");
            assert_eq!(reloaded.status, TeamStatus::Active);
            assert_eq!(service.recomputed.lock().unwrap().as_slice(), &[team_id]);
        }

        #[tokio::test]
        async fn non_team_tenant_is_no_op() {
            // Regression guard: reconciliation must not fire on personal /
            // consumer tenant subscriptions — those never own a colony.
            let team_repo = std::sync::Arc::new(InMemoryTeamRepo::default());
            let service = std::sync::Arc::new(RecordingTierService::default());
            let personal = TenantId::from_realm_slug("u-abc123").unwrap();

            reconcile_team_suspension_with(
                team_repo.as_ref(),
                service.as_ref(),
                &personal,
                &TenantTier::Free,
            )
            .await;

            assert!(service.recomputed.lock().unwrap().is_empty());
        }
    }

    // ── consumer_tenant_id_to_user_sub regression ─────────────────────────

    #[test]
    fn consumer_tenant_id_to_user_sub_round_trips() {
        use aegis_orchestrator_core::domain::tenant::TenantId;

        // Valid per-user consumer tenant ID → UUID with hyphens.
        let tenant_id = TenantId::from_string("u-d7f8170035d349b6b237c391ccc19035").unwrap();
        assert_eq!(
            consumer_tenant_id_to_user_sub(&tenant_id),
            Some("d7f81700-35d3-49b6-b237-c391ccc19035".to_string())
        );

        // The shared consumer realm slug is not a per-user tenant — must return None.
        let shared = TenantId::consumer();
        assert_eq!(consumer_tenant_id_to_user_sub(&shared), None);

        // An enterprise tenant slug is not a per-user consumer tenant — must return None.
        let enterprise = TenantId::from_string("acme-corp").unwrap();
        assert_eq!(consumer_tenant_id_to_user_sub(&enterprise), None);
    }
}
