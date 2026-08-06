// SPDX-License-Identifier: MIT
//! Issue #26 — a typed edge must be READABLE by the agent that wrote it.
//!
//! Found agent-in-the-loop: `relation add` acked rc=0, and the agent then burned
//! 4 of its 16 turns failing to verify its own write — `show` renders no relation
//! section at all, and `relation.query` answers with a bare count (no target, no
//! predicate; `count: 1` is compatible with an edge to anything, of any kind).
//! A write that reports success and cannot be read back is a masking shape.

use arrow_kanban::crud::{CreateItemInput, KanbanStore};
use arrow_kanban::item_type::ItemType;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::KanbanEngine;
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::storage::ParquetBackend;

fn input(title: &str) -> CreateItemInput {
    CreateItemInput {
        title: title.to_string(),
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

fn engine_with(items: &[&str]) -> (tempfile::TempDir, KanbanEngine) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let mut store = KanbanStore::new();
    for id in items {
        store
            .create_item_with_id(id, &input(&format!("title of {id}")))
            .expect("seed");
    }
    arrow_kanban::persist::save_all(root, &store, &RelationsStore::new()).expect("save_all");
    let mut backend = ParquetBackend::open(root).expect("open");
    use arrow_kanban_server::storage::StorageBackend;
    let (loaded, _seq) = backend.load().expect("load");
    let engine = KanbanEngine::with_backend(
        loaded.store,
        loaded.relations,
        root.to_path_buf(),
        HealthGate::new(),
        backend,
    );
    (tmp, engine)
}

/// `relation.query` returns THE EDGES, not a tally: source, target, predicate —
/// enough for the writer to confirm exactly what it wrote. `count` stays for
/// compatibility with existing consumers.
#[test]
fn relation_query_returns_the_edges_themselves() {
    let (_tmp, mut engine) = engine_with(&["CH-1", "EX-2"]);
    let add = json(&engine.dispatch(
        "kanban.cmd.relation.add",
        br#"{"source_id":"CH-1","target_id":"EX-2","predicate":"related"}"#,
    ));
    assert!(add.get("error").is_none(), "relation.add must ack: {add}");

    let q = json(&engine.dispatch("kanban.cmd.relation.query", br#"{"item_id":"CH-1"}"#));
    assert_eq!(q["count"], 1, "count stays, for existing consumers: {q}");

    let rels = q["relations"]
        .as_array()
        .unwrap_or_else(|| panic!("relation.query must return the edges, not a tally: {q}"));
    assert_eq!(rels.len(), 1);
    assert_eq!(
        rels[0]["source"], "CH-1",
        "the writer must see WHAT it wrote: {q}"
    );
    assert_eq!(rels[0]["target"], "EX-2");
    assert_eq!(rels[0]["predicate"], "related");
}

/// Both endpoints see the edge — an incoming edge is as real as an outgoing one.
#[test]
fn relation_query_sees_the_edge_from_both_ends() {
    let (_tmp, mut engine) = engine_with(&["CH-1", "EX-2"]);
    engine.dispatch(
        "kanban.cmd.relation.add",
        br#"{"source_id":"CH-1","target_id":"EX-2","predicate":"blocks"}"#,
    );
    let q = json(&engine.dispatch("kanban.cmd.relation.query", br#"{"item_id":"EX-2"}"#));
    let rels = q["relations"].as_array().expect("edges array");
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0]["source"], "CH-1");
    assert_eq!(rels[0]["predicate"], "blocks");
}

/// ANTI-VACUITY: an edge-free item reports an EMPTY list and count 0 — the reader
/// can distinguish "no edges" from "cannot read edges".
#[test]
fn relation_query_on_an_edge_free_item_is_empty_not_absent() {
    let (_tmp, mut engine) = engine_with(&["CH-1"]);
    let q = json(&engine.dispatch("kanban.cmd.relation.query", br#"{"item_id":"CH-1"}"#));
    assert_eq!(q["count"], 0);
    assert_eq!(
        q["relations"].as_array().map(|a| a.len()),
        Some(0),
        "an empty list is an ANSWER; a missing field is a shrug: {q}"
    );
}

/// `show` renders the typed edges — the obvious verification command must show
/// the write. Outgoing on the source...
#[test]
fn show_renders_outgoing_edges() {
    let (_tmp, mut engine) = engine_with(&["CH-1", "EX-2"]);
    engine.dispatch(
        "kanban.cmd.relation.add",
        br#"{"source_id":"CH-1","target_id":"EX-2","predicate":"related"}"#,
    );
    let s = json(&engine.dispatch("kanban.cmd.show", br#"{"id":"CH-1"}"#));
    let detail = s["detail"].as_str().expect("detail text");
    assert!(
        detail.contains("EX-2") && detail.contains("related"),
        "show must render the edge (target + predicate) so a writer can verify: {detail}"
    );
}

/// ...and incoming on the target, with the direction readable.
#[test]
fn show_renders_incoming_edges() {
    let (_tmp, mut engine) = engine_with(&["CH-1", "EX-2"]);
    engine.dispatch(
        "kanban.cmd.relation.add",
        br#"{"source_id":"CH-1","target_id":"EX-2","predicate":"implements"}"#,
    );
    let s = json(&engine.dispatch("kanban.cmd.show", br#"{"id":"EX-2"}"#));
    let detail = s["detail"].as_str().expect("detail text");
    assert!(
        detail.contains("CH-1") && detail.contains("implements"),
        "the target's show must render the incoming edge: {detail}"
    );
}

/// CONTROL: an edge-free item's show does not grow a phantom edges section.
#[test]
fn show_without_edges_stays_clean() {
    let (_tmp, mut engine) = engine_with(&["CH-1"]);
    let s = json(&engine.dispatch("kanban.cmd.show", br#"{"id":"CH-1"}"#));
    let detail = s["detail"].as_str().expect("detail text");
    assert!(
        !detail.contains("Edges"),
        "no edges -> no edges section (a header over nothing reads as a bug): {detail}"
    );
}

/// CH-7319 — the deletion half: `relation.remove` deletes the exact triple, the
/// removal is READABLE back (query shows it gone), and a nonexistent triple
/// refuses rather than claiming success. The measured need: a false seeded
/// PROP-sourced edge was undeletable through every prior surface.
#[test]
fn relation_remove_round_trip_and_refusal() {
    let (_tmp, mut engine) = engine_with(&["CH-1", "CH-2"]);

    let add = json(&engine.dispatch(
        "relation.add",
        br#"{"source_id":"PROP-9001","target_id":"CH-1","predicate":"leavesRemainder"}"#,
    ));
    assert!(add.get("error").is_none(), "add failed: {add}");

    // Remove the exact triple — the reply says what was removed.
    let rm = json(&engine.dispatch(
        "relation.remove",
        br#"{"source_id":"PROP-9001","target_id":"CH-1","predicate":"leavesRemainder"}"#,
    ));
    assert!(rm.get("error").is_none(), "remove failed: {rm}");
    assert_eq!(rm["removed"], true);
    assert_eq!(rm["source"], "PROP-9001");

    // Readable back: the edge is GONE from both endpoints' queries.
    let q = json(&engine.dispatch("relation.query", br#"{"item_id":"CH-1"}"#));
    assert_eq!(q["count"], 0, "edge must be gone: {q}");

    // A nonexistent triple REFUSES — a remove that claims success on nothing
    // is the false-green shape (the guarded-deleter lesson, CH-7310).
    let rm2 = json(&engine.dispatch(
        "relation.remove",
        br#"{"source_id":"PROP-9001","target_id":"CH-1","predicate":"leavesRemainder"}"#,
    ));
    assert!(
        rm2.get("error").is_some(),
        "removing an absent edge must refuse: {rm2}"
    );
}

/// CH-7319 durability: the removal survives an engine RELOAD from disk — the
/// HZ-7179 lesson verbatim (an in-memory assertion cannot fail on a persistence
/// defect, which is precisely why eleven passing tests once missed one).
#[test]
fn relation_remove_survives_reload() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let mut store = KanbanStore::new();
    store
        .create_item_with_id("CH-1", &input("one"))
        .expect("seed");
    arrow_kanban::persist::save_all(root, &store, &RelationsStore::new()).expect("save_all");

    use arrow_kanban_server::storage::StorageBackend;
    let mut backend = ParquetBackend::open(root).expect("open");
    let (loaded, _seq) = backend.load().expect("load");
    let mut engine = KanbanEngine::with_backend(
        loaded.store,
        loaded.relations,
        root.to_path_buf(),
        HealthGate::new(),
        backend,
    );
    let add = json(&engine.dispatch(
        "relation.add",
        br#"{"source_id":"PROP-9001","target_id":"CH-1","predicate":"leavesRemainder"}"#,
    ));
    assert!(add.get("error").is_none(), "{add}");
    let rm = json(&engine.dispatch(
        "relation.remove",
        br#"{"source_id":"PROP-9001","target_id":"CH-1","predicate":"leavesRemainder"}"#,
    ));
    assert!(rm.get("error").is_none(), "{rm}");
    drop(engine);

    // A FRESH engine from the same directory: the edge must STILL be gone.
    let mut backend2 = ParquetBackend::open(root).expect("reopen");
    let (loaded2, _seq) = backend2.load().expect("reload");
    let mut engine2 = KanbanEngine::with_backend(
        loaded2.store,
        loaded2.relations,
        root.to_path_buf(),
        HealthGate::new(),
        backend2,
    );
    let q = json(&engine2.dispatch("relation.query", br#"{"item_id":"CH-1"}"#));
    assert_eq!(
        q["count"], 0,
        "the removal must survive the reload — a resurrected edge is the \
         HZ-7179 persistence-gap shape: {q}"
    );
}
