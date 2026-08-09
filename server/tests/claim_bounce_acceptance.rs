// SPDX-License-Identifier: MIT
//! Atomic claim/bounce verbs (issue #76): claims stop being comment
//! conventions raced on comment order — the single writer is the
//! compare-and-set point, the loser gets a TYPED refusal naming the holder,
//! and bounce state lives in the store instead of external scripts.

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
        body: Some("the contract text v1".to_string()),
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
            .expect("seed item");
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

/// The item's own battery clause: two racing claims — exactly one winner, one
/// typed refusal naming the holder. (The single writer serializes the race;
/// "racing" means both requests arrive, and the CAS adjudicates — no comment
/// timestamps, no retraction etiquette.)
#[test]
fn two_racing_claims_one_winner_one_typed_refusal_naming_the_holder() {
    let (_tmp, mut engine) = engine_with(&["CH-1"]);

    let win = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-1","agent":"DGX1"}"#));
    assert!(win.get("error").is_none(), "first claim must win: {win}");
    assert_eq!(win["to"], "in_progress");

    let lose = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-1","agent":"Mini"}"#));
    assert_eq!(lose["code"], "CLAIM_HELD", "{lose}");
    assert!(
        lose["error"].as_str().is_some_and(|e| e.contains("DGX1")),
        "the refusal must NAME the holder: {lose}"
    );

    // Re-claiming your own item is idempotent, not a conflict.
    let again = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-1","agent":"DGX1"}"#));
    assert!(
        again.get("error").is_none(),
        "self re-claim is allowed: {again}"
    );
}

/// Bounce returns the item to the pool atomically: backlog + unassigned +
/// the stamp + the line-start marker comment the fleet's counters match.
#[test]
fn bounce_is_atomic_and_the_unrepaired_gate_refuses_the_next_claim() {
    let (_tmp, mut engine) = engine_with(&["CH-2"]);

    let c = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-2","agent":"DGX1"}"#));
    assert!(c.get("error").is_none(), "{c}");

    let b = json(&engine.dispatch(
        "kanban.cmd.bounce",
        br#"{"id":"CH-2","agent":"DGX1","reason":"the cited section contradicts phase 2"}"#,
    ));
    assert!(b.get("error").is_none(), "bounce must succeed: {b}");
    assert_eq!(b["to"], "backlog");
    assert!(
        b["body_sha"].as_str().is_some(),
        "the stamp carries the body hash: {b}"
    );

    // The claim gate is NATIVE now: unrepaired bounce (body unchanged) refuses.
    let refused = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-2","agent":"Mini"}"#));
    assert_eq!(refused["code"], "CLAIM_BOUNCED", "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("UNREPAIRED")),
        "the refusal explains the gate: {refused}"
    );

    // The un-gate is the BODY EDIT — repair the contract and the claim opens.
    let up = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"CH-2","body":"the contract text v2 - phase 2 re-scoped"}"#,
    ));
    assert!(up.get("error").is_none(), "{up}");
    let ok = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-2","agent":"Mini"}"#));
    assert!(
        ok.get("error").is_none(),
        "an edited body un-gates the claim: {ok}"
    );

    // The human notification exists in the exact marker format the fleet's
    // line-anchored bounce counters already match on.
    let shown = json(&engine.dispatch("kanban.cmd.show", br#"{"id":"CH-2"}"#));
    let text = shown.to_string();
    assert!(
        text.contains("[workit bounce — DGX1, server-verb"),
        "the marker comment must exist: {text}"
    );
}

/// The reviewer's reproduction (PR #77 round 1): an ORDINARY later move to
/// backlog — a board-hygiene sweep's normal job — must NOT displace the bounce
/// stamp and disarm the gate. The un-gate is the body edit, never "any
/// subsequent backlog move". Control first, so a never-firing gate cannot pass.
#[test]
fn an_ordinary_backlog_move_does_not_displace_the_bounce_gate() {
    let (_tmp, mut engine) = engine_with(&["CH-9"]);

    let c = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-9","agent":"DGX1"}"#));
    assert!(c.get("error").is_none(), "{c}");
    let b = json(&engine.dispatch(
        "kanban.cmd.bounce",
        br#"{"id":"CH-9","agent":"DGX1","reason":"missing Docs: line"}"#,
    ));
    assert!(b.get("error").is_none(), "{b}");

    // CONTROL: the gate is live before the displacement attempt.
    let refused = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-9","agent":"Mini"}"#));
    assert_eq!(
        refused["code"], "CLAIM_BOUNCED",
        "control — the gate must be live: {refused}"
    );

    // The displacing landing: an ordinary move to backlog with a prose reason.
    let mv = json(&engine.dispatch(
        "kanban.cmd.move",
        br#"{"id":"CH-9","status":"backlog","reason":"housekeeping sweep"}"#,
    ));
    assert!(
        mv.get("error").is_none(),
        "the ordinary move itself is legal: {mv}"
    );

    // THE POINT: the gate still fires — the newest BOUNCE landing governs,
    // not the newest landing overall.
    let refused = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-9","agent":"Mini"}"#));
    assert_eq!(
        refused["code"], "CLAIM_BOUNCED",
        "a non-bounce landing must not disarm the gate: {refused}"
    );

    // And the real un-gate still works.
    let up = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"CH-9","body":"contract v2 - Docs line added"}"#,
    ));
    assert!(up.get("error").is_none(), "{up}");
    let ok = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-9","agent":"Mini"}"#));
    assert!(ok.get("error").is_none(), "the body edit un-gates: {ok}");
}

/// The bounce survives REPLAY: rebuild the store from the committed log and the
/// unrepaired gate still refuses — the stamp rides the event, and the unassign
/// is replayed (an event that left the old assignee in place would resurrect
/// the executor's claim on a rebuilt store).
#[test]
fn the_bounce_gate_and_unassign_survive_snapshot_replay() {
    let (tmp, mut engine) = engine_with(&["CH-3"]);

    let c = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-3","agent":"DGX1"}"#));
    assert!(c.get("error").is_none(), "{c}");
    let b = json(&engine.dispatch(
        "kanban.cmd.bounce",
        br#"{"id":"CH-3","agent":"DGX1","reason":"needs a canon citation"}"#,
    ));
    assert!(b.get("error").is_none(), "{b}");

    // Rebuild a SECOND engine from the same data dir (checkpoint + committed
    // tail replay — the crash-recovery path).
    drop(engine);
    let mut backend = ParquetBackend::open(tmp.path()).expect("reopen");
    use arrow_kanban_server::storage::StorageBackend;
    let (loaded, _seq) = backend.load().expect("reload");
    let mut rebuilt = KanbanEngine::with_backend(
        loaded.store,
        loaded.relations,
        tmp.path().to_path_buf(),
        HealthGate::new(),
        backend,
    );

    let refused = json(&rebuilt.dispatch("kanban.cmd.claim", br#"{"id":"CH-3","agent":"Mini"}"#));
    assert_eq!(
        refused["code"], "CLAIM_BOUNCED",
        "the bounce gate must survive a rebuild: {refused}"
    );
}

#[test]
fn claim_of_a_missing_item_and_bad_payloads_are_typed_errors() {
    let (_tmp, mut engine) = engine_with(&[]);
    let r = json(&engine.dispatch("kanban.cmd.claim", br#"{"id":"CH-404","agent":"DGX1"}"#));
    assert_eq!(r["code"], "NOT_FOUND", "{r}");
    let r = json(&engine.dispatch("kanban.cmd.claim", b"not json"));
    assert_eq!(r["code"], "INVALID_PAYLOAD", "{r}");
    let r = json(&engine.dispatch("kanban.cmd.bounce", b"not json"));
    assert_eq!(r["code"], "INVALID_PAYLOAD", "{r}");
}
