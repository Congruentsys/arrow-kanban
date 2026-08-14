// SPDX-License-Identifier: MIT
//! An edge lives in TWO places — the typed relations table and the flat
//! `depends_on`/`related` projection — and the ADD path writes both in one
//! operation. On REMOVE that property did not hold: each remover cleaned one
//! half and reported success (measured on the live bus: `--unrelate` cleared
//! the flat column and left the typed edge; `relation remove` cleared the
//! typed edge and left the flat column). The halves have different readers —
//! flat feeds `blocked`/`roadmap`/`critical-path` and the ready-frontier
//! subtraction; typed feeds `relation query` and campaign rollups — so which
//! half is stale decides the answer: a phantom blocker on one surface, an
//! invisible dependency on the other. Both directions pinned here, driving
//! the REAL engine dispatch, exactly one arm per remover per half.

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

/// Open the store at `root` exactly as a server restart does — load (which replays
/// any log tail past the checkpoint) and build the engine on the recovered state.
/// Returns the committed high-water alongside, so a test can assert the tail was
/// actually seen rather than assuming it.
fn engine_at(root: &std::path::Path) -> (KanbanEngine, arrow_kanban_server::storage::Seq) {
    use arrow_kanban_server::storage::StorageBackend;
    let mut backend = ParquetBackend::open(root).expect("open");
    let (loaded, seq) = backend.load().expect("load");
    let engine = KanbanEngine::with_backend(
        loaded.store,
        loaded.relations,
        root.to_path_buf(),
        HealthGate::new(),
        backend,
    );
    (engine, seq)
}

fn engine_with(items: &[&str]) -> (tempfile::TempDir, KanbanEngine) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let mut store = KanbanStore::new();
    for id in items {
        store
            .create_item_with_id(id, &input(&format!("title of {id}")))
            .expect("seed item");
    }
    arrow_kanban::persist::save_all(root, &store, &RelationsStore::new()).expect("save_all");
    let (engine, _seq) = engine_at(root);
    (tmp, engine)
}

/// Add `a -dependsOn-> b` through the update relate path (typed + flat in one op).
fn add_edge(engine: &mut KanbanEngine, a: &str, b: &str) {
    let reply = json(&engine.dispatch(
        "update",
        format!(r#"{{"id":"{a}","relate":["dependsOn:{b}"]}}"#).as_bytes(),
    ));
    assert!(
        reply.get("error").is_none(),
        "edge add must succeed: {reply}"
    );
}

fn typed_edge_present(engine: &mut KanbanEngine, a: &str, b: &str) -> bool {
    let q = json(&engine.dispatch(
        "relation.query",
        format!(r#"{{"item_id":"{a}"}}"#).as_bytes(),
    ));
    q["relations"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|r| r["source"] == a && r["target"] == b && r["predicate"] == "dependsOn")
    })
}

fn flat_deps(engine: &mut KanbanEngine, a: &str) -> Vec<String> {
    // `show` replies with a RENDERED `detail` string, not structured fields — the flat
    // column surfaces as a "  Depends on X, Y" line. Parsing that line is deliberate:
    // it is the exact surface `arrow-kanban show` readers see, and the first draft of
    // this file read a `["item"]["depends_on"]` path that does not exist, which made the
    // remover-B flat arm VACUOUSLY GREEN against the unfixed handler (always-empty
    // reader ⇒ "flat half gone" always true) — an assertion that cannot fail proves
    // nothing, caught here by probing the reply before trusting the arm.
    let s = json(&engine.dispatch("show", format!(r#"{{"id":"{a}"}}"#).as_bytes()));
    s["detail"]
        .as_str()
        .unwrap_or("")
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("Depends on "))
        .map(|rest| {
            rest.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn remover_a_unrelate_clears_the_typed_half_too() {
    let (_tmp, mut engine) = engine_with(&["CH-1", "CH-2"]);
    add_edge(&mut engine, "CH-1", "CH-2");
    assert!(
        typed_edge_present(&mut engine, "CH-1", "CH-2"),
        "typed added"
    );
    assert_eq!(flat_deps(&mut engine, "CH-1"), vec!["CH-2"], "flat added");

    let del = json(&engine.dispatch("update", br#"{"id":"CH-1","unrelate":["dependsOn:CH-2"]}"#));
    assert!(del.get("error").is_none(), "unrelate must succeed: {del}");

    assert!(
        flat_deps(&mut engine, "CH-1").is_empty(),
        "remover A: the flat half must be gone"
    );
    assert!(
        !typed_edge_present(&mut engine, "CH-1", "CH-2"),
        "remover A: the TYPED half must be gone too — a half-removed edge keeps \
         answering `relation query` while every flat reader says it is gone"
    );
}

#[test]
fn remover_b_relation_remove_clears_the_flat_half_too() {
    let (_tmp, mut engine) = engine_with(&["CH-1", "CH-2"]);
    add_edge(&mut engine, "CH-1", "CH-2");

    let del = json(&engine.dispatch(
        "relation.remove",
        br#"{"source_id":"CH-1","target_id":"CH-2","predicate":"dependsOn"}"#,
    ));
    assert!(
        del.get("error").is_none(),
        "relation.remove must succeed: {del}"
    );

    assert!(
        !typed_edge_present(&mut engine, "CH-1", "CH-2"),
        "remover B: the typed half must be gone"
    );
    assert!(
        flat_deps(&mut engine, "CH-1").is_empty(),
        "remover B: the FLAT half must be gone too — a phantom `depends_on` keeps the \
         item off the ready frontier for every agent, and a blocker that exists only \
         in the flat projection is invisible to every typed-edge reader"
    );
}

#[test]
fn remover_b_untyped_predicate_still_removes_and_touches_no_flat_column() {
    // `parent` projects into no flat column: the flat subtraction must be a no-op,
    // not an error, and the typed removal must work exactly as before.
    let (_tmp, mut engine) = engine_with(&["CH-1", "CH-2"]);
    let add = json(&engine.dispatch(
        "relation.add",
        br#"{"source_id":"CH-1","target_id":"CH-2","predicate":"parent"}"#,
    ));
    assert!(add.get("error").is_none(), "parent add: {add}");

    let del = json(&engine.dispatch(
        "relation.remove",
        br#"{"source_id":"CH-1","target_id":"CH-2","predicate":"parent"}"#,
    ));
    assert!(del.get("error").is_none(), "parent remove: {del}");
    assert!(
        flat_deps(&mut engine, "CH-1").is_empty(),
        "no flat side effect"
    );
}

#[test]
fn remover_b_absent_triple_still_refuses() {
    // The verb's own contract: an absent triple REFUSES — the flat-half widening
    // must not turn "removed nothing" into success.
    let (_tmp, mut engine) = engine_with(&["CH-1", "CH-2"]);
    let del = json(&engine.dispatch(
        "relation.remove",
        br#"{"source_id":"CH-1","target_id":"CH-2","predicate":"dependsOn"}"#,
    ));
    assert!(
        del.get("error").is_some(),
        "an absent triple must refuse, got: {del}"
    );
}

/// Fixing the HANDLER is not enough: the same edge-lives-in-two-places rule has to
/// hold on the REPLAY path, or the fix survives only until the next restart.
///
/// A checkpoint that trails the commit log replays the tail on load. The
/// `RelationRemoved` arm removed only the typed triple, so recovery from a torn
/// checkpoint put the flat `depends_on` back while `relation query` said the edge
/// was gone — the phantom blocker this battery exists to prevent, reintroduced by
/// the recovery path. Same both-halves defect, one code path over.
///
/// The crash shape is hand-crafted the way the outbox battery does it: commit the
/// edge cleanly (checkpoint + log agree), then append a COMMITTED `relation_removed`
/// line with no matching checkpoint, so `load()` must replay it.
#[test]
fn replay_of_relation_removed_clears_the_flat_half_too() {
    use arrow_kanban_server::events::{MutationEvent, RelationRemovedEvent};
    use std::io::Write;

    // Seed two items and the edge through the real engine, so the on-disk state is
    // whatever production actually writes (typed triple + flat projection).
    let (tmp, mut engine) = engine_with(&["CH-1", "CH-2"]);
    let root = tmp.path().to_path_buf();
    add_edge(&mut engine, "CH-1", "CH-2");
    drop(engine); // release the backend before reopening the same root

    // NEGATIVE CONTROL: before the torn line, a restart still sees BOTH halves. This
    // is what makes the assertions below evidence — without it, a reader that always
    // reported "gone" would pass the real check for the wrong reason.
    let committed_seq = {
        let (mut clean, seq) = engine_at(&root);
        assert!(
            typed_edge_present(&mut clean, "CH-1", "CH-2"),
            "control: the typed edge is present before the torn replay"
        );
        assert_eq!(
            flat_deps(&mut clean, "CH-1"),
            vec!["CH-2".to_string()],
            "control: the flat projection is present before the torn replay"
        );
        seq
    };

    // Crash shape: the removal was appended+fsynced (committed) but the process died
    // before the checkpoint — append the line WITHOUT updating the checkpoint.
    let line = serde_json::to_string(&serde_json::json!({
        "seq": committed_seq + 1,
        "event": MutationEvent::RelationRemoved(RelationRemovedEvent {
            source: "CH-1".to_string(),
            target: "CH-2".to_string(),
            predicate: "dependsOn".to_string(),
            agent: None,
        }),
    }))
    .expect("encode line");
    let dir = arrow_kanban::persist::data_dir(&root).expect("store dir");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("_events.log"))
        .expect("open log");
    writeln!(f, "{line}").expect("append line");
    drop(f);

    let (mut recovered, seq) = engine_at(&root);
    assert_eq!(
        seq,
        committed_seq + 1,
        "the committed high-water comes from the log, not the stale checkpoint"
    );
    assert!(
        !typed_edge_present(&mut recovered, "CH-1", "CH-2"),
        "replay: the typed half must be gone"
    );
    assert!(
        flat_deps(&mut recovered, "CH-1").is_empty(),
        "replay: the FLAT half must be gone too — otherwise every restart from a \
         trailing checkpoint resurrects the phantom `depends_on` the handler fix \
         just removed, and the hot-path fix lasts only until the next recovery"
    );
}
