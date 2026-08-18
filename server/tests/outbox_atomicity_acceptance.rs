// SPDX-License-Identifier: MIT
//! Transactional-outbox atomicity acceptance — the commit boundary, end to end.
//!
//! # What this proves
//!
//! The commit boundary (E3 PR-2) makes the append-only event log the single
//! fsync commit point and the state parquet a derived checkpoint. Two invariants
//! must hold across every crash point:
//!
//! 1. **No durable write without its event** — a committed write is always
//!    paired with a committed event (the event is written first, as the barrier).
//! 2. **No event for a write that did not land** — a write the caller was told
//!    FAILED leaves no committed event to resurrect it.
//!
//! # Why a separate battery
//!
//! `write_durability_acceptance.rs` proves the health-gate refuses unpersistable
//! writes. This battery proves the *ordering* the new commit boundary adds:
//! event-first commit, atomic rollback of a checkpoint that could not be written,
//! recovery of a torn checkpoint from the log, and re-publication of a
//! committed-but-unpublished event. Fault injection is deterministic and
//! in-process — a fault-injecting [`StorageBackend`] double for the engine path,
//! and hand-crafted on-disk post-crash state for the [`ParquetBackend`] path (no
//! process kill), matching the house style.

use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::KanbanEngine;
use arrow_kanban_server::events::{CreatedEvent, MutationEvent};
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::lease::Epoch;
use arrow_kanban_server::storage::{
    CommittedEvent, LoadedState, ParquetBackend, Seq, StorageBackend, StoreError,
};

// ── A fault-injecting StorageBackend double ─────────────────────────────────

/// An in-memory backend whose `commit` can be made to fail on demand — the
/// deterministic stand-in for "the append fsync could not be written".
struct FaultBackend {
    committed: Vec<CommittedEvent>,
    committed_seq: Seq,
    /// When true, every `commit` fails at the barrier (as if the append failed).
    fail_commit: bool,
}

impl FaultBackend {
    fn new(fail_commit: bool) -> Self {
        FaultBackend {
            committed: Vec::new(),
            committed_seq: 0,
            fail_commit,
        }
    }
}

impl StorageBackend for FaultBackend {
    fn commit(
        &mut self,
        _store: &KanbanStore,
        _relations: &RelationsStore,
        epoch: Epoch,
        event: &MutationEvent,
    ) -> Result<Seq, StoreError> {
        if self.fail_commit {
            // The barrier failed: NOTHING is committed, the seq does not advance.
            return Err(StoreError::Log(std::io::Error::other(
                "injected commit-log append failure",
            )));
        }
        let seq = self.committed_seq + 1;
        self.committed.push(CommittedEvent {
            seq,
            epoch,
            event: event.clone(),
        });
        self.committed_seq = seq;
        Ok(seq)
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

    fn events_since(&self, after: Seq) -> Result<Vec<CommittedEvent>, StoreError> {
        Ok(self
            .committed
            .iter()
            .filter(|c| c.seq > after)
            .cloned()
            .collect())
    }

    fn committed_seq(&self) -> Seq {
        self.committed_seq
    }
}

fn engine_over(dir: &std::path::Path, backend: FaultBackend) -> KanbanEngine<FaultBackend> {
    KanbanEngine::with_backend(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.to_path_buf(),
        HealthGate::new(),
        backend,
    )
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
}

fn create_payload(title: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "title": title, "item_type": "chore" }))
        .expect("encode")
}

/// A minimal committed Created event for a given id.
fn created(id: &str) -> MutationEvent {
    MutationEvent::Created(CreatedEvent {
        id: id.to_string(),
        title: format!("title {id}"),
        item_type: "chore".to_string(),
        board: "development".to_string(),
        priority: None,
        assignee: None,
        tags: Vec::new(),
        related: Vec::new(),
        depends_on: Vec::new(),
        body: None,
        relationships: Vec::new(),
        agent: None,
    })
}

// ── Invariant 2 via the engine: a failed commit exposes no event ─────────────

/// event-append fails → no event committed, gate degraded, write reported failed.
/// (The write's `route` result may sit in memory for the ≤1 window that
/// `write_durability_acceptance` pins; the DURABLE layer records nothing.)
#[test]
fn a_failed_commit_exposes_no_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = engine_over(dir.path(), FaultBackend::new(true));

    let resp = json(&engine.dispatch("kanban.cmd.create", &create_payload("doomed")));

    assert_eq!(
        resp["code"], "STORE_NOT_DURABLE",
        "a write whose commit fails must be reported failed, got: {resp}"
    );
    assert_eq!(
        engine.committed_seq(),
        0,
        "INVARIANT 2 — a write that did not land committed NO event"
    );
    assert!(
        engine.events_since(0).expect("events_since").is_empty(),
        "no event may be visible for a failed commit"
    );
    assert!(
        engine.health.is_degraded(),
        "a commit failure must degrade the gate (refuse the next write up front)"
    );
}

/// NEGATIVE CONTROL for the test above: with the injection OFF, the identical
/// write COMMITS one event. Proves the fault — not some other refusal — is what
/// suppressed the event (the injection genuinely bites).
#[test]
fn negative_control_without_the_fault_the_commit_lands_its_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = engine_over(dir.path(), FaultBackend::new(false));

    let resp = json(&engine.dispatch("kanban.cmd.create", &create_payload("kept")));

    assert!(resp.get("id").is_some(), "healthy commit must ack: {resp}");
    assert_eq!(
        engine.committed_seq(),
        1,
        "INVARIANT 1 — a durable write committed exactly one event"
    );
    let events = engine.events_since(0).expect("events_since");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.subject_suffix(), "created");
    assert!(!engine.health.is_degraded());
}

// ── Invariant 1 via the real backend: a commit pairs event + checkpoint ──────

/// A successful commit writes BOTH the event (visible to `events_since`) AND the
/// checkpoint (reflected by `load`). Neither exists without the other.
#[test]
fn a_commit_pairs_its_event_with_its_checkpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut backend = ParquetBackend::open(root).expect("open");

    // Build a store consistent with the event, then commit through the backend.
    let mut store = KanbanStore::new();
    store
        .create_item_with_id("CH-1", &input("CH-1"))
        .expect("seed store");
    let relations = RelationsStore::new();
    let seq = backend
        .commit(&store, &relations, 1, &created("CH-1"))
        .expect("commit");
    assert_eq!(seq, 1);

    // The event is durable...
    let events = backend.events_since(0).expect("events_since");
    assert_eq!(events.len(), 1, "the committed event is in the log");
    assert_eq!(events[0].seq, 1);

    // ...and so is the checkpoint (a fresh open+load reflects the item).
    let mut reopened = ParquetBackend::open(root).expect("reopen");
    let (loaded, committed_seq) = reopened.load().expect("load");
    assert_eq!(committed_seq, 1);
    assert!(
        loaded.store.get_item("CH-1").is_ok(),
        "INVARIANT 1 — the checkpoint reflects the committed write"
    );
}

// ── Invariant 2 via the real backend: a torn checkpoint rolls the event back ─

/// A checkpoint that cannot be written IN-PROCESS rolls its just-appended event
/// back off the log, so the refused write leaves no committed event. The store
/// dir is made read-only after the log exists, so the append (existing file)
/// succeeds while the checkpoint (a new `.tmp`) cannot be created — the same
/// `ENOSPC`-shaped split the rollback exists for.
#[cfg(unix)]
#[test]
fn a_checkpoint_that_cannot_be_written_rolls_its_event_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut backend = ParquetBackend::open(root).expect("open");
    let mut store = KanbanStore::new();
    let relations = RelationsStore::new();

    // First commit lands normally (creates the store dir + the log file).
    store
        .create_item_with_id("CH-1", &input("CH-1"))
        .expect("seed");
    backend
        .commit(&store, &relations, 1, &created("CH-1"))
        .expect("first commit");

    // Fault: the store dir is now read-only. The append to the EXISTING log
    // succeeds; the checkpoint's new `.tmp` cannot be created.
    let store_dir = arrow_kanban::persist::data_dir(root).expect("store dir");
    set_writable(&store_dir, false);

    store
        .create_item_with_id("CH-2", &input("CH-2"))
        .expect("seed 2");
    let err = backend
        .commit(&store, &relations, 1, &created("CH-2"))
        .expect_err("checkpoint must fail under the fault");
    set_writable(&store_dir, true);

    assert!(
        matches!(err, StoreError::Checkpoint(_)),
        "the failure must be the checkpoint, got: {err:?}"
    );
    assert_eq!(
        backend.committed_seq(),
        1,
        "the refused commit must NOT advance the committed seq"
    );
    let events = backend.events_since(0).expect("events_since");
    assert_eq!(
        events.len(),
        1,
        "INVARIANT 2 — the rolled-back event left the log at exactly one entry (no phantom CH-2)"
    );
    assert_eq!(events[0].seq, 1);

    // The refused write must not resurrect on a reload either.
    let mut reopened = ParquetBackend::open(root).expect("reopen");
    let (loaded, committed_seq) = reopened.load().expect("load");
    assert_eq!(committed_seq, 1);
    assert!(
        loaded.store.get_item("CH-2").is_err(),
        "a rolled-back write must not appear after a reload"
    );
}

/// NEGATIVE CONTROL for the rollback: with the store WRITABLE, the identical
/// second commit lands — committed_seq advances to 2 and both events persist.
/// Proves the read-only fault is what caused the rollback (the injection bites).
#[cfg(unix)]
#[test]
fn negative_control_a_writable_checkpoint_commits_the_second_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut backend = ParquetBackend::open(root).expect("open");
    let mut store = KanbanStore::new();
    let relations = RelationsStore::new();

    store.create_item_with_id("CH-1", &input("CH-1")).unwrap();
    backend
        .commit(&store, &relations, 1, &created("CH-1"))
        .unwrap();
    store.create_item_with_id("CH-2", &input("CH-2")).unwrap();
    backend
        .commit(&store, &relations, 1, &created("CH-2"))
        .expect("second commit lands when the store is writable");

    assert_eq!(backend.committed_seq(), 2);
    assert_eq!(backend.events_since(0).expect("events_since").len(), 2);
}

// ── Torn checkpoint recovered from the log (crash after append, before ckpt) ─

/// A committed event whose checkpoint never landed (crash after the append
/// fsync, before the checkpoint) is RECOVERED from the log on load — the state
/// is rebuilt by replaying the tail, not silently served stale. The post-crash
/// state is hand-crafted: a real checkpoint at seq 2, plus a committed event at
/// seq 3 appended to the log with no matching checkpoint.
#[test]
fn a_torn_checkpoint_is_recovered_by_replaying_the_log_tail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut backend = ParquetBackend::open(root).expect("open");
    let mut store = KanbanStore::new();
    let relations = RelationsStore::new();

    // Two clean commits: checkpoint + log both reflect seq 2.
    store.create_item_with_id("CH-1", &input("CH-1")).unwrap();
    backend
        .commit(&store, &relations, 1, &created("CH-1"))
        .unwrap();
    store.create_item_with_id("CH-2", &input("CH-2")).unwrap();
    backend
        .commit(&store, &relations, 1, &created("CH-2"))
        .unwrap();

    // NEGATIVE CONTROL (the torn state genuinely differs): a load NOW recovers
    // seq 2 with CH-1/CH-2 and NO CH-3 — proving CH-3 appears below only because
    // it is genuinely in the log, not by accident.
    {
        let mut clean = ParquetBackend::open(root).expect("open clean");
        let (loaded, seq) = clean.load().expect("load clean");
        assert_eq!(seq, 2);
        assert!(
            loaded.store.get_item("CH-3").is_err(),
            "CH-3 not yet logged"
        );
    }

    // Crash shape: the seq-3 event was appended+fsynced (committed) but the
    // process died before the checkpoint — hand-craft it by appending the line
    // WITHOUT updating the checkpoint.
    append_committed_line(root, 3, &created("CH-3"));

    let mut recovered = ParquetBackend::open(root).expect("reopen");
    let (loaded, committed_seq) = recovered.load().expect("load recovers");
    assert_eq!(
        committed_seq, 3,
        "the committed high-water is recovered from the log, not the stale checkpoint"
    );
    assert!(
        loaded.store.get_item("CH-3").is_ok(),
        "the torn-checkpoint write is REBUILT from the log (not lost)"
    );
    // The relay would re-derive + re-publish it (it is past the checkpoint's seq).
    let tail = recovered.events_since(2).expect("events_since");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, 3);
}

// ── Crash after commit, before publish → the event re-publishes (dedup) ──────

/// A committed event the outbox had not yet published is re-derivable after a
/// restart, so the relay re-publishes it. The re-derivation is deterministic
/// (identical event each read), which is what makes the seq-derived
/// `Nats-Msg-Id` (`kanban-event-{seq}`, in `main.rs`) STABLE across restarts so
/// JetStream deduplicates the re-publish. No write is lost between commit and
/// publish.
#[test]
fn a_committed_but_unpublished_event_re_publishes_after_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut backend = ParquetBackend::open(root).expect("open");
    let mut store = KanbanStore::new();
    let relations = RelationsStore::new();

    store.create_item_with_id("CH-1", &input("CH-1")).unwrap();
    backend
        .commit(&store, &relations, 1, &created("CH-1"))
        .unwrap();
    store.create_item_with_id("CH-2", &input("CH-2")).unwrap();
    let k = backend
        .commit(&store, &relations, 1, &created("CH-2"))
        .expect("commit");
    assert_eq!(k, 2);

    // Simulate a crash AFTER commit(seq 2) but BEFORE the outbox published it:
    // the durable published watermark is still at seq 1. On restart the relay
    // drains `events_since(watermark)`.
    let published_watermark: Seq = 1;
    let mut fresh = ParquetBackend::open(root).expect("reopen");
    let (_state, committed_seq) = fresh.load().expect("load");
    assert_eq!(committed_seq, 2);

    let unpublished = fresh
        .events_since(published_watermark)
        .expect("events_since");
    assert_eq!(
        unpublished.len(),
        1,
        "the committed-but-unpublished event is re-derived for re-publish"
    );
    assert_eq!(
        unpublished[0].seq, 2,
        "exactly the un-acked event, no lost write"
    );

    // Deterministic re-derivation → the seq-derived Nats-Msg-Id is stable →
    // JetStream dedups the re-publish.
    let again = fresh
        .events_since(published_watermark)
        .expect("events_since");
    assert_eq!(
        unpublished, again,
        "re-derivation is deterministic (stable Nats-Msg-Id → dedup-safe re-publish)"
    );
}

// ── Every mutating command commits exactly one event (no fail-closed None) ───

/// Each mutating verb, run against a real engine, advances the committed seq by
/// exactly one — i.e. `mutation_event` returned `Some` for it. A mutating
/// command that produced no event would fail-close to STORE_NOT_DURABLE and NOT
/// advance the seq, so this pins that every `is_mutation` command declares its
/// event (the wildcard-free match is compiler-enforced; this checks the reply
/// field extraction actually yields an event for the real reply shapes).
#[test]
fn every_mutating_command_commits_exactly_one_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = KanbanEngine::new(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.path().to_path_buf(),
        HealthGate::new(),
    );

    // create → the item the rest mutate.
    let created = json(&engine.dispatch("kanban.cmd.create", &create_payload("root")));
    let id = created["id"].as_str().expect("created id").to_string();
    assert_eq!(engine.committed_seq(), 1, "create commits an event");

    // Each mutating verb advances the committed seq by exactly one.
    let steps: Vec<(&str, serde_json::Value)> = vec![
        (
            "move",
            serde_json::json!({ "id": id, "status": "in_progress" }),
        ),
        ("comment", serde_json::json!({ "id": id, "text": "note" })),
        ("rank", serde_json::json!({ "id": id, "rank": 1 })),
        (
            "update",
            serde_json::json!({ "id": id, "title": "renamed" }),
        ),
        (
            "ratify",
            serde_json::json!({ "phase_tag": "no-such-phase" }),
        ),
        (
            "relation.add",
            serde_json::json!({ "source_id": id, "target_id": "CH-2", "predicate": "related" }),
        ),
        ("delete", serde_json::json!({ "id": id })),
    ];
    let mut expected = engine.committed_seq();
    for (verb, payload) in steps {
        let bytes = serde_json::to_vec(&payload).expect("encode");
        let resp = json(&engine.dispatch(&format!("kanban.cmd.{verb}"), &bytes));
        assert!(
            resp.get("code").and_then(|c| c.as_str()) != Some("STORE_NOT_DURABLE"),
            "'{verb}' fail-closed (produced no event): {resp}"
        );
        expected += 1;
        assert_eq!(
            engine.committed_seq(),
            expected,
            "'{verb}' must commit exactly one event"
        );
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn input(_id: &str) -> arrow_kanban::crud::CreateItemInput {
    arrow_kanban::crud::CreateItemInput {
        embedding: None,
        title: "t".to_string(),
        item_type: arrow_kanban::item_type::ItemType::Chore,
        priority: None,
        assignee: None,
        tags: Vec::new(),
        related: Vec::new(),
        depends_on: Vec::new(),
        body: None,
    }
}

/// Append one committed log line `{seq, event}` to `_events.log` WITHOUT touching
/// the checkpoint — the hand-crafted "crash after append, before checkpoint" state.
fn append_committed_line(root: &std::path::Path, seq: Seq, event: &MutationEvent) {
    use std::io::Write;
    let dir = arrow_kanban::persist::data_dir(root).expect("store dir");
    let line = serde_json::to_string(&serde_json::json!({ "seq": seq, "event": event }))
        .expect("encode line");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("_events.log"))
        .expect("open log");
    writeln!(f, "{line}").expect("append line");
}

#[cfg(unix)]
fn set_writable(dir: &std::path::Path, writable: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(dir).expect("meta").permissions();
    perms.set_mode(if writable { 0o700 } else { 0o500 });
    std::fs::set_permissions(dir, perms).expect("chmod");
}
