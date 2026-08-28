// SPDX-License-Identifier: MIT
//! Vocabulary hot-reload: a predicate addition reaches ACCEPTANCE in the running
//! process with zero restarts — and every refusal leaves the running set
//! untouched, loudly.
//!
//! Every predicate addition used to gate on restarting the fleet's single
//! writer (a destructive, non-atomic service swap). This acceptance battery
//! pins the replacement path end-to-end at the wire: dispatch the admin verb,
//! then USE the new predicate in the same process.
//!
//! These tests mutate process-global vocabulary state, so they run under one
//! serial lock and each restores the embedded vocabulary before releasing it.

use arrow_kanban::crud::{CreateItemInput, KanbanStore};
use arrow_kanban::item_type::ItemType;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::KanbanEngine;
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::storage::ParquetBackend;

const EMBEDDED_TTL: &str = include_str!("../../ontology/kanban.ttl");

fn reload_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

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

fn restore_embedded(engine: &mut KanbanEngine) {
    let payload = serde_json::json!({ "ttl": EMBEDDED_TTL });
    let reply = json(&engine.dispatch(
        "kanban.cmd.admin_reload_vocab",
        &serde_json::to_vec(&payload).unwrap(),
    ));
    assert!(reply.get("error").is_none(), "restore failed: {reply}");
}

#[test]
fn an_added_predicate_is_usable_in_the_same_process_with_zero_restarts() {
    let _g = reload_lock();
    let (_tmp, mut engine) = engine_with(&["CH-1", "EX-2"]);
    restore_embedded(&mut engine);

    // Baseline: the probe predicate is refused before the reload — on the
    // VALIDATED path (`--relate` through update). Deliberately NOT
    // `relation.add`, which writes without consulting the vocabulary at all
    // (a pre-existing gap this battery documents but does not fix here).
    let add = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"CH-1","relate":["hotReloadProbe:EX-2"]}"#,
    ));
    assert_eq!(
        add["code"], "INVALID_RELATION",
        "the probe predicate must be REFUSED before the reload: {add}"
    );

    // Reload with the embedded ontology EXTENDED by one pair.
    let extended = format!(
        "{EMBEDDED_TTL}\n\nkb:hotReloadProbe a owl:ObjectProperty ;\n    rdfs:label \"hotReloadProbe\" ;\n    rdfs:comment \"hot-reload acceptance probe\" .\n"
    );
    let reply = json(&engine.dispatch(
        "kanban.cmd.admin_reload_vocab",
        &serde_json::to_vec(&serde_json::json!({ "ttl": extended })).unwrap(),
    ));
    assert!(reply.get("error").is_none(), "reload refused: {reply}");
    assert!(
        reply["added"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "hotReloadProbe")),
        "the reply must report the delta by name: {reply}"
    );

    // THE POINT: the new predicate is accepted by the SAME running engine,
    // through the validated path, with zero restarts.
    let add = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"CH-1","relate":["hotReloadProbe:EX-2"]}"#,
    ));
    assert!(
        add.get("error").is_none(),
        "the added predicate must be usable without a restart: {add}"
    );

    restore_embedded(&mut engine);
}

#[test]
fn a_malformed_ttl_is_refused_loudly_and_the_running_set_is_untouched() {
    let _g = reload_lock();
    let (_tmp, mut engine) = engine_with(&["CH-1", "EX-2"]);
    restore_embedded(&mut engine);

    let reply = json(
        &engine.dispatch(
            "kanban.cmd.admin_reload_vocab",
            &serde_json::to_vec(
                &serde_json::json!({ "ttl": "kb:x a owl:ObjectProperty ;\n rdfs:label \"broken" }),
            )
            .unwrap(),
        ),
    );
    assert_eq!(
        reply["code"], "VOCAB_RELOAD_REFUSED",
        "a malformed ttl must refuse with its own code: {reply}"
    );
    assert!(
        reply["error"]
            .as_str()
            .is_some_and(|e| e.contains("untouched")),
        "the refusal must SAY the running set is untouched: {reply}"
    );

    // The running vocabulary still works: a known predicate writes fine
    // through the validated path.
    let add = json(&engine.dispatch(
        "kanban.cmd.update",
        br#"{"id":"CH-1","relate":["related:EX-2"]}"#,
    ));
    assert!(
        add.get("error").is_none(),
        "the running set must survive: {add}"
    );

    restore_embedded(&mut engine);
}

#[test]
fn a_bad_payload_is_an_invalid_payload_error_not_a_crash() {
    let _g = reload_lock();
    let (_tmp, mut engine) = engine_with(&[]);
    let reply = json(&engine.dispatch("kanban.cmd.admin_reload_vocab", b"not json"));
    assert_eq!(reply["code"], "INVALID_PAYLOAD", "{reply}");
}
