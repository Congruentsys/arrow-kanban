// SPDX-License-Identifier: MIT
//! The lifecycle model ENFORCES transitions at the served move (PR #89).
//!
//! PR #87 loaded states + transitions from data and wired the consult into
//! `validate_transition_for_type` — which had no caller on the served path, so
//! the model DESCRIBED transitions while the server ACCEPTED anything: the
//! definition and the behaviour were unrelated things that happened to agree.
//! These arms pin the served chokepoint itself: an illegal development-board
//! transition is refused with a typed error NAMING the legal targets; a legal
//! one passes; `--force` bypasses (the audited repair valve, mirroring the
//! WIP-limit override); unknown states keep their existing semantics.
//!
//! SCOPE: these arms own the DEVELOPMENT board's default lifecycle only. The
//! per-TYPE lifecycles — the same gap, one board over, left open here because
//! this battery's own `unknown_states_keep_todays_semantics` arm is about states
//! and not about types — are owned by
//! `research_lifecycle_enforcement_acceptance.rs`. Neither battery's green says
//! anything about the other's board.

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

fn mv(engine: &mut KanbanEngine, id: &str, to: &str) -> serde_json::Value {
    json(&engine.dispatch(
        "move",
        format!(r#"{{"id":"{id}","status":"{to}"}}"#).as_bytes(),
    ))
}

#[test]
fn illegal_transition_is_refused_naming_the_legal_targets() {
    let (_tmp, mut engine) = engine_with(&["CH-1"]);

    // backlog -> planning is in the model.
    let ok = mv(&mut engine, "CH-1", "planning");
    assert!(
        ok.get("error").is_none(),
        "backlog -> planning is legal: {ok}"
    );

    // planning -> in_progress is NOT (the funnel routes through `ready`).
    let refused = mv(&mut engine, "CH-1", "in_progress");
    let err = refused["error"]
        .as_str()
        .unwrap_or_else(|| panic!("planning -> in_progress must be refused: {refused}"));
    assert!(
        err.contains("ready") && err.contains("planning"),
        "the refusal names the legal targets from 'planning': {err}"
    );
    assert_eq!(
        refused["code"].as_str(),
        Some("ILLEGAL_TRANSITION"),
        "typed refusal: {refused}"
    );

    // The legal funnel is unaffected.
    for step in ["ready", "in_progress"] {
        let ok = mv(&mut engine, "CH-1", step);
        assert!(ok.get("error").is_none(), "legal step to {step}: {ok}");
    }
}

#[test]
fn force_bypasses_the_model_as_the_audited_repair_valve() {
    let (_tmp, mut engine) = engine_with(&["CH-2"]);
    let ok = mv(&mut engine, "CH-2", "planning");
    assert!(ok.get("error").is_none());

    let forced = json(&engine.dispatch(
        "move",
        br#"{"id":"CH-2","status":"in_progress","force":true}"#,
    ));
    assert!(
        forced.get("error").is_none(),
        "--force is the audited escape valve (store recovery needs arbitrary repair moves): {forced}"
    );
}

/// Status EXISTENCE: a junk status is refused naming the set (the typo that used
/// to silently hide an item from every view), while `stale` — the deliberate
/// operator park 132 live items rode in on — is accepted BOTH ways (park and
/// unpark). Landing the refusal without `stale` in the vocabulary would have
/// orphaned those rows on the deployment's next server swap.
#[test]
fn junk_status_refused_but_the_stale_park_survives() {
    let (_tmp, mut engine) = engine_with(&["CH-9"]);

    let junk = mv(&mut engine, "CH-9", "zzz-not-a-status");
    assert_eq!(junk["code"].as_str(), Some("UNKNOWN_STATUS"), "{junk}");
    assert!(
        junk["error"].as_str().unwrap_or("").contains("stale"),
        "the refusal names the valid set (incl. stale): {junk}"
    );

    // The park: backlog -> stale, and the unpark: stale -> backlog.
    let park = mv(&mut engine, "CH-9", "stale");
    assert!(
        park.get("error").is_none(),
        "the operator park must stay legal: {park}"
    );
    let unpark = mv(&mut engine, "CH-9", "backlog");
    assert!(unpark.get("error").is_none(), "unpark to backlog: {unpark}");

    // Typo-adjacent realism: the one-character slip the hazard names.
    let typo = mv(&mut engine, "CH-9", "in_progres");
    assert_eq!(typo["code"].as_str(), Some("UNKNOWN_STATUS"), "{typo}");
}

#[test]
fn unknown_states_keep_todays_semantics() {
    // A state the model does not know falls through to the positional rule — a
    // deployment's custom config state must not break at this chokepoint. `blocked`
    // is in VALID_STATUSES but not a kb:LifecycleState, so the model answers
    // UnknownState and the legacy path decides.
    let (_tmp, mut engine) = engine_with(&["CH-3"]);
    let out = mv(&mut engine, "CH-3", "blocked");
    // Whatever the legacy rule says, it must NOT be the model's typed refusal.
    if let Some(code) = out["code"].as_str() {
        assert_ne!(
            code, "ILLEGAL_TRANSITION",
            "an unknown-to-the-model state must never take the model's refusal path: {out}"
        );
    }
}
