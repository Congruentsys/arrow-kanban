// SPDX-License-Identifier: MIT
//! Proposals as first-class relation SUBJECTS (issue #78): the forward edge
//! spelling works from the PROP side, deleting the inverse-only asymmetry that
//! forced every proposal-anchored edge to be hand-written backwards — an
//! asymmetry so easy to mis-spell that project docs shipped the impossible
//! direction at least once.

use arrow_kanban::crud::{CreateItemInput, KanbanStore};
use arrow_kanban::item_type::ItemType;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::KanbanEngine;
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::storage::ParquetBackend;

fn input(title: &str) -> CreateItemInput {
    CreateItemInput {
        embedding: None,
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

/// The old NOT_FOUND shape is unrepresentable: the forward spelling works, and
/// the edge reads back from BOTH ends.
#[test]
fn a_proposal_subject_writes_the_forward_edge_and_it_reads_back_from_both_ends() {
    let (_tmp, mut engine) = engine_with(&["CH-1"]);

    let up = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"PROP-9001","relate":["closes:CH-1"]}"#,
    ));
    assert!(
        up.get("error").is_none(),
        "the forward spelling must succeed (the old shape was NOT_FOUND): {up}"
    );
    assert!(
        up["relationships_added"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "closes:CH-1")),
        "{up}"
    );

    // Read back from the PROP end...
    let q = json(&engine.dispatch("kanban.cmd.relation.query", br#"{"item_id":"PROP-9001"}"#));
    assert!(q.to_string().contains("CH-1"), "PROP-end readback: {q}");
    // ...and from the item end.
    let q = json(&engine.dispatch("kanban.cmd.relation.query", br#"{"item_id":"CH-1"}"#));
    assert!(
        q.to_string().contains("PROP-9001"),
        "item-end readback: {q}"
    );

    // And --unrelate works from the SUBJECT side too (the repair path that
    // used to be impossible for proposal-anchored edges).
    let un = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"PROP-9001","unrelate":["closes:CH-1"]}"#,
    ));
    assert!(un.get("error").is_none(), "{un}");
    let q = json(&engine.dispatch("kanban.cmd.relation.query", br#"{"item_id":"CH-1"}"#));
    assert!(!q.to_string().contains("PROP-9001"), "edge removed: {q}");
}

/// Relate-only means relate-only: item-field mutations on an entity subject
/// refuse BY NAME (the record lives in its own store), and an unknown prefix
/// is still NOT_FOUND — the leniency is scoped to declared entity classes.
#[test]
fn entity_subjects_are_relate_only_and_unknown_prefixes_still_not_found() {
    let (_tmp, mut engine) = engine_with(&["CH-1"]);

    let up = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"PROP-9001","title":"sneaky retitle"}"#,
    ));
    assert_eq!(up["code"], "ENTITY_RELATE_ONLY", "{up}");

    let up = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"ZZZ-1","relate":["closes:CH-1"]}"#,
    ));
    assert_eq!(
        up["code"], "NOT_FOUND",
        "an undeclared prefix stays refused: {up}"
    );
}

/// Domain/range enforcement is UNCHANGED by the subject widening: `closes` is
/// declared proposal->item, so a CHORE subject still refuses it — the widening
/// admits new subjects, it does not open the vocabulary.
#[test]
fn domain_enforcement_is_unchanged_for_the_widened_subjects() {
    let (_tmp, mut engine) = engine_with(&["CH-1", "CH-2"]);

    let up = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"CH-1","relate":["closes:CH-2"]}"#,
    ));
    assert_eq!(
        up["code"], "INVALID_RELATION",
        "closes from a chore must still refuse (domain is proposal): {up}"
    );

    // And a PROP subject on an item-domain predicate refuses the same way.
    let up = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"PROP-9001","relate":["implements:VY-1"]}"#,
    ));
    assert_eq!(
        up["code"], "INVALID_RELATION",
        "implements is expedition->voyage; a proposal subject refuses: {up}"
    );
}
