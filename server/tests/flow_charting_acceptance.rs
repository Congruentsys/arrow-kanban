// SPDX-License-Identifier: MIT
//! Flow & WIP charting served end-to-end: all four modes render through the
//! real engine against real transitions (moves write the runs audit trail the
//! charts fold), and the mode list is closed — an unknown mode refuses by name.

use arrow_kanban::crud::{CreateItemInput, KanbanStore};
use arrow_kanban::item_type::ItemType;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::KanbanEngine;
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::storage::ParquetBackend;

fn input(title: &str, ty: ItemType) -> CreateItemInput {
    CreateItemInput {
        title: title.to_string(),
        item_type: ty,
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

fn engine() -> (tempfile::TempDir, KanbanEngine) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let root = root.as_path();
    let mut store = KanbanStore::new();
    store
        .create_item_with_id("CH-1", &input("a chore", ItemType::Chore))
        .unwrap();
    store
        .create_item_with_id("EX-2", &input("a feature", ItemType::Expedition))
        .unwrap();
    store
        .create_item_with_id("VY-3", &input("an epic", ItemType::Voyage))
        .unwrap();
    arrow_kanban::persist::save_all(root, &store, &RelationsStore::new()).expect("save_all");
    let mut backend = ParquetBackend::open(root).expect("open");
    use arrow_kanban_server::storage::StorageBackend;
    let (loaded, _seq) = backend.load().expect("load");
    (
        tmp,
        KanbanEngine::with_backend(
            loaded.store,
            loaded.relations,
            root.to_path_buf(),
            HealthGate::new(),
            backend,
        ),
    )
}

fn flow(engine: &mut KanbanEngine, mode: &str) -> serde_json::Value {
    json(&engine.dispatch("flow", format!(r#"{{"mode":"{mode}"}}"#).as_bytes()))
}

#[test]
fn all_four_modes_render_through_the_served_verb() {
    let (_tmp, mut engine) = engine();
    // Real transitions through the real move path — the runs trail the charts fold.
    for (id, to) in [
        ("CH-1", "planning"),
        ("CH-1", "ready"),
        ("CH-1", "in_progress"),
        ("EX-2", "planning"),
    ] {
        let r = json(&engine.dispatch(
            "move",
            format!(r#"{{"id":"{id}","status":"{to}"}}"#).as_bytes(),
        ));
        assert!(r.get("error").is_none(), "seed move {id}->{to}: {r}");
    }
    for mode in ["wip", "aging", "throughput", "cfd"] {
        let out = flow(&mut engine, mode);
        let view = out["view"]
            .as_str()
            .unwrap_or_else(|| panic!("mode {mode} did not render: {out}"));
        assert!(!view.is_empty(), "mode {mode} rendered empty");
    }
    // The wip render carries the in_progress row the seed created.
    let wip = flow(&mut engine, "wip");
    assert!(
        wip["view"].as_str().unwrap().contains("in_progress"),
        "wip must show the seeded in_progress row: {wip}"
    );
    // Aging measures CH-1's time in its CURRENT status from the runs trail.
    let aging = flow(&mut engine, "aging");
    assert!(
        aging["view"].as_str().unwrap().contains("CH-1"),
        "aging must list the in-progress item: {aging}"
    );
}

#[test]
fn unknown_mode_refuses_by_name() {
    let (_tmp, mut engine) = engine();
    let out = flow(&mut engine, "sparkline");
    assert_eq!(out["code"].as_str(), Some("FLOW_MODE"), "{out}");
}
