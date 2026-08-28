// SPDX-License-Identifier: MIT
//! Read-only replica acceptance battery (E3 PR-4, "Mode R+").
//!
//! The replica is the read-side mirror of the write-side commit boundary: it tails
//! the DURABLE committed log, fails closed on a gap, and refuses reads once it
//! cannot confirm currency within a declared staleness bound. This battery proves
//! all four guarantees end to end, in-process and deterministic — a fault-injecting
//! [`ReplicaSource`] double, hand-crafted committed tails, and an injected clock. No
//! NATS, no process kill, no wall-clock sleep.
//!
//! Every fail-closed claim carries BOTH a negative control (the same setup without
//! the fault tails/serves cleanly) AND a real-fault RED control (a disarmed guard
//! goes the WRONG way — silently applying past a gap, or serving an old snapshot as
//! current), so no assertion is a tautology. The RED toggles mirror the house style:
//! `detect_gaps=false` for the contiguity guard (as `extension_persistence`'s
//! `dedup_replay=false` disables its idempotence guard), and an effectively-infinite
//! staleness bound for the time guard.

use arrow_kanban::crud::{CreateItemInput, KanbanStore};
use arrow_kanban::item_type::ItemType;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::KanbanReply;
use arrow_kanban_server::events::{CreatedEvent, MutationEvent};
use arrow_kanban_server::extension::{EngineExtension, ExtCommand, ExtVerbSpec, ExtensionSnapshot};
use arrow_kanban_server::lease::Epoch;
use arrow_kanban_server::replica::{
    Replica, ReplicaError, ReplicaSource, ReplicaState, StalenessBound,
};
use arrow_kanban_server::snapshot::AggregateSnapshot;
use arrow_kanban_server::storage::{CommittedEvent, Seq};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── A fault-injecting ReplicaSource double ───────────────────────────────────

/// An in-memory source whose committed tail may contain a HOLE or begin past the
/// replica's position (a reclaimed tail), whose currency probe and bootstrap can be
/// made to fail (a partition), and whose canonical high-water and snapshot the test
/// reconfigures MID-RUN through shared interior mutability.
#[derive(Clone)]
struct GapSource {
    events: Arc<Mutex<Vec<CommittedEvent>>>,
    canonical_seq: Arc<AtomicU64>,
    reachable: Arc<AtomicBool>,
    bootstrap_ok: Arc<AtomicBool>,
    bootstrap_snapshot: Arc<Mutex<AggregateSnapshot>>,
    /// Counts SUCCESSFUL bootstraps — 1 after open, 2 after one re-bootstrap. The
    /// direct evidence that a gap re-bootstrapped (and a clean tail did not).
    bootstrap_calls: Arc<AtomicU64>,
}

impl GapSource {
    fn new(bootstrap: AggregateSnapshot, canonical: Seq, events: Vec<CommittedEvent>) -> Self {
        GapSource {
            events: Arc::new(Mutex::new(events)),
            canonical_seq: Arc::new(AtomicU64::new(canonical)),
            reachable: Arc::new(AtomicBool::new(true)),
            bootstrap_ok: Arc::new(AtomicBool::new(true)),
            bootstrap_snapshot: Arc::new(Mutex::new(bootstrap)),
            bootstrap_calls: Arc::new(AtomicU64::new(0)),
        }
    }
    fn set_events(&self, events: Vec<CommittedEvent>) {
        *self.events.lock().unwrap() = events;
    }
    fn set_canonical(&self, seq: Seq) {
        self.canonical_seq.store(seq, Ordering::SeqCst);
    }
    fn set_reachable(&self, reachable: bool) {
        self.reachable.store(reachable, Ordering::SeqCst);
    }
    fn set_bootstrap_ok(&self, ok: bool) {
        self.bootstrap_ok.store(ok, Ordering::SeqCst);
    }
    fn set_bootstrap(&self, snap: AggregateSnapshot) {
        *self.bootstrap_snapshot.lock().unwrap() = snap;
    }
    fn bootstrap_calls(&self) -> u64 {
        self.bootstrap_calls.load(Ordering::SeqCst)
    }
}

impl ReplicaSource for GapSource {
    fn events_since(&self, after: Seq) -> Result<Vec<CommittedEvent>, ReplicaError> {
        if !self.reachable.load(Ordering::SeqCst) {
            return Err(ReplicaError::SourceUnreachable("partitioned".into()));
        }
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.seq > after)
            .cloned()
            .collect())
    }

    fn canonical_committed_seq(&self) -> Result<Seq, ReplicaError> {
        if !self.reachable.load(Ordering::SeqCst) {
            return Err(ReplicaError::SourceUnreachable("partitioned".into()));
        }
        Ok(self.canonical_seq.load(Ordering::SeqCst))
    }

    fn bootstrap(&self) -> Result<AggregateSnapshot, ReplicaError> {
        if !self.reachable.load(Ordering::SeqCst) || !self.bootstrap_ok.load(Ordering::SeqCst) {
            return Err(ReplicaError::SourceUnreachable("cannot bootstrap".into()));
        }
        self.bootstrap_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.bootstrap_snapshot.lock().unwrap().clone())
    }
}

// ── An injected clock (millis), shared with the test ─────────────────────────

fn clock_at(cell: &Arc<AtomicU64>) -> impl Fn() -> u64 + Send + 'static {
    let c = cell.clone();
    move || c.load(Ordering::SeqCst)
}

// ── state / event builders ───────────────────────────────────────────────────

fn input() -> CreateItemInput {
    CreateItemInput {
        embedding: None,
        title: "t".into(),
        item_type: ItemType::Chore,
        priority: None,
        assignee: None,
        tags: Vec::new(),
        related: Vec::new(),
        depends_on: Vec::new(),
        body: None,
    }
}

fn store_with(ids: &[&str]) -> KanbanStore {
    let mut s = KanbanStore::new();
    for id in ids {
        s.create_item_with_id(id, &input()).expect("seed store");
    }
    s
}

/// A complete snapshot at `seq` holding `ids` — a hand-crafted bootstrap state.
fn snap_at(seq: Seq, ids: &[&str]) -> AggregateSnapshot {
    AggregateSnapshot::new(&store_with(ids), &RelationsStore::new(), seq, None)
}

/// A committed `Created` event at `seq`/`epoch` for item `id` — a replayable tail entry.
fn created(seq: Seq, epoch: Epoch, id: &str) -> CommittedEvent {
    CommittedEvent {
        seq,
        epoch,
        event: MutationEvent::Created(CreatedEvent {
            id: id.into(),
            title: format!("title {id}"),
            item_type: "chore".into(),
            board: "development".into(),
            priority: None,
            assignee: None,
            tags: Vec::new(),
            related: Vec::new(),
            depends_on: Vec::new(),
            body: None,
            relationships: Vec::new(),
            agent: None,
        }),
    }
}

fn open(source: GapSource, bound: StalenessBound, clock: &Arc<AtomicU64>) -> Replica<GapSource> {
    Replica::with_clock(source, bound, None, true, clock_at(clock)).expect("open replica")
}

/// Open with the contiguity guard DISARMED — the RED control constructor.
fn open_no_gap_guard(
    source: GapSource,
    bound: StalenessBound,
    clock: &Arc<AtomicU64>,
) -> Replica<GapSource> {
    Replica::with_clock(source, bound, None, false, clock_at(clock)).expect("open replica")
}

fn bound_60s() -> StalenessBound {
    StalenessBound::of(Duration::from_secs(60))
}

// ── Test 1 — happy-path bootstrap + tail (the baseline negative control) ──────

#[test]
fn t1_bootstrap_then_tail_stays_live_and_applies_each_event() {
    let src = GapSource::new(
        snap_at(2, &["A", "B"]),
        4,
        vec![
            created(1, 1, "A"),
            created(2, 1, "B"),
            created(3, 1, "C"),
            created(4, 1, "D"),
        ],
    );
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open(src.clone(), bound_60s(), &clock);

    // Bootstrapped at seq 2 with A, B.
    assert_eq!(replica.seq(), 2);
    assert!(replica.is_live());
    assert_eq!(replica.schema_version(), snap_at(0, &[]).schema_version);
    let s0 = replica.read().expect("read");
    assert!(s0.store.get_item("A").is_ok() && s0.store.get_item("B").is_ok());

    // Tail the contiguous 3, 4.
    replica.poll().expect("poll");
    assert_eq!(replica.seq(), 4, "tailed straight through");
    assert!(replica.is_live());
    let s1 = replica.read().expect("read");
    assert_eq!(s1.seq(), 4);
    for id in ["A", "B", "C", "D"] {
        assert!(
            s1.store.get_item(id).is_ok(),
            "{id} reflected in the snapshot"
        );
    }
    assert_eq!(src.bootstrap_calls(), 1, "a clean tail never re-bootstraps");
}

// ── Test 2 — dropped-event recovery (+ negative control + RED control) ────────

#[test]
fn t2_dropped_event_resyncs_and_rebootstraps() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open(src.clone(), bound_60s(), &clock);
    assert_eq!(replica.seq(), 1);

    // The source advances to 4, but seq 3 is DROPPED from the tail (1, 2, _, 4). The
    // re-bootstrap snapshot reflects the full correct state.
    src.set_events(vec![
        created(1, 1, "A"),
        created(2, 1, "B"),
        created(4, 1, "D"),
    ]);
    src.set_bootstrap(snap_at(4, &["A", "B", "C", "D"]));
    src.set_canonical(4);

    replica.poll().expect("poll");
    assert!(replica.is_live(), "the gap was healed");
    assert_eq!(replica.seq(), 4);
    assert_eq!(src.bootstrap_calls(), 2, "open + exactly one re-bootstrap");
    let snap = replica.read().expect("read");
    assert!(
        snap.store.get_item("C").is_ok(),
        "the dropped seq-3 item is present via re-bootstrap — NOT skipped by an apply of [B, D]"
    );
}

#[test]
fn t2_negative_control_a_contiguous_tail_never_resyncs() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open(src.clone(), bound_60s(), &clock);

    // Same growth, but the tail is CONTIGUOUS. A re-bootstrap would pull in this
    // marker; assert it never does.
    src.set_events(vec![
        created(1, 1, "A"),
        created(2, 1, "B"),
        created(3, 1, "C"),
        created(4, 1, "D"),
    ]);
    src.set_bootstrap(snap_at(4, &["REBOOTSTRAP-MARKER"]));
    src.set_canonical(4);

    replica.poll().expect("poll");
    assert_eq!(replica.seq(), 4);
    assert!(replica.is_live());
    assert_eq!(
        src.bootstrap_calls(),
        1,
        "a contiguous tail tails straight through"
    );
    let snap = replica.read().expect("read");
    assert!(
        snap.store.get_item("REBOOTSTRAP-MARKER").is_err(),
        "no re-bootstrap happened"
    );
    assert!(
        snap.store.get_item("C").is_ok(),
        "applied the tail directly"
    );
}

#[test]
fn t2_red_control_disarmed_guard_silently_applies_past_the_hole() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open_no_gap_guard(src.clone(), bound_60s(), &clock);

    src.set_events(vec![
        created(1, 1, "A"),
        created(2, 1, "B"),
        created(4, 1, "D"),
    ]);
    src.set_bootstrap(snap_at(4, &["A", "B", "C", "D"]));
    src.set_canonical(4);

    replica.poll().expect("poll");
    // The hazard: it advanced to 4 by applying [B, D], SILENTLY skipping seq 3 (C).
    assert_eq!(replica.seq(), 4);
    assert_eq!(
        src.bootstrap_calls(),
        1,
        "no resync — the guard was disarmed"
    );
    let snap = replica.read().expect("read");
    assert!(
        snap.store.get_item("C").is_err(),
        "RED: the dropped event's effect is silently LOST — the exact hazard the guard prevents"
    );
    assert!(
        snap.store.get_item("D").is_ok(),
        "yet it applied PAST the gap"
    );
}

// ── Test 3 — expired-retention recovery (+ RED control) ───────────────────────

#[test]
fn t3_expired_retention_leapfrogs_via_rebootstrap() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open(src.clone(), bound_60s(), &clock);
    assert_eq!(replica.seq(), 1);

    // Retention GC reclaimed seqs 1, 2: the earliest available event is now seq 3.
    src.set_events(vec![created(3, 1, "C"), created(4, 1, "D")]);
    src.set_bootstrap(snap_at(4, &["A", "B", "C", "D"]));
    src.set_canonical(4);

    replica.poll().expect("poll");
    assert!(replica.is_live());
    assert_eq!(replica.seq(), 4);
    assert_eq!(
        src.bootstrap_calls(),
        2,
        "leapfrogged the reclaimed span via re-bootstrap"
    );
    let snap = replica.read().expect("read");
    assert_eq!(snap.seq(), 4);
    assert!(
        snap.store.get_item("B").is_ok(),
        "the reclaimed seq-2 item is present via re-bootstrap — a pre-gap state was never served as current"
    );
}

#[test]
fn t3_red_control_disarmed_guard_applies_over_a_reclaimed_tail() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open_no_gap_guard(src.clone(), bound_60s(), &clock);

    src.set_events(vec![created(3, 1, "C"), created(4, 1, "D")]);
    src.set_bootstrap(snap_at(4, &["A", "B", "C", "D"]));
    src.set_canonical(4);

    replica.poll().expect("poll");
    let snap = replica.read().expect("read");
    assert!(
        snap.store.get_item("B").is_err(),
        "RED: seq-2 (B) is silently missing — the disarmed guard applied over a reclaimed tail"
    );
    assert_eq!(src.bootstrap_calls(), 1, "no re-bootstrap");
}

// ── Test 4 — expired-retention / partition REFUSAL (+ RED control) ────────────

#[test]
fn t4_partition_past_bound_refuses_reads() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open(
        src.clone(),
        StalenessBound::of(Duration::from_millis(500)),
        &clock,
    );
    assert!(replica.is_live());
    assert!(replica.read().is_ok());

    // A hard partition: the currency probe AND bootstrap both fail. The clock passes
    // the staleness bound.
    src.set_reachable(false);
    src.set_bootstrap_ok(false);
    src.set_canonical(9); // grows, but unmeasurable while partitioned
    clock.store(1_000 + 501, Ordering::SeqCst);

    replica
        .poll()
        .expect("poll is Ok; the refusal is enforced by read()");
    assert!(
        matches!(replica.state(), ReplicaState::StaleRefused { .. }),
        "flipped to StaleRefused past the bound"
    );
    assert!(
        matches!(replica.read(), Err(ReplicaError::Stale { .. })),
        "a stale replica must refuse reads with a typed Stale error"
    );
}

#[test]
fn t4_red_control_no_effective_time_bound_silently_serves_stale() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    // A staleness bound so large the time guard cannot bite — the RED control.
    let mut replica = open(
        src.clone(),
        StalenessBound::of(Duration::from_secs(86_400)),
        &clock,
    );

    src.set_reachable(false);
    src.set_bootstrap_ok(false);
    clock.store(1_000 + 10_000, Ordering::SeqCst);

    replica.poll().expect("poll");
    assert!(
        replica.is_live(),
        "with no effective time bound the replica stays Live during the partition"
    );
    let snap = replica
        .read()
        .expect("RED: it silently serves the OLD snapshot as current");
    assert_eq!(
        snap.seq(),
        1,
        "RED: an old snapshot handed back as current — the hazard the bound prevents"
    );
}

// ── Test 5 — staleness flips Live -> StaleRefused on partition (+ neg control) ─

#[test]
fn t5_staleness_flips_live_to_stale_refused_on_partition() {
    let src = GapSource::new(
        snap_at(2, &["A", "B"]),
        2,
        vec![created(1, 1, "A"), created(2, 1, "B")],
    );
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open(
        src.clone(),
        StalenessBound::of(Duration::from_millis(1_000)),
        &clock,
    );
    assert!(replica.is_live());

    // Partition; the source keeps advancing (unmeasurable). Clock passes the bound.
    src.set_reachable(false);
    src.set_canonical(7);
    clock.store(1_000 + 1_001, Ordering::SeqCst);

    replica.poll().expect("poll");
    assert!(
        matches!(replica.state(), ReplicaState::StaleRefused { .. }),
        "a partition past the bound refuses — even holding a coherent OLD snapshot"
    );
    assert!(replica.read().is_err());
}

#[test]
fn t5_negative_control_a_reachable_source_stays_live_as_the_clock_advances() {
    let src = GapSource::new(
        snap_at(2, &["A", "B"]),
        2,
        vec![created(1, 1, "A"), created(2, 1, "B")],
    );
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open(
        src.clone(),
        StalenessBound::of(Duration::from_millis(1_000)),
        &clock,
    );

    // The clock races far ahead of any bound, but each poll re-confirms currency
    // against a REACHABLE source, so the replica never crosses it.
    for t in [5_000_u64, 50_000, 5_000_000] {
        clock.store(t, Ordering::SeqCst);
        replica.poll().expect("poll");
        assert!(
            replica.is_live(),
            "a reachable source keeps it Live at t={t}"
        );
        assert!(replica.read().is_ok());
    }
}

// ── Test 6 — epoch split-brain (+ negative control) ───────────────────────────

#[test]
fn t6_epoch_regression_fails_closed() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open(src.clone(), bound_60s(), &clock);

    // A CONTIGUOUS tail whose epoch REGRESSES at seq 4 (a second writer generation).
    src.set_events(vec![
        created(2, 1, "B"),
        created(3, 1, "C"),
        created(4, 0, "D"),
    ]);
    src.set_canonical(4);

    let err = replica
        .poll()
        .expect_err("an epoch regression must fail closed");
    assert!(
        matches!(err, ReplicaError::SplitBrain { at_seq: 4, .. }),
        "split brain at the regressing position: {err}"
    );
    assert!(matches!(replica.state(), ReplicaState::StaleRefused { .. }));
    assert_eq!(
        replica.seq(),
        1,
        "no partial application — the position did not advance"
    );
    assert!(replica.read().is_err());
}

#[test]
fn t6_negative_control_a_monotonic_epoch_tail_applies_cleanly() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut replica = open(src.clone(), bound_60s(), &clock);

    // The same tail, but the epoch is MONOTONIC (a clean failover at seq 4).
    src.set_events(vec![
        created(2, 1, "B"),
        created(3, 1, "C"),
        created(4, 2, "D"),
    ]);
    src.set_canonical(4);

    replica.poll().expect("a monotonic-epoch tail applies");
    assert!(replica.is_live());
    assert_eq!(replica.seq(), 4);
    assert!(replica.read().expect("read").store.get_item("D").is_ok());
}

// ── Test 7 — extension bootstrap round-trip ───────────────────────────────────

/// One row of a minimal, vocabulary-neutral demo extension table.
#[derive(Clone)]
struct DemoRow {
    id: String,
    title: String,
}

/// A minimal replica-side extension: it only needs to REPLAY an `Extension` event
/// and bind its rows into the snapshot (the write-path methods are unused here, so
/// they are trivial). Proves the extension seam compiles and bootstraps read-side.
struct ReplicaDemoExt {
    rows: Arc<Vec<DemoRow>>,
}

struct DemoSnap {
    rows: Arc<Vec<DemoRow>>,
}

impl ExtensionSnapshot for DemoSnap {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl EngineExtension for ReplicaDemoExt {
    fn classify(&self, verb: &str) -> Option<ExtVerbSpec> {
        (verb == "ext.demo.propose").then(|| ExtVerbSpec {
            namespace: "demo".into(),
            is_mutation: true,
        })
    }
    fn apply(&mut self, _verb: &str, _payload: &[u8]) -> Result<KanbanReply, KanbanReply> {
        Ok(KanbanReply::Value(json!({})))
    }
    fn derive_event(&self, _cmd: &ExtCommand, _reply: &KanbanReply) -> Option<MutationEvent> {
        None
    }
    fn replay(
        &mut self,
        namespace: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<(), String> {
        if namespace == "demo" && kind == "propose" {
            let id = payload
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or("replay: missing id")?
                .to_string();
            let title = payload
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if self.rows.iter().any(|r| r.id == id) {
                return Ok(());
            }
            Arc::make_mut(&mut self.rows).push(DemoRow { id, title });
        }
        Ok(())
    }
    fn snapshot(&self) -> Option<Arc<dyn ExtensionSnapshot>> {
        Some(Arc::new(DemoSnap {
            rows: self.rows.clone(),
        }))
    }
}

#[test]
fn t7_extension_bound_replica_bootstraps_and_reflects_a_replayed_event() {
    // A committed tail carrying one Extension event; the core bootstrap is empty at seq 1.
    let ext_event = CommittedEvent {
        seq: 1,
        epoch: 1,
        event: MutationEvent::Extension {
            namespace: "demo".into(),
            kind: "propose".into(),
            payload: json!({ "id": "DEMO-1", "title": "hello" }),
        },
    };
    let src = GapSource::new(snap_at(1, &[]), 1, vec![ext_event]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let ext = Box::new(ReplicaDemoExt {
        rows: Arc::new(Vec::new()),
    });
    let replica = Replica::with_clock(src.clone(), bound_60s(), Some(ext), true, clock_at(&clock))
        .expect("open with extension");

    assert_eq!(replica.seq(), 1);
    assert!(replica.is_live());
    let snap = replica.read().expect("read");
    let ext_bind = snap
        .ext
        .as_ref()
        .expect("the replica snapshot binds the extension table");
    let demo = ext_bind
        .as_any()
        .downcast_ref::<DemoSnap>()
        .expect("the ext bind downcasts to the consumer's DemoSnap type");
    assert_eq!(
        demo.rows.len(),
        1,
        "the replayed extension event seeded exactly one row"
    );
    assert_eq!(demo.rows[0].id, "DEMO-1");
    assert_eq!(demo.rows[0].title, "hello");
}

// ── Test 8 — the optional position (lag) bound (+ negative control) ───────────

#[test]
fn t8_lag_bound_refuses_an_unhealable_gap_too_far_behind() {
    // Open at seq 1; a gap appears; the source is reachable for probes but cannot
    // supply a fresh snapshot, and the lag far exceeds the ceiling.
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let bound = StalenessBound {
        max_staleness: Duration::from_secs(60),
        max_lag_seqs: Some(10),
    };
    let mut replica = open(src.clone(), bound, &clock);

    src.set_events(vec![
        created(1, 1, "A"),
        created(2, 1, "B"),
        created(4, 1, "D"),
    ]); // hole at 3
    src.set_bootstrap_ok(false); // cannot re-bootstrap right now
    src.set_canonical(100); // lag = 99 >> 10

    let err = replica
        .poll()
        .expect_err("an unhealable gap beyond the lag bound fails closed");
    assert!(
        matches!(err, ReplicaError::Gap { .. }),
        "surfaces the unhealable gap: {err}"
    );
    assert!(matches!(replica.state(), ReplicaState::StaleRefused { .. }));
    assert!(replica.read().is_err());
}

#[test]
fn t8_negative_control_within_bounds_tolerates_a_transient_unhealable_gap() {
    let src = GapSource::new(snap_at(1, &["A"]), 1, vec![created(1, 1, "A")]);
    let clock = Arc::new(AtomicU64::new(1_000));
    let bound = StalenessBound {
        max_staleness: Duration::from_secs(60),
        max_lag_seqs: Some(1_000),
    };
    let mut replica = open(src.clone(), bound, &clock);

    src.set_events(vec![
        created(1, 1, "A"),
        created(2, 1, "B"),
        created(4, 1, "D"),
    ]); // hole at 3
    src.set_bootstrap_ok(false);
    src.set_canonical(100); // lag 99 <= 1000, and within the time bound

    replica.poll().expect("poll");
    assert!(
        matches!(replica.state(), ReplicaState::Resyncing { .. }),
        "within BOTH bounds it stays Resyncing on the last known-good snapshot"
    );
    // Reads are still served within the bound — the last coherent snapshot at seq 1.
    let snap = replica.read().expect("read within bound");
    assert_eq!(snap.seq(), 1);
    assert!(snap.store.get_item("A").is_ok());
}
