// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # ClusterDutyRunner (ADR-152 D2)
//!
//! Owns the tick of every background loop the daemon runs. There are two
//! kinds:
//!
//! - **Cluster duties** ([`ClusterDutyName`]) are singletons: on a cluster
//!   node exactly one process runs each at a time, the one holding its lease
//!   in `cluster_duties`. The runner tries to acquire or renew every held
//!   or wanted duty every `renew_every` for `ttl`; a duty runs its first
//!   tick when this process gains it (at a new epoch) and then on its
//!   interval while it keeps it. The tick receives a [`DutyFence`] carrying
//!   the epoch it was granted, which a duty with non-idempotent writes puts
//!   into them. A duty whose lease is lost, or whose holder stops being
//!   `Active`, starts no further tick; a tick already running finishes, and
//!   its fenced writes affect nothing.
//! - **Node duties** run on every node, because what they tend is the
//!   node's own (its Podman containers, its tool catalog). They hold no
//!   lease.
//!
//! On a node without clustering, or without a database, the runner is
//! [`DutyLeasing::Solo`]: every duty runs here, alone, with
//! [`DutyFence::Solo`], exactly on its schedule.
//!
//! A duty's lease is judged locally by the monotonic clock: held until `ttl`
//! after the moment its last successful renewal was sent, never later than
//! the database's own expiry.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::application::cluster::membership::NodeMembership;
use crate::domain::cluster_membership::{
    ClusterDutyName, ClusterDutyRepository, DutyEpoch, DutyFence, MemberState,
};

/// One run of a cluster duty.
pub type DutyTick = Arc<dyn Fn(DutyFence) -> BoxFuture<'static, ()> + Send + Sync>;
/// One run of a node duty.
pub type NodeDutyTick = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// When a duty runs: `first` after this process gains it (a node duty:
/// after the runner starts), then every `every`, or never again when
/// `every` is `None` (a pass at acquisition, like a reconcile at start).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DutySchedule {
    pub first: Duration,
    pub every: Option<Duration>,
}

impl DutySchedule {
    /// At once, then every `interval` (a `tokio::time::interval` loop).
    pub fn every(interval: Duration) -> Self {
        Self {
            first: Duration::ZERO,
            every: Some(interval),
        }
    }

    /// Every `interval`, the first after one interval.
    pub fn every_after_first_interval(interval: Duration) -> Self {
        Self {
            first: interval,
            every: Some(interval),
        }
    }

    /// Once, each time this process gains the duty.
    pub fn on_acquire() -> Self {
        Self {
            first: Duration::ZERO,
            every: None,
        }
    }
}

/// Whether duties are leased in `cluster_duties` or run here alone.
#[derive(Clone)]
pub enum DutyLeasing {
    Solo,
    Leased {
        repo: Arc<dyn ClusterDutyRepository>,
        membership: Arc<NodeMembership>,
        /// How often a duty's lease is acquired or renewed, and for how long.
        renew_every: Duration,
        ttl: Duration,
    },
}

struct ClusterDuty {
    name: ClusterDutyName,
    schedule: DutySchedule,
    tick: DutyTick,
}

struct NodeDuty {
    name: &'static str,
    schedule: DutySchedule,
    tick: NodeDutyTick,
}

/// The duties this process holds, by epoch (observability and tests).
pub type HeldDuties = Arc<Mutex<HashMap<ClusterDutyName, DutyEpoch>>>;

/// Collects the duties, then runs them.
pub struct ClusterDutyRunner {
    leasing: DutyLeasing,
    cluster: Vec<ClusterDuty>,
    node: Vec<NodeDuty>,
}

impl ClusterDutyRunner {
    pub fn new(leasing: DutyLeasing) -> Self {
        Self {
            leasing,
            cluster: Vec::new(),
            node: Vec::new(),
        }
    }

    pub fn is_leased(&self) -> bool {
        matches!(self.leasing, DutyLeasing::Leased { .. })
    }

    /// Register a singleton duty. A name registered twice is refused at
    /// start, where it is a wiring mistake.
    pub fn register<F, Fut>(&mut self, name: ClusterDutyName, schedule: DutySchedule, tick: F)
    where
        F: Fn(DutyFence) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        assert!(
            self.cluster.iter().all(|d| d.name != name),
            "cluster duty {name} registered twice"
        );
        self.cluster.push(ClusterDuty {
            name,
            schedule,
            tick: Arc::new(move |fence| Box::pin(tick(fence))),
        });
    }

    /// Register a duty every node runs for itself.
    pub fn register_node_duty<F, Fut>(
        &mut self,
        name: &'static str,
        schedule: DutySchedule,
        tick: F,
    ) where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        assert!(
            self.node.iter().all(|d| d.name != name),
            "node duty {name} registered twice"
        );
        self.node.push(NodeDuty {
            name,
            schedule,
            tick: Arc::new(move || Box::pin(tick())),
        });
    }

    /// The names registered so far.
    pub fn cluster_duties(&self) -> Vec<ClusterDutyName> {
        self.cluster.iter().map(|d| d.name).collect()
    }

    pub fn node_duties(&self) -> Vec<&'static str> {
        self.node.iter().map(|d| d.name).collect()
    }

    /// Start every duty. Dropping or shutting down the handle stops them;
    /// a leased duty is released on the way out.
    pub fn spawn(self) -> DutyRunnerHandle {
        let (stop_tx, stop_rx) = watch::channel(false);
        let held: HeldDuties = Arc::default();
        let mut tasks = Vec::new();
        tracing::info!(
            leased = self.is_leased(),
            cluster_duties = ?self.cluster_duties().iter().map(|d| d.as_str()).collect::<Vec<_>>(),
            node_duties = ?self.node_duties(),
            "Cluster duty runner started"
        );
        for duty in self.cluster {
            let stop = stop_rx.clone();
            let held = held.clone();
            tasks.push(match self.leasing.clone() {
                DutyLeasing::Solo => tokio::spawn(run_solo(duty, stop)),
                DutyLeasing::Leased {
                    repo,
                    membership,
                    renew_every,
                    ttl,
                } => tokio::spawn(run_leased(
                    duty,
                    repo,
                    membership,
                    renew_every,
                    ttl,
                    held,
                    stop,
                )),
            });
        }
        for duty in self.node {
            tasks.push(tokio::spawn(run_node_duty(duty, stop_rx.clone())));
        }
        DutyRunnerHandle {
            stop: stop_tx,
            tasks,
            held,
        }
    }
}

/// Stops the duties.
pub struct DutyRunnerHandle {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    held: HeldDuties,
}

impl DutyRunnerHandle {
    /// The duties this process holds now, with their epochs.
    pub fn held(&self) -> HashMap<ClusterDutyName, DutyEpoch> {
        self.held.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Stop every duty and release the leased ones, waiting for each.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for DutyRunnerHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

async fn sleep_or_stop(duration: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    if *stop.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => *stop.borrow(),
        _ = stop.changed() => true,
    }
}

async fn run_solo(duty: ClusterDuty, mut stop: watch::Receiver<bool>) {
    if sleep_or_stop(duty.schedule.first, &mut stop).await {
        return;
    }
    loop {
        (duty.tick)(DutyFence::Solo).await;
        metrics::counter!("aegis_cluster_duty_ticks_total", "duty" => duty.name.as_str())
            .increment(1);
        match duty.schedule.every {
            Some(every) => {
                if sleep_or_stop(every, &mut stop).await {
                    return;
                }
            }
            None => return,
        }
    }
}

async fn run_node_duty(duty: NodeDuty, mut stop: watch::Receiver<bool>) {
    if sleep_or_stop(duty.schedule.first, &mut stop).await {
        return;
    }
    loop {
        (duty.tick)().await;
        metrics::counter!("aegis_cluster_node_duty_ticks_total", "duty" => duty.name).increment(1);
        match duty.schedule.every {
            Some(every) => {
                if sleep_or_stop(every, &mut stop).await {
                    return;
                }
            }
            None => return,
        }
    }
}

struct Tenure {
    epoch: DutyEpoch,
    /// Local judgement of the lease: `ttl` after the last renewal was sent.
    deadline: Instant,
    /// When the next tick is due; `None` once a run-once duty has run.
    next_due: Option<Instant>,
}

fn set_held(held: &HeldDuties, name: ClusterDutyName, epoch: Option<DutyEpoch>) {
    let mut map = held.lock().unwrap_or_else(|p| p.into_inner());
    match epoch {
        Some(epoch) => {
            map.insert(name, epoch);
        }
        None => {
            map.remove(&name);
        }
    }
    metrics::gauge!("aegis_cluster_duty_held", "duty" => name.as_str()).set(if epoch.is_some() {
        1.0
    } else {
        0.0
    });
}

async fn run_leased(
    duty: ClusterDuty,
    repo: Arc<dyn ClusterDutyRepository>,
    membership: Arc<NodeMembership>,
    renew_every: Duration,
    ttl: Duration,
    held: HeldDuties,
    mut stop: watch::Receiver<bool>,
) {
    let me = membership.process();
    while let Err(e) = repo.ensure(duty.name).await {
        tracing::warn!(duty = %duty.name, error = %e, "Could not create the duty's row; retrying");
        if sleep_or_stop(renew_every, &mut stop).await {
            return;
        }
    }
    let mut tenure: Option<Tenure> = None;
    let mut running: Option<JoinHandle<()>> = None;
    loop {
        let now = Instant::now();
        if membership.state() != MemberState::Active {
            if let Some(t) = tenure.take() {
                tracing::warn!(duty = %duty.name, epoch = %t.epoch, "Giving the duty up: this node is not active");
                let _ = repo.release(duty.name, me, t.epoch).await;
                set_held(&held, duty.name, None);
            }
        } else {
            match repo.try_acquire(duty.name, me, ttl).await {
                Ok(Some(epoch)) => {
                    let deadline = now + ttl;
                    match &mut tenure {
                        Some(t) if t.epoch == epoch => t.deadline = deadline,
                        _ => {
                            tracing::info!(duty = %duty.name, epoch = %epoch, "Acquired the duty");
                            tenure = Some(Tenure {
                                epoch,
                                deadline,
                                next_due: Some(now + duty.schedule.first),
                            });
                            set_held(&held, duty.name, Some(epoch));
                        }
                    }
                }
                Ok(None) => {
                    if let Some(t) = tenure.take() {
                        tracing::warn!(duty = %duty.name, epoch = %t.epoch, "Lost the duty to another holder");
                        set_held(&held, duty.name, None);
                    }
                }
                Err(e) => {
                    tracing::warn!(duty = %duty.name, error = %e, "Could not renew the duty's lease");
                    if tenure
                        .as_ref()
                        .is_some_and(|t| Instant::now() >= t.deadline)
                    {
                        tracing::warn!(duty = %duty.name, "The duty's lease ran out unrenewed");
                        tenure = None;
                        set_held(&held, duty.name, None);
                    }
                }
            }
        }

        if let Some(t) = &mut tenure {
            let idle = running.as_ref().is_none_or(|r| r.is_finished());
            if idle && t.next_due.is_some_and(|due| Instant::now() >= due) {
                let fence = DutyFence::Leased {
                    duty: duty.name,
                    epoch: t.epoch,
                };
                let tick = duty.tick.clone();
                let name = duty.name;
                running = Some(tokio::spawn(async move {
                    tick(fence).await;
                    metrics::counter!("aegis_cluster_duty_ticks_total", "duty" => name.as_str())
                        .increment(1);
                }));
                t.next_due = duty.schedule.every.map(|every| Instant::now() + every);
            }
        }

        if sleep_or_stop(renew_every, &mut stop).await {
            break;
        }
    }
    if let Some(t) = tenure.take() {
        match repo.release(duty.name, me, t.epoch).await {
            Ok(true) => tracing::info!(duty = %duty.name, epoch = %t.epoch, "Released the duty"),
            Ok(false) => {}
            Err(e) => tracing::warn!(duty = %duty.name, error = %e, "Could not release the duty"),
        }
        set_held(&held, duty.name, None);
    }
    if let Some(r) = running {
        let _ = r.await;
    }
}

/// An in-process `cluster_duties`, with the statement's semantics and the
/// monotonic clock standing in for the database's (tests).
#[derive(Default)]
pub struct InMemoryClusterDutyRepository {
    rows: Mutex<HashMap<ClusterDutyName, DutyRow>>,
    unreachable: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, Default)]
struct DutyRow {
    holder: Option<crate::domain::cluster_membership::NodeProcess>,
    epoch: i64,
    expires: Option<Instant>,
}

impl InMemoryClusterDutyRepository {
    /// Make every call fail, as an unreachable database does.
    pub fn set_unreachable(&self, unreachable: bool) {
        self.unreachable
            .store(unreachable, std::sync::atomic::Ordering::SeqCst);
    }

    /// The duty's current epoch (the fence a write is checked against).
    pub fn epoch(&self, duty: ClusterDutyName) -> Option<DutyEpoch> {
        self.rows
            .lock()
            .unwrap()
            .get(&duty)
            .map(|r| DutyEpoch(r.epoch))
    }

    fn check(&self) -> anyhow::Result<()> {
        if self.unreachable.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("database unreachable");
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ClusterDutyRepository for InMemoryClusterDutyRepository {
    async fn ensure(&self, duty: ClusterDutyName) -> anyhow::Result<()> {
        self.check()?;
        self.rows.lock().unwrap().entry(duty).or_default();
        Ok(())
    }

    async fn try_acquire(
        &self,
        duty: ClusterDutyName,
        holder: crate::domain::cluster_membership::NodeProcess,
        ttl: Duration,
    ) -> anyhow::Result<Option<DutyEpoch>> {
        self.check()?;
        let now = Instant::now();
        let mut rows = self.rows.lock().unwrap();
        let Some(row) = rows.get_mut(&duty) else {
            return Ok(None);
        };
        let free = match row.holder {
            None => true,
            Some(h) => {
                row.expires.is_none_or(|e| e < now)
                    || (h.node_id == holder.node_id && h.incarnation <= holder.incarnation)
            }
        };
        if !free {
            return Ok(None);
        }
        if row.holder != Some(holder) {
            row.epoch += 1;
        }
        row.holder = Some(holder);
        row.expires = Some(now + ttl);
        Ok(Some(DutyEpoch(row.epoch)))
    }

    async fn release(
        &self,
        duty: ClusterDutyName,
        holder: crate::domain::cluster_membership::NodeProcess,
        epoch: DutyEpoch,
    ) -> anyhow::Result<bool> {
        self.check()?;
        let mut rows = self.rows.lock().unwrap();
        match rows.get_mut(&duty) {
            Some(row) if row.holder == Some(holder) && row.epoch == epoch.0 => {
                row.holder = None;
                row.expires = None;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::cluster::membership::tests::{join_request, FakeLeases};
    use crate::application::cluster::membership::LeaseTiming;
    use crate::domain::cluster::NodeId;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DUTY: ClusterDutyName = ClusterDutyName::custom("test-duty");

    /// A node with its own lease row, renewing it.
    async fn node() -> Arc<NodeMembership> {
        let membership = NodeMembership::join(
            Arc::new(FakeLeases::default()),
            join_request(NodeId::new()),
            LeaseTiming::default(),
        )
        .await
        .unwrap();
        membership.clone().spawn_renewal();
        membership
    }

    fn leased(
        repo: &Arc<InMemoryClusterDutyRepository>,
        membership: &Arc<NodeMembership>,
    ) -> DutyLeasing {
        DutyLeasing::Leased {
            repo: repo.clone(),
            membership: membership.clone(),
            renew_every: Duration::from_secs(3),
            ttl: Duration::from_secs(15),
        }
    }

    /// A runner with one duty that counts its ticks and records the fence
    /// of each.
    fn counting(
        leasing: DutyLeasing,
        schedule: DutySchedule,
    ) -> (
        ClusterDutyRunner,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<DutyFence>>>,
    ) {
        let ticks = Arc::new(AtomicUsize::new(0));
        let fences = Arc::new(Mutex::new(Vec::new()));
        let mut runner = ClusterDutyRunner::new(leasing);
        let (t, f) = (ticks.clone(), fences.clone());
        runner.register(DUTY, schedule, move |fence| {
            let (t, f) = (t.clone(), f.clone());
            async move {
                t.fetch_add(1, Ordering::SeqCst);
                f.lock().unwrap().push(fence);
            }
        });
        (runner, ticks, fences)
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn solo_runs_on_the_schedule_with_no_fence() {
        let (runner, ticks, fences) = counting(
            DutyLeasing::Solo,
            DutySchedule::every(Duration::from_secs(60)),
        );
        let handle = runner.spawn();
        settle().await;
        assert_eq!(ticks.load(Ordering::SeqCst), 1, "the first tick is at once");
        tokio::time::advance(Duration::from_secs(61)).await;
        settle().await;
        assert_eq!(ticks.load(Ordering::SeqCst), 2);
        assert!(fences.lock().unwrap().iter().all(|f| *f == DutyFence::Solo));
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn of_two_nodes_only_the_holder_ticks() {
        let repo = Arc::new(InMemoryClusterDutyRepository::default());
        let (a, b) = (node().await, node().await);
        let schedule = DutySchedule::every(Duration::from_secs(10));
        let (ra, ta, fa) = counting(leased(&repo, &a), schedule);
        let (rb, tb, _) = counting(leased(&repo, &b), schedule);
        let (ha, hb) = (ra.spawn(), rb.spawn());
        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(3)).await;
            settle().await;
        }
        let (na, nb) = (ta.load(Ordering::SeqCst), tb.load(Ordering::SeqCst));
        assert!(
            (na == 0) != (nb == 0),
            "exactly one node runs the duty: a ran {na}, b ran {nb}"
        );
        let holder = if na > 0 { &ha } else { &hb };
        assert_eq!(holder.held().get(&DUTY), Some(&DutyEpoch(1)));
        if na > 0 {
            assert!(fa.lock().unwrap().iter().all(|f| *f
                == DutyFence::Leased {
                    duty: DUTY,
                    epoch: DutyEpoch(1)
                }));
        }
        ha.shutdown().await;
        hb.shutdown().await;
    }

    /// The holder is fenced: it gives the duty up and the other node takes
    /// it at the next epoch, and ticks.
    #[tokio::test(start_paused = true)]
    async fn a_fenced_holder_hands_the_duty_over_at_the_next_epoch() {
        let repo = Arc::new(InMemoryClusterDutyRepository::default());
        let a = node().await;
        let (ra, ta, _) = counting(
            leased(&repo, &a),
            DutySchedule::every(Duration::from_secs(5)),
        );
        let ha = ra.spawn();
        settle().await;
        assert_eq!(ha.held().get(&DUTY), Some(&DutyEpoch(1)));

        let b = node().await;
        let (rb, tb, fb) = counting(
            leased(&repo, &b),
            DutySchedule::every(Duration::from_secs(5)),
        );
        let hb = rb.spawn();
        tokio::time::advance(Duration::from_secs(6)).await;
        settle().await;
        assert_eq!(
            tb.load(Ordering::SeqCst),
            0,
            "b waits while a holds the duty"
        );

        assert!(a.fence(a.incarnation(), "test"));
        let ran_by_a = ta.load(Ordering::SeqCst);
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(3)).await;
            settle().await;
        }
        assert!(ha.held().is_empty(), "the fenced node holds nothing");
        assert_eq!(hb.held().get(&DUTY), Some(&DutyEpoch(2)));
        assert!(
            tb.load(Ordering::SeqCst) >= 1,
            "b ticks once it holds the duty"
        );
        assert_eq!(ta.load(Ordering::SeqCst), ran_by_a, "a ticks no more");
        assert!(fb.lock().unwrap().iter().all(|f| *f
            == DutyFence::Leased {
                duty: DUTY,
                epoch: DutyEpoch(2)
            }));
        ha.shutdown().await;
        hb.shutdown().await;
    }

    /// A holder that cannot reach the database stops starting ticks once
    /// its lease has run out locally; a peer takes the duty when the lease
    /// has run out in the database.
    #[tokio::test(start_paused = true)]
    async fn a_holder_whose_lease_expires_unrenewed_stops_ticking() {
        let repo = Arc::new(InMemoryClusterDutyRepository::default());
        let a = node().await;
        let (ra, ta, _) = counting(
            leased(&repo, &a),
            DutySchedule::every(Duration::from_secs(3)),
        );
        let ha = ra.spawn();
        settle().await;
        assert_eq!(ta.load(Ordering::SeqCst), 1);

        repo.set_unreachable(true);
        for _ in 0..6 {
            tokio::time::advance(Duration::from_secs(3)).await;
            settle().await;
        }
        assert!(ha.held().is_empty(), "the lease ran out unrenewed");
        let after = ta.load(Ordering::SeqCst);
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(3)).await;
            settle().await;
        }
        assert_eq!(ta.load(Ordering::SeqCst), after);
        ha.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_releases_the_duty_for_a_peer() {
        let repo = Arc::new(InMemoryClusterDutyRepository::default());
        let a = node().await;
        let (ra, _, _) = counting(leased(&repo, &a), DutySchedule::on_acquire());
        let ha = ra.spawn();
        settle().await;
        assert_eq!(ha.held().get(&DUTY), Some(&DutyEpoch(1)));
        ha.shutdown().await;

        let b = node().await;
        let got = repo
            .try_acquire(DUTY, b.process(), Duration::from_secs(15))
            .await
            .unwrap();
        assert_eq!(got, Some(DutyEpoch(2)), "released at once, not after 15 s");
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_once_duty_runs_once_per_acquisition() {
        let repo = Arc::new(InMemoryClusterDutyRepository::default());
        let a = node().await;
        let (ra, ta, _) = counting(leased(&repo, &a), DutySchedule::on_acquire());
        let ha = ra.spawn();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(3)).await;
            settle().await;
        }
        assert_eq!(ta.load(Ordering::SeqCst), 1);
        ha.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn node_duties_run_on_every_node_without_a_lease() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let repo = Arc::new(InMemoryClusterDutyRepository::default());
            let a = node().await;
            let mut runner = ClusterDutyRunner::new(leased(&repo, &a));
            let t = ticks.clone();
            runner.register_node_duty(
                "test-node-duty",
                DutySchedule::every_after_first_interval(Duration::from_secs(60)),
                move || {
                    let t = t.clone();
                    async move {
                        t.fetch_add(1, Ordering::SeqCst);
                    }
                },
            );
            handles.push(runner.spawn());
        }
        settle().await;
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            0,
            "the first run is after one interval"
        );
        tokio::time::advance(Duration::from_secs(61)).await;
        settle().await;
        assert_eq!(ticks.load(Ordering::SeqCst), 2);
        for h in handles {
            h.shutdown().await;
        }
    }
}
