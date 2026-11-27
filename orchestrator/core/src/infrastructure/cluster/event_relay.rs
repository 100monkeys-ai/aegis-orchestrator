// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The cluster event relay (AEGIS ADR-152 D3): one `StreamClusterEvents`
//! subscription to each live peer, whose events this node republishes on its
//! local bus with their origin intact, so that cluster-wide subscribers (SSE
//! for executions and workflows, canvas SSE, volume deregistration) see what
//! happens on every node.
//!
//! Delivery is per-origin FIFO and at most once. The relay watches each
//! origin's sequence: a hole, or a stream that ends, is a gap, counted and
//! logged here; the durable trail stays `execution_events`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::domain::cluster::NodeId;
use crate::domain::cluster_membership::{NodeLeaseRepository, NodeProcess};
use crate::infrastructure::aegis_cluster_proto::ClusterEventEnvelope;
use crate::infrastructure::cluster::GrpcPeerTransport;
use crate::infrastructure::event_bus::{DomainEvent, EventBus, EventOrigin};

/// What one envelope's sequence says about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceCheck {
    /// The next one, or the first seen from this process.
    InOrder,
    /// `missed` events from this process never arrived.
    Gap { missed: u64 },
    /// Already delivered (a resumed stream overlapping the last): dropped.
    Duplicate,
}

/// The last sequence delivered from each origin process (ADR-152 D3.4).
#[derive(Debug, Default)]
pub struct OriginSequences {
    last: HashMap<(uuid::Uuid, i64), u64>,
}

impl OriginSequences {
    pub fn check(&mut self, origin: EventOrigin) -> SequenceCheck {
        // A newer incarnation of the origin starts its sequence again.
        self.last.retain(|(node, incarnation), _| {
            *node != origin.node_id || *incarnation >= origin.incarnation
        });
        let key = (origin.node_id, origin.incarnation);
        match self.last.get(&key).copied() {
            Some(last) if origin.seq <= last => SequenceCheck::Duplicate,
            Some(last) if origin.seq > last + 1 => {
                self.last.insert(key, origin.seq);
                SequenceCheck::Gap {
                    missed: origin.seq - last - 1,
                }
            }
            _ => {
                self.last.insert(key, origin.seq);
                SequenceCheck::InOrder
            }
        }
    }
}

/// Decode an envelope into the event and its origin.
pub fn decode(envelope: &ClusterEventEnvelope) -> anyhow::Result<(EventOrigin, DomainEvent)> {
    let origin = EventOrigin {
        node_id: uuid::Uuid::parse_str(&envelope.origin_node_id)?,
        incarnation: envelope.origin_incarnation,
        seq: envelope.origin_seq,
    };
    let event: DomainEvent = serde_json::from_slice(&envelope.event_json)?;
    Ok((origin, event))
}

/// Holds a subscription to each live peer and republishes what arrives.
pub struct ClusterEventRelay {
    me: NodeProcess,
    leases: Arc<dyn NodeLeaseRepository>,
    transport: Arc<GrpcPeerTransport>,
    bus: EventBus,
    refresh: Duration,
}

impl ClusterEventRelay {
    pub fn new(
        me: NodeProcess,
        leases: Arc<dyn NodeLeaseRepository>,
        transport: Arc<GrpcPeerTransport>,
        bus: EventBus,
    ) -> Self {
        Self {
            me,
            leases,
            transport,
            bus,
            refresh: Duration::from_secs(5),
        }
    }

    /// Re-read the live peers every few seconds; open a subscription to each
    /// new one and drop those whose lease has gone.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut streams: HashMap<NodeId, (String, tokio::task::JoinHandle<()>)> =
                HashMap::new();
            loop {
                match self.leases.leases().await {
                    Ok(leases) => {
                        let live: HashMap<NodeId, String> = leases
                            .into_iter()
                            .filter(|l| l.live && l.node_id != self.me.node_id)
                            .map(|l| (l.node_id, l.grpc_address))
                            .collect();
                        let gone: HashSet<NodeId> = streams
                            .iter()
                            .filter(|(id, (address, task))| {
                                live.get(id) != Some(address) || task.is_finished()
                            })
                            .map(|(id, _)| *id)
                            .collect();
                        for id in gone {
                            if let Some((_, task)) = streams.remove(&id) {
                                task.abort();
                            }
                        }
                        for (id, address) in live {
                            streams.entry(id).or_insert_with(|| {
                                let relay = self.clone();
                                let at = address.clone();
                                (
                                    address,
                                    tokio::spawn(async move { relay.follow(id, at).await }),
                                )
                            });
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "The event relay could not read the peers")
                    }
                }
                tokio::time::sleep(self.refresh).await;
            }
        })
    }

    /// One peer's stream, until it ends; the supervisor opens it again.
    async fn follow(&self, peer: NodeId, address: String) {
        let mut sequences = OriginSequences::default();
        let mut stream = match self
            .transport
            .stream_cluster_events(&address, self.me)
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                tracing::debug!(peer = %peer, error = %e, "The event relay could not reach a peer");
                return;
            }
        };
        tracing::info!(peer = %peer, "Following the peer's cluster events");
        loop {
            let envelope = match stream.message().await {
                Ok(Some(envelope)) => envelope,
                Ok(None) | Err(_) => {
                    metrics::counter!("aegis_cluster_relay_gaps_total", "reason" => "stream_ended")
                        .increment(1);
                    tracing::warn!(peer = %peer, "The peer's event relay ended; events may have been missed");
                    return;
                }
            };
            let (origin, event) = match decode(&envelope) {
                Ok(decoded) => decoded,
                Err(e) => {
                    tracing::error!(peer = %peer, error = %e, "A relayed event did not decode");
                    continue;
                }
            };
            match sequences.check(origin) {
                SequenceCheck::Duplicate => continue,
                SequenceCheck::Gap { missed } => {
                    metrics::counter!("aegis_cluster_relay_gaps_total", "reason" => "sequence")
                        .increment(1);
                    tracing::warn!(peer = %peer, missed, "Events relayed from the peer were missed");
                }
                SequenceCheck::InOrder => {}
            }
            self.bus.publish_relayed(event, origin);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(node: uuid::Uuid, incarnation: i64, seq: u64) -> EventOrigin {
        EventOrigin {
            node_id: node,
            incarnation,
            seq,
        }
    }

    #[test]
    fn a_hole_in_an_origins_sequence_is_a_gap_and_a_repeat_is_dropped() {
        let node = uuid::Uuid::new_v4();
        let mut s = OriginSequences::default();
        assert_eq!(
            s.check(at(node, 1, 7)),
            SequenceCheck::InOrder,
            "the first seen starts anywhere"
        );
        assert_eq!(s.check(at(node, 1, 8)), SequenceCheck::InOrder);
        assert_eq!(s.check(at(node, 1, 11)), SequenceCheck::Gap { missed: 2 });
        assert_eq!(s.check(at(node, 1, 11)), SequenceCheck::Duplicate);
        assert_eq!(s.check(at(node, 1, 9)), SequenceCheck::Duplicate);
        assert_eq!(
            s.check(at(node, 2, 1)),
            SequenceCheck::InOrder,
            "a new process starts again"
        );
        assert_eq!(s.check(at(node, 2, 2)), SequenceCheck::InOrder);
    }

    #[tokio::test]
    async fn origin_subscribers_see_local_events_once_and_cluster_wide_ones_see_relayed_too() {
        use crate::domain::events::ExecutionEvent;
        let bus = EventBus::new(16);
        let mut origin_only = bus.subscribe();
        let mut cluster_wide = bus.subscribe_cluster_wide();
        let mut outbound = bus.subscribe_origin();
        let me = uuid::Uuid::new_v4();
        bus.set_origin(me, 3);

        let event = || ExecutionEvent::ConsoleOutput {
            execution_id: crate::domain::execution::ExecutionId::new(),
            agent_id: crate::domain::agent::AgentId::new(),
            iteration_number: 1,
            stream: "stdout".to_string(),
            content: "x".to_string(),
            timestamp: chrono::Utc::now(),
        };
        bus.publish_execution_event(event());
        bus.publish_execution_event(event());
        let peer = uuid::Uuid::new_v4();
        bus.publish_relayed(DomainEvent::Execution(event()), at(peer, 1, 40));

        let (first, _) = outbound.recv().await.unwrap();
        let (second, _) = outbound.recv().await.unwrap();
        assert_eq!(
            (first, second),
            (at(me, 3, 1), at(me, 3, 2)),
            "local events carry their origin and sequence"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), outbound.recv())
                .await
                .is_err(),
            "a relayed copy is never streamed back out"
        );

        origin_only.recv().await.unwrap();
        origin_only.recv().await.unwrap();
        assert!(
            matches!(
                origin_only.try_recv(),
                Err(crate::infrastructure::event_bus::EventBusError::Empty)
            ),
            "origin-only never sees the relayed copy"
        );

        for _ in 0..3 {
            cluster_wide.recv().await.unwrap();
        }

        let wire = ClusterEventEnvelope {
            origin_node_id: peer.to_string(),
            origin_incarnation: 1,
            origin_seq: 40,
            category: "execution".into(),
            event_json: serde_json::to_vec(&DomainEvent::Execution(event())).unwrap(),
        };
        let (origin, decoded) = decode(&wire).unwrap();
        assert_eq!(origin, at(peer, 1, 40));
        assert!(matches!(
            decoded,
            DomainEvent::Execution(ExecutionEvent::ConsoleOutput { .. })
        ));
    }
}
