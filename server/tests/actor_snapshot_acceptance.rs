// SPDX-License-Identifier: MIT
//! Actor + snapshot coherence acceptance — the read/write serialization contract.
//!
//! # What this proves (E3 PR-3a)
//!
//! The [`KanbanHandle`] serializes every mutation onto one owning task and serves
//! reads from an [`AggregateSnapshot`] the task installs after each durable
//! commit. Three coherence properties must hold:
//!
//! 1. **A snapshot at seq N reflects exactly the commits with seq ≤ N** — a held
//!    snapshot sees the writes up to its seq and none after.
//! 2. **The snapshot's seq advances only after a DURABLE commit** — a mutation
//!    whose commit fails does not advance the seq or become readable (RED-before-
//!    green: the same input over a healthy backend DOES advance it).
//! 3. **A held snapshot is an immutable value** — later commits never mutate it.
//!
//! Plus the wire contract: the actor's reply bytes are byte-identical to calling
//! the engine directly, and the durable-commit seq never leaks into a reply.

use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::actor;
use arrow_kanban_server::engine::KanbanEngine;
use arrow_kanban_server::events::MutationEvent;
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::storage::{CommittedEvent, LoadedState, Seq, StorageBackend, StoreError};

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
}

fn create_payload(title: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "title": title, "item_type": "chore" }))
        .expect("encode")
}

fn healthy_engine(dir: &std::path::Path) -> KanbanEngine {
    KanbanEngine::new(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.to_path_buf(),
        HealthGate::new(),
    )
}

// ── A backend whose commit always fails at the barrier (for property 2) ──────

struct FailBackend {
    committed_seq: Seq,
}

impl StorageBackend for FailBackend {
    fn commit(
        &mut self,
        _store: &KanbanStore,
        _relations: &RelationsStore,
        _event: &MutationEvent,
    ) -> Result<Seq, StoreError> {
        // The barrier fails: nothing is committed, the seq never advances.
        Err(StoreError::Log(std::io::Error::other(
            "injected commit-log append failure",
        )))
    }
    fn load(&mut self) -> Result<(LoadedState, Seq), StoreError> {
        Ok((
            LoadedState {
                store: KanbanStore::new(),
                relations: RelationsStore::new(),
            },
            self.committed_seq,
        ))
    }
    fn events_since(&self, _after: Seq) -> Result<Vec<CommittedEvent>, StoreError> {
        Ok(Vec::new())
    }
    fn committed_seq(&self) -> Seq {
        self.committed_seq
    }
}

// ── 1 + 3: reflects exactly ≤ N, and a held snapshot is immutable ────────────

#[tokio::test]
async fn a_snapshot_reflects_exactly_commits_up_to_its_seq_and_is_immutable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (handle, _join) = actor::spawn(healthy_engine(dir.path()), |_e| {});

    // Fresh board: seq 0, empty.
    assert_eq!(handle.committed_seq(), 0, "a fresh handle is at seq 0");
    assert_eq!(handle.snapshot().store.active_item_count(), 0);

    // Commit 1 → seq 1; the snapshot reflects it.
    let r1 = json(
        &handle
            .dispatch("kanban.cmd.create", &create_payload("first"))
            .await,
    );
    let id1 = r1["id"].as_str().expect("id1").to_string();
    let snap1 = handle.snapshot();
    assert_eq!(
        snap1.seq(),
        1,
        "the snapshot advanced to the durable commit seq"
    );
    assert!(
        snap1.store.get_item(&id1).is_ok(),
        "the seq-1 snapshot reflects commit 1"
    );

    // A READ does not advance the committed seq (no rebuild on a non-mutation).
    let _ = handle.dispatch("kanban.cmd.list", b"{}").await;
    assert_eq!(
        handle.committed_seq(),
        1,
        "a read does not advance the committed seq"
    );

    // Commit 2 → seq 2.
    let r2 = json(
        &handle
            .dispatch("kanban.cmd.create", &create_payload("second"))
            .await,
    );
    let id2 = r2["id"].as_str().expect("id2").to_string();
    assert_eq!(handle.snapshot().seq(), 2);
    assert!(handle.snapshot().store.get_item(&id2).is_ok());

    // The seq-1 snapshot held from before reflects EXACTLY commits ≤ 1: it still
    // has id1 and never sees id2 — a held snapshot is an immutable value.
    assert!(
        snap1.store.get_item(&id1).is_ok(),
        "the held seq-1 snapshot still reflects commit 1"
    );
    assert!(
        snap1.store.get_item(&id2).is_err(),
        "the held seq-1 snapshot does NOT see commit 2 (reflects only seq ≤ 1)"
    );
    assert_eq!(snap1.seq(), 1, "a held snapshot's seq is immutable");
}

// ── 2: the snapshot seq advances ONLY after a durable commit ─────────────────

#[tokio::test]
async fn the_snapshot_seq_advances_only_after_a_durable_commit() {
    // Fault path: the commit fails, so no durable write happens.
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = KanbanEngine::with_backend(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.path().to_path_buf(),
        HealthGate::new(),
        FailBackend { committed_seq: 0 },
    );
    let (handle, _join) = actor::spawn(engine, |_e| {});

    let resp = json(
        &handle
            .dispatch("kanban.cmd.create", &create_payload("doomed"))
            .await,
    );
    assert_eq!(
        resp["code"], "STORE_NOT_DURABLE",
        "a write whose commit fails is reported failed, got: {resp}"
    );
    let after = handle.snapshot();
    assert_eq!(
        after.seq(),
        0,
        "a FAILED commit does not advance the snapshot seq"
    );
    assert_eq!(
        after.store.active_item_count(),
        0,
        "the non-durable write is not visible through the snapshot — a reader never \
         sees a seq that is not durably committed"
    );

    // NEGATIVE CONTROL: the identical input over a HEALTHY backend advances to 1,
    // proving the seq is genuinely tied to durable commit (not always stuck at 0).
    let dir2 = tempfile::tempdir().expect("tempdir");
    let (h2, _j2) = actor::spawn(healthy_engine(dir2.path()), |_e| {});
    let ok = json(
        &h2.dispatch("kanban.cmd.create", &create_payload("kept"))
            .await,
    );
    assert!(ok.get("id").is_some(), "healthy commit must ack: {ok}");
    assert_eq!(
        h2.snapshot().seq(),
        1,
        "a DURABLE commit DOES advance the snapshot seq (control)"
    );
}

// ── Wire contract: reply bytes unchanged, seq never in a reply ───────────────

#[tokio::test]
async fn the_actor_reply_is_byte_identical_to_the_engine_and_carries_no_seq() {
    let payload = create_payload("byte-check");

    // The direct engine path — exactly what the codec goldens pin.
    let d0 = tempfile::tempdir().expect("tempdir");
    let mut engine = healthy_engine(d0.path());
    let direct = engine.dispatch("kanban.cmd.create", &payload);

    // The actor path over a fresh, identical engine.
    let d1 = tempfile::tempdir().expect("tempdir");
    let (handle, _join) = actor::spawn(healthy_engine(d1.path()), |_e| {});
    let via_actor = handle.dispatch("kanban.cmd.create", &payload).await;

    assert_eq!(
        direct, via_actor,
        "the actor returns byte-identical reply bytes to the engine (codec unchanged)"
    );

    let v: serde_json::Value = serde_json::from_slice(&via_actor).expect("reply is JSON");
    assert!(
        v.get("seq").is_none(),
        "the durable-commit seq must NEVER leak into a wire reply"
    );
    assert!(
        v.get("id").is_some(),
        "the create reply still carries its id"
    );
}
