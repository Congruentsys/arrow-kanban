// SPDX-License-Identifier: MIT
//! Backward-compat + no-double-apply acceptance — the live-Mini upgrade path.
//!
//! # What this proves
//!
//! The checkpoint-manifest fix (E3 PR-3a) replaces the fragile `_checkpoint.seq`
//! marker with a single manifest written as the FINAL commit step, and makes the
//! non-idempotent replays (comment / relation.add / update-relate) idempotent as
//! belt-and-suspenders. Two properties must hold for a store that predates this
//! backend and is then upgraded in place:
//!
//! 1. **A pre-backend store loads unchanged at seq 0.** No event log ⇒ no replay
//!    ⇒ the parquet is served exactly as written (the byte-identical layout).
//! 2. **A trailing checkpoint replays without doubling.** When the checkpoint
//!    record trails the parquet (the crash the old marker mishandled), the
//!    replayed tail — including a comment and a relation.add — must reconstruct
//!    the state with NO duplicate comment or edge.
//!
//! The idempotency tests are RED-before-green by construction: each asserts the
//! raw store API doubles a re-applied write (the hazard), then that the replay
//! path does not (the fix).

use arrow_kanban::crud::{CreateItemInput, KanbanStore};
use arrow_kanban::item_type::ItemType;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::KanbanEngine;
use arrow_kanban_server::events::{CommentedEvent, MutationEvent, RelationAddedEvent};
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::storage::{ParquetBackend, StorageBackend};

fn input() -> CreateItemInput {
    CreateItemInput {
        title: "t".to_string(),
        item_type: ItemType::Chore,
        priority: None,
        assignee: None,
        tags: Vec::new(),
        related: Vec::new(),
        depends_on: Vec::new(),
        body: None,
    }
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
}

// ── 1. A pre-backend store loads unchanged at seq 0 ──────────────────────────

/// A store written by the OLD path (`persist::save_all`, no event log / manifest)
/// opens + loads Ok at `committed_seq == 0`, and its item is served unchanged.
#[test]
fn a_pre_backend_store_loads_unchanged_at_seq_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // Build a PRE-backend store the old way: no event log, no checkpoint marker,
    // no manifest — exactly the on-disk shape a store written before this backend has.
    let mut store = KanbanStore::new();
    store
        .create_item_with_id("CH-1", &input())
        .expect("seed pre-backend item");
    arrow_kanban::persist::save_all(root, &store, &RelationsStore::new()).expect("save_all");

    let store_dir = arrow_kanban::persist::data_dir(root).expect("store dir");
    assert!(
        !store_dir.join("_events.log").exists(),
        "a pre-backend store has NO event log"
    );
    assert!(
        !store_dir.join("_checkpoint.manifest").exists(),
        "a pre-backend store has NO checkpoint manifest"
    );

    let mut backend = ParquetBackend::open(root).expect("open");
    let (loaded, committed_seq) = backend.load().expect("load");
    assert_eq!(
        committed_seq, 0,
        "no log ⇒ no replay ⇒ a pre-backend store has committed nothing"
    );
    assert!(
        loaded.store.get_item("CH-1").is_ok(),
        "the pre-backend item loads unchanged"
    );
}

// ── 2. A trailing checkpoint replays without doubling (upgrade path) ─────────

/// The live-Mini upgrade shape: a pre-backend store is opened and takes several
/// commits through the backend INCLUDING a comment and a relation.add, then a
/// crash leaves the checkpoint record TRAILING the parquet (simulated by removing
/// the manifest so the checkpoint high-water reads 0 — the strongest trailing
/// case). On load the whole tail is replayed onto a parquet that already reflects
/// it, and there must be NO duplicated comment or edge.
#[test]
fn an_upgrade_with_a_trailing_manifest_replays_without_doubling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // A pre-backend store (old path).
    let mut store = KanbanStore::new();
    store.create_item_with_id("CH-1", &input()).expect("seed");
    arrow_kanban::persist::save_all(root, &store, &RelationsStore::new()).expect("save_all");

    // Upgrade in place: the server starts over the pre-backend store, then commits.
    let mut backend = ParquetBackend::open(root).expect("open");
    let (loaded, seq0) = backend.load().expect("load");
    assert_eq!(
        seq0, 0,
        "the upgrade starts from a pre-backend store at seq 0"
    );
    let mut engine = KanbanEngine::with_backend(
        loaded.store,
        loaded.relations,
        root.to_path_buf(),
        HealthGate::new(),
        backend,
    );

    // A comment (seq 1) and a relation.add (seq 2) — the two non-idempotent kinds.
    let c = json(&engine.dispatch(
        "kanban.cmd.comment",
        br#"{"id":"CH-1","text":"note","agent":"A"}"#,
    ));
    assert!(c.get("error").is_none(), "comment must ack: {c}");
    let r = json(&engine.dispatch(
        "kanban.cmd.relation.add",
        br#"{"source_id":"CH-1","target_id":"CH-2","predicate":"related"}"#,
    ));
    assert!(r.get("error").is_none(), "relation.add must ack: {r}");
    assert_eq!(engine.committed_seq(), 2, "two commits landed");
    drop(engine);

    // Simulate the crash: the parquet reflects seq 2 (comment + edge persisted),
    // but the checkpoint record TRAILS — remove the manifest so the high-water
    // reads 0 and the load replays the whole (already-checkpointed) tail.
    let store_dir = arrow_kanban::persist::data_dir(root).expect("store dir");
    std::fs::remove_file(store_dir.join("_checkpoint.manifest")).expect("drop manifest");

    // RED-evidence guard: the tail the load will replay is genuinely non-empty, so
    // the single-count assertions below exercise idempotency — not an empty replay.
    let tail = ParquetBackend::read_events_since(root, 0).expect("events_since");
    assert_eq!(
        tail.len(),
        2,
        "both the comment and the relation.add are in the replayed tail"
    );

    // Load recovers seq 2 with NO doubled comment or edge.
    let mut reopened = ParquetBackend::open(root).expect("reopen");
    let (recovered, committed_seq) = reopened.load().expect("load recovers");
    assert_eq!(
        committed_seq, 2,
        "committed high-water recovered from the log"
    );
    assert_eq!(
        recovered.store.list_comments("CH-1").len(),
        1,
        "the comment is NOT duplicated by the trailing-checkpoint replay"
    );
    assert_eq!(
        recovered.relations.active_count(),
        1,
        "the edge is NOT duplicated by the trailing-checkpoint replay"
    );
}

// ── Focused RED-before-green: the replay guards actually prevent doubling ─────

/// Same input, two paths: the raw `add_comment` API DOUBLES a re-applied comment
/// (the hazard the trailing-checkpoint replay would otherwise hit), while the
/// idempotent replay of the same committed event does NOT.
#[test]
fn comment_replay_is_idempotent_where_raw_add_is_not() {
    // RED: the raw store API doubles a re-applied comment.
    let mut raw = KanbanStore::new();
    raw.create_item_with_id("CH-1", &input()).expect("seed");
    raw.add_comment("CH-1", "note", Some("A")).expect("add 1");
    raw.add_comment("CH-1", "note", Some("A")).expect("add 2");
    assert_eq!(
        raw.list_comments("CH-1").len(),
        2,
        "raw add_comment doubles a re-applied comment — the hazard"
    );

    // GREEN: replaying the SAME committed comment twice (the trailing-checkpoint
    // case) yields exactly one.
    let mut store = KanbanStore::new();
    store.create_item_with_id("CH-1", &input()).expect("seed");
    let mut rels = RelationsStore::new();
    let ev = MutationEvent::Commented(CommentedEvent {
        id: "CH-1".into(),
        text: "note".into(),
        agent: Some("A".into()),
    });
    ev.replay_into(&mut store, &mut rels).expect("replay 1");
    ev.replay_into(&mut store, &mut rels)
        .expect("replay 2 (re-apply)");
    assert_eq!(
        store.list_comments("CH-1").len(),
        1,
        "comment replay is idempotent — no double comment"
    );
}

/// The relation.add counterpart: raw `add_relation` doubles a re-applied edge;
/// the idempotent replay of the same committed event does not.
#[test]
fn relation_add_replay_is_idempotent_where_raw_add_is_not() {
    // RED: raw add_relation doubles.
    let mut raw = RelationsStore::new();
    raw.add_relation("CH-1", "CH-2", "related").expect("add 1");
    raw.add_relation("CH-1", "CH-2", "related").expect("add 2");
    assert_eq!(
        raw.active_count(),
        2,
        "raw add_relation doubles a re-applied edge — the hazard"
    );

    // GREEN: replaying the same committed edge twice yields exactly one.
    let mut store = KanbanStore::new();
    let mut rels = RelationsStore::new();
    let ev = MutationEvent::RelationAdded(RelationAddedEvent {
        source: "CH-1".into(),
        target: "CH-2".into(),
        predicate: "related".into(),
        agent: None,
    });
    ev.replay_into(&mut store, &mut rels).expect("replay 1");
    ev.replay_into(&mut store, &mut rels)
        .expect("replay 2 (re-apply)");
    assert_eq!(
        rels.active_count(),
        1,
        "relation.add replay is idempotent — no double edge"
    );
}
