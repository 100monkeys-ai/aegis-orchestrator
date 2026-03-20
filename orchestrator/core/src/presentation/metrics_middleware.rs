// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Metrics Middleware
//!
//! Axum and gRPC middleware for tracking request metrics (ADR-058).
//! Tracks request counts and duration with method, path, and status code labels.

use axum::{
    extract::MatchedPath,
    http::{Request, Response},
    middleware::Next,
    response::Response as AxumResponse,
};
use futures::future::BoxFuture;
use std::task::{Context, Poll};
use std::time::Instant;
use tower::{Layer, Service};

fn normalize_grpc_method(path: &str) -> String {
    path.trim_matches('/').replace('/', ".").replace(':', "_")
}

/// Axum middleware that tracks HTTP metrics.
///
/// Tracks:
/// - `aegis_http_requests_total`: Counter (labels: method, path_template, status_code)
/// - `aegis_http_request_duration_seconds`: Histogram (labels: method, path_template)
pub async fn metrics_middleware(req: Request<axum::body::Body>, next: Next) -> AxumResponse {
    let start = Instant::now();
    let method = req.method().to_string();

    // Get the matched path template (e.g., "/v1/executions/:id")
    // If not available (e.g., 404), default to "unknown_path"
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|mp| mp.as_str())
        .unwrap_or("unknown_path")
        .to_string();

    let response = next.run(req).await;

    let latency = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    metrics::counter!(
        "aegis_http_requests_total",
        "method" => method.clone(),
        "path_template" => path.clone(),
        "status_code" => status
    )
    .increment(1);

    metrics::histogram!(
        "aegis_http_request_duration_seconds",
        "method" => method,
        "path_template" => path
    )
    .record(latency);

    response
}

/// Tower layer for tracking gRPC metrics.
#[derive(Clone)]
pub struct GrpcMetricsLayer;

impl<S> Layer<S> for GrpcMetricsLayer {
    type Service = GrpcMetricsService<S>;

    fn layer(&self, service: S) -> Self::Service {
        GrpcMetricsService { service }
    }
}

/// Tower service for tracking gRPC metrics.
#[derive(Clone)]
pub struct GrpcMetricsService<S> {
    service: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for GrpcMetricsService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let start = Instant::now();
        let method = normalize_grpc_method(req.uri().path());

        let mut next_service = self.service.clone();
        Box::pin(async move {
            let response = next_service.call(req).await?;

            let latency = start.elapsed().as_secs_f64();

            // In gRPC, the status is often in the headers (for errors) or trailers.
            // For simplicity in this middleware, we check headers.
            let grpc_status = response
                .headers()
                .get("grpc-status")
                .and_then(|s| s.to_str().ok())
                .unwrap_or("0") // Default to 0 (OK) if not present in headers
                .to_string();

            metrics::counter!(
                "aegis_grpc_requests_total",
                "method" => method.clone(),
                "code" => grpc_status
            )
            .increment(1);

            metrics::histogram!(
                "aegis_grpc_request_duration_seconds",
                "method" => method
            )
            .record(latency);

            Ok(response)
        })
    }
}
