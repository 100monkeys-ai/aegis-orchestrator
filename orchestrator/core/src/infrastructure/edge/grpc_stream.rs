// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 ConnectEdge / RotateEdgeKey tonic handler glue.
//!
//! See `crate::application::edge::connect_edge` and
//! `crate::application::edge::rotate_edge_key` for the underlying logic.

use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

use crate::application::edge::connect_edge::ConnectEdgeService;
use crate::application::edge::rotate_edge_key::RotateEdgeKeyService;
use crate::infrastructure::aegis_cluster_proto::{
    EdgeCommand, EdgeEvent, RotateEdgeKeyRequest, RotateEdgeKeyResponse,
};

pub type EdgeCommandStream = Pin<Box<dyn Stream<Item = Result<EdgeCommand, Status>> + Send>>;

pub async fn handle_connect_edge(
    service: Arc<ConnectEdgeService>,
    request: Request<Streaming<EdgeEvent>>,
) -> Result<Response<EdgeCommandStream>, Status> {
    let mut inbound = request.into_inner();
    let (cmd_tx, cmd_rx) = mpsc::channel::<EdgeCommand>(32);

    let svc = service.clone();
    tokio::spawn(async move {
        if let Some(first) = inbound.next().await {
            match first {
                Ok(event) => {
                    if let Err(e) = svc.handle_stream(event, inbound, cmd_tx.clone()).await {
                        tracing::warn!(error = %e, "ConnectEdge stream terminated with error");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "ConnectEdge first message error");
                }
            }
        }
    });

    let outbound: EdgeCommandStream = Box::pin(ReceiverStream::new(cmd_rx).map(Ok));
    Ok(Response::new(outbound))
}

pub async fn handle_rotate_edge_key(
    service: Arc<RotateEdgeKeyService>,
    request: Request<RotateEdgeKeyRequest>,
) -> Result<Response<RotateEdgeKeyResponse>, Status> {
    let req = request.into_inner();
    let resp = service
        .rotate(req)
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    Ok(Response::new(resp))
}
