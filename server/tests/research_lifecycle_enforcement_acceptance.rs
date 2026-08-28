// SPDX-License-Identifier: MIT
//! The served `move` chokepoint enforces the PER-TYPE lifecycle, not only the
//! development board's default one.
//!
//! `handle_move_typed` gated its lifecycle consult on `board == "development"`, and the
//! per-type check (`validate_transition_for_type`) had exactly one caller anywhere — the
//! local-mode CLI. Every served client therefore moved a research item to any status the
//! cross-board `--status` union contained, which is every status any lifecycle can
//! produce: the per-type model DESCRIBED lifecycles while the server ACCEPTED anything.
//!
//! These arms pin the served path itself. The posture is deliberately three-valued: a
//! move BETWEEN two states of the item type's own lifecycle is judged, and a move
//! touching a state OUTSIDE it falls through untouched. That is what makes the gate
//! safe to switch on over a store whose items were created at the board intake state and
//! parked by existing tooling at statuses no per-type list contains — refusing those
//! would break every caller on day one, and a gate that refuses real moves gets bypassed.

use arrow_kanban::crud::{CreateItemInput, KanbanStore};
use arrow_kanban::item_type::ItemType;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::KanbanEngine;
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::storage::ParquetBackend;

fn input(title: &str, item_type: ItemType) -> CreateItemInput {
    CreateItemInput {
        embedding: None,
        title: title.to_string(),
        item_type,
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

fn engine_with(items: &[(&str, ItemType)]) -> (tempfile::TempDir, KanbanEngine) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let mut store = KanbanStore::new();
    for (id, ty) in items {
        store
            .create_item_with_id(id, &input(&format!("title of {id}"), *ty))
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

fn forced(engine: &mut KanbanEngine, id: &str, to: &str) -> serde_json::Value {
    json(&engine.dispatch(
        "move",
        format!(r#"{{"id":"{id}","status":"{to}","force":true}}"#).as_bytes(),
    ))
}

/// The un-claim, end to end at the served chokepoint: an experiment claimed to `running`
/// returns to the head of its own queue. Under a forward-only rule with no served consult
/// at all, this move succeeded for the wrong reason — nothing looked at it. It now
/// succeeds because the lifecycle says so, which is what makes the refusals below
/// trustworthy.
#[test]
fn a_released_experiment_returns_to_its_queue_through_the_served_path() {
    let (_tmp, mut engine) = engine_with(&[("EXPR-1", ItemType::Experiment)]);

    // Items are created at the board intake state, which is outside every per-type
    // lifecycle — the fall-through case, and the ordinary starting position of the 74
    // experiments a live store holds at `backlog`.
    let enter = mv(&mut engine, "EXPR-1", "planned");
    assert!(enter.get("error").is_none(), "backlog -> planned: {enter}");

    let claim = mv(&mut engine, "EXPR-1", "running");
    assert!(claim.get("error").is_none(), "planned -> running: {claim}");

    let release = mv(&mut engine, "EXPR-1", "planned");
    assert!(
        release.get("error").is_none(),
        "running -> planned is the un-claim and must be legal at the served path: {release}"
    );
}

/// The half that must refuse. A terminal state is absorbing, and the refusal NAMES the
/// legal targets (the `--relate` refusal shape the development-board gate already uses).
#[test]
fn resurrecting_a_closed_experiment_is_refused_naming_the_legal_targets() {
    let (_tmp, mut engine) = engine_with(&[("EXPR-2", ItemType::Experiment)]);
    for step in ["planned", "running", "complete"] {
        let ok = mv(&mut engine, "EXPR-2", step);
        assert!(ok.get("error").is_none(), "legal step to {step}: {ok}");
    }

    let refused = mv(&mut engine, "EXPR-2", "running");
    assert_eq!(
        refused["code"].as_str(),
        Some("ILLEGAL_TRANSITION"),
        "complete -> running is a resurrection: {refused}"
    );
    let err = refused["error"].as_str().unwrap_or("");
    assert!(
        err.contains("complete") && err.contains("running"),
        "the refusal names the move: {err}"
    );
    assert!(
        err.contains("abandoned"),
        "the refusal names the legal targets from 'complete': {err}"
    );

    // --force is the audited repair valve, exactly as on the development board.
    let repair = forced(&mut engine, "EXPR-2", "running");
    assert!(
        repair.get("error").is_none(),
        "--force bypasses the per-type gate too: {repair}"
    );
}

/// The fall-through, and the arm that makes this gate deployable. A live store holds
/// research items at statuses no per-type list contains — items are created at the board
/// intake state, and a deployment's own tooling parks and reopens them there. Every one of
/// those moves must keep working untouched; only a move BETWEEN two states of the type's
/// own lifecycle is judged.
#[test]
fn a_state_outside_the_type_lifecycle_falls_through_untouched() {
    let (_tmp, mut engine) = engine_with(&[
        ("EXPR-3", ItemType::Experiment),
        ("H-3", ItemType::Hypothesis),
        ("IDEA-3", ItemType::Idea),
    ]);

    // running -> backlog: the release path a deployment ships today. `backlog` is not an
    // experiment state, so the gate has no opinion and must not invent one.
    for step in ["planned", "running", "backlog"] {
        let out = mv(&mut engine, "EXPR-3", step);
        assert!(
            out.get("error").is_none(),
            "EXPR-3 -> {step} must fall through: {out}"
        );
    }

    // A closed record parked at a board-level terminal state, then reopened. `done` is
    // outside the experiment lifecycle in BOTH directions.
    for step in ["done", "planned"] {
        let out = mv(&mut engine, "EXPR-3", step);
        assert!(
            out.get("error").is_none(),
            "EXPR-3 -> {step} must fall through: {out}"
        );
    }

    // Hypotheses live at `backlog` in bulk on a real store; retiring one and re-parking
    // it must not become an error.
    for step in ["draft", "active", "retired", "backlog"] {
        let out = mv(&mut engine, "H-3", step);
        assert!(
            out.get("error").is_none(),
            "H-3 -> {step} must fall through or be legal: {out}"
        );
    }

    // `refining` is a deliberate const-only status with no place in the idea lifecycle —
    // precisely the shape a strict per-type gate would have broken.
    for step in ["captured", "refining", "captured", "formalized"] {
        let out = mv(&mut engine, "IDEA-3", step);
        assert!(
            out.get("error").is_none(),
            "IDEA-3 -> {step} must fall through or be legal: {out}"
        );
    }
}

/// An idempotent re-move is bookkeeping. It is called out on its own because switching
/// the gate on is exactly where it would have regressed: `complete -> complete` is a
/// no-op every client makes, both states are in the lifecycle, and a forward-only rule
/// answers "not forward" — a refusal manufactured by the gate itself.
#[test]
fn an_idempotent_re_move_is_not_an_error_at_the_served_path() {
    let (_tmp, mut engine) = engine_with(&[("EXPR-4", ItemType::Experiment)]);
    for step in ["planned", "running", "complete"] {
        assert!(mv(&mut engine, "EXPR-4", step).get("error").is_none());
    }
    let again = mv(&mut engine, "EXPR-4", "complete");
    assert!(
        again.get("error").is_none(),
        "complete -> complete is a no-op re-move, not a refusal: {again}"
    );
}

/// The development board is untouched by any of this: it keeps the transition MODEL, not
/// the per-type rule, and its own acceptance battery still owns that behaviour.
#[test]
fn the_development_board_keeps_the_transition_model() {
    let (_tmp, mut engine) = engine_with(&[("CH-5", ItemType::Chore)]);
    let ok = mv(&mut engine, "CH-5", "planning");
    assert!(ok.get("error").is_none(), "backlog -> planning: {ok}");

    let refused = mv(&mut engine, "CH-5", "in_progress");
    assert_eq!(
        refused["code"].as_str(),
        Some("ILLEGAL_TRANSITION"),
        "planning -> in_progress is still the model's refusal: {refused}"
    );
    assert!(
        refused["error"].as_str().unwrap_or("").contains("ready"),
        "still the model's message, naming its targets: {refused}"
    );
}
