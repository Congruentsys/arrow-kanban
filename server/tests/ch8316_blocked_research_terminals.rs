// SPDX-License-Identifier: MIT
//! The blocked-item listing must exclude ALL terminal states, not just `done`.
//!
//! Research-board items terminate at `complete` (experiment/literature) or
//! `retired` (hypothesis/measure) and never reach `done`. Before this fix,
//! five complete experiments appeared in blocked output despite being finished.

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

fn mv(engine: &mut KanbanEngine, id: &str, to: &str) -> serde_json::Value {
    json(&engine.dispatch(
        "move",
        format!(r#"{{"id":"{id}","status":"{to}"}}"#).as_bytes(),
    ))
}

fn update_deps(engine: &mut KanbanEngine, id: &str, deps: Vec<&str>) -> serde_json::Value {
    let deps_json: Vec<String> = deps.iter().map(|d| format!(r#""{d}""#)).collect();
    json(&engine.dispatch(
        "update",
        format!(r#"{{"id":"{id}","depends_on":[{}]}}"#, deps_json.join(",")).as_bytes(),
    ))
}

fn blocked(engine: &mut KanbanEngine) -> serde_json::Value {
    json(&engine.dispatch("blocked", b"{}"))
}

#[test]
fn blocked_excludes_all_terminal_states_not_just_done() {
    let (_tmp, mut engine) = engine_with(&[
        "EXPR-8124", // complete experiment (the live fixture from the issue)
        "CH-1",      // stale item (NOT terminal — must stay in blocked)
        "CH-2",      // done item (terminal on dev board)
        "H-1",       // retired hypothesis (terminal on research board)
        "EXPR-2",    // abandoned experiment (terminal on research board)
    ]);

    // Set up dependencies (all unmet — the items they depend on don't exist)
    update_deps(&mut engine, "EXPR-8124", vec!["EX-8087"]);
    update_deps(&mut engine, "CH-1", vec!["CH-9999"]);
    update_deps(&mut engine, "CH-2", vec!["CH-8888"]);
    update_deps(&mut engine, "H-1", vec!["H-7777"]);
    update_deps(&mut engine, "EXPR-2", vec!["EX-6666"]);

    // Move items to their terminal/non-terminal states
    mv(&mut engine, "EXPR-8124", "complete"); // Terminal on research
    mv(&mut engine, "CH-1", "stale"); // NOT terminal
    mv(&mut engine, "CH-2", "done"); // Terminal on dev
    mv(&mut engine, "H-1", "retired"); // Terminal on research
    mv(&mut engine, "EXPR-2", "abandoned"); // Terminal on research

    // Query blocked items
    let result = blocked(&mut engine);
    let count = result["count"].as_u64().unwrap();
    let table = result["table"].as_str().unwrap();

    // The stale item should appear (it's NOT terminal)
    assert!(
        table.contains("CH-1") || table.contains("title of CH-1"),
        "stale items must still appear in blocked (they're not terminal): {table}"
    );

    // Terminal states should NOT appear (use word boundaries to avoid substring matches):
    assert!(
        !table.contains("E EXPR-8124"),
        "complete items must not appear (terminal on research board): {table}"
    );
    assert!(
        !table.contains("C CH-2"),
        "done items must not appear (terminal on dev board): {table}"
    );
    assert!(
        !table.contains("H H-1"),
        "retired items must not appear (terminal on research board): {table}"
    );
    assert!(
        !table.contains("E EXPR-2"),
        "abandoned items must not appear (terminal on research board): {table}"
    );

    // Exactly 1 item should be blocked (the stale one)
    assert_eq!(
        count, 1,
        "Only non-terminal items should be blocked (expected 1 stale item), got: {table}"
    );
}
