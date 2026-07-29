// SPDX-License-Identifier: MIT
//! CH-6645 — `nk show <ID> --format json` emits the FULL item: its own fields
//! PLUS comments (with timestamps) PLUS status-history transitions. This is what
//! makes the FA-E3 time-to-orient measure (claim transition → first work artifact)
//! computable from the CLI. Tests the server-side rendering
//! ([`export::export_item_json_full`]) against a real store — no live NATS server
//! needed — and asserts the JSON round-trips through a real parser.

use arrow_kanban::{CreateItemInput, ItemType, KanbanStore, export};

fn probe_input() -> CreateItemInput {
    CreateItemInput {
        title: "Time-to-orient probe".into(),
        item_type: ItemType::Chore,
        priority: None,
        assignee: None,
        tags: vec!["v20".into()],
        related: vec![],
        depends_on: vec![],
        body: Some("body text".into()),
    }
}

#[test]
fn show_json_carries_status_history_and_comments_with_timestamps() {
    let mut store = KanbanStore::new();
    let id = store.create_item(&probe_input()).expect("create");

    // A real transition (the "claim") + a comment (the "first work artifact").
    store
        .update_status(&id, "in_progress", Some("Air"), false, None)
        .expect("move");
    store
        .add_comment(&id, "[Air] first artifact", Some("Air"))
        .expect("comment");

    let item = store.get_item(&id).expect("get");
    let comments = store.list_comments(&id);
    let json_str = export::export_item_json_full(&item, &comments, store.runs_batches(), &id);

    // 1. Round-trips through a real JSON parser (was: printed NOTHING at all).
    let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("valid JSON");
    // `--format json` is a 1-element array (the tool's convention); the item is [0].
    let v = &parsed[0];

    // 2. The item's own fields survive the enrichment.
    assert_eq!(v["id"], serde_json::json!(id));
    assert_eq!(v["status"], serde_json::json!("in_progress"));

    // 3. status_history holds BOTH transitions (creation + the move), each with a
    //    real epoch-ms timestamp and the by/forced fields.
    let hist = v["status_history"]
        .as_array()
        .expect("status_history array");
    assert!(
        hist.len() >= 2,
        "creation + move ⇒ ≥2 transitions, got {}",
        hist.len()
    );
    let claim = hist
        .iter()
        .find(|h| h["to"] == serde_json::json!("in_progress"))
        .expect("the claim transition to in_progress");
    let claim_at = claim["at"].as_i64().expect("transition carries epoch-ms");
    assert!(
        claim_at > 0,
        "the claim transition carries a real timestamp"
    );
    assert_eq!(claim["by"], serde_json::json!("Air"));
    assert_eq!(claim["forced"], serde_json::json!(false));

    // 4. comments carry their created_at_ms (the first-artifact signal).
    let cmts = v["comments"].as_array().expect("comments array");
    assert_eq!(cmts.len(), 1);
    let first_artifact_at = cmts[0]["created_at_ms"]
        .as_i64()
        .expect("comment carries a timestamp");
    assert!(first_artifact_at > 0);
    assert_eq!(cmts[0]["author"], serde_json::json!("Air"));

    // 5. THE point (EX-6643): time_to_orient = first-artifact − claim is now
    //    computable from the JSON alone.
    assert!(
        first_artifact_at >= claim_at,
        "the artifact lands at/after the claim — the delta is a real, non-negative measure"
    );
}

#[test]
fn show_json_history_is_scoped_to_the_asked_item_only() {
    // A run belonging to ANOTHER item must never leak into this item's history —
    // the filter is by item_id, not "all runs".
    let mut store = KanbanStore::new();
    let a = store.create_item(&probe_input()).expect("a");
    let b = store.create_item(&probe_input()).expect("b");
    store
        .update_status(&b, "in_progress", Some("Mini"), false, None)
        .expect("move b");

    let item_a = store.get_item(&a).expect("get a");
    let json_a =
        export::export_item_json_full(&item_a, &store.list_comments(&a), store.runs_batches(), &a);
    let parsed: serde_json::Value = serde_json::from_str(&json_a).expect("valid JSON");
    let v = &parsed[0];

    let hist = v["status_history"].as_array().expect("array");
    assert!(
        hist.iter().all(|h| h["by"] != serde_json::json!("Mini")),
        "item A's history must not contain item B's Mini transition"
    );
}
