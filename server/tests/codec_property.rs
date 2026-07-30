// SPDX-License-Identifier: MIT
//! Codec property test — the engine's decode/encode is exhaustive and consistent.
//!
//! For EVERY compiled wire verb (driven by [`engine::ALL_VERBS`], so a new
//! command cannot be added without appearing here) and a representative set of
//! payloads (a valid one, the empty `{}`, and a malformed-JSON one), this proves:
//!
//! 1. **`dispatch` == `parse` → `apply` → `into_bytes`.** Two identically-seeded
//!    engines produce byte-identical wire output whether the command flows
//!    through [`KanbanEngine::dispatch`] or is decoded with [`engine::parse`] and
//!    applied by hand. That pins the codec: the JSON bytes are a pure function of
//!    (verb, payload, seed).
//! 2. **Post-state agrees.** After each command the two engines are in the same
//!    observable state (a timestamp-free `list` snapshot) — so a mutation lands
//!    identically down both paths, not just its reply.
//!
//! Exhaustiveness is anchored two ways: [`engine::ALL_VERBS`] is the iterated
//! list, and the engine's own matches (`route` / `is_mutation` / `verb_str`) are
//! wildcard-free, so the compiler rejects a new [`engine::KanbanCommand`] variant
//! until it is routed, classified, and named.

use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::{self, KanbanEngine};
use arrow_kanban_server::health::HealthGate;

/// A fresh, empty engine rooted at `dir`. Two of these seeded the same way are
/// "identically seeded" — same store, same relations, same healthy gate.
fn fresh_engine(dir: &std::path::Path) -> KanbanEngine {
    KanbanEngine::new(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.to_path_buf(),
        HealthGate::new(),
    )
}

/// A payload that deserializes at the *codec* level for `verb` (so `parse`
/// returns `Ok`). Handler-level validity (an item existing, a paper linked) is
/// deliberately NOT required — an honest NOT_FOUND is still applied identically
/// down both paths, which is exactly what the property asserts.
fn valid_payload(verb: &str) -> Vec<u8> {
    let v = match verb {
        "create" => serde_json::json!({ "title": "T", "item_type": "chore" }),
        "move" => serde_json::json!({ "id": "EX-1", "status": "backlog" }),
        "ratify" => serde_json::json!({ "phase_tag": "phase-x" }),
        "update" => serde_json::json!({ "id": "EX-1" }),
        "comment" => serde_json::json!({ "id": "EX-1", "text": "note" }),
        "rank" => serde_json::json!({ "id": "EX-1", "rank": 1 }),
        "show" => serde_json::json!({ "id": "EX-1" }),
        "delete" => serde_json::json!({ "id": "EX-1" }),
        "next-id" => serde_json::json!({ "item_type": "chore" }),
        "hdd.paper" | "hdd.hypothesis" | "hdd.experiment" | "hdd.measure" | "hdd.idea"
        | "hdd.literature" => {
            serde_json::json!({ "title": "X", "paper": 130, "hypothesis": "H130.1" })
        }
        "relation.add" => {
            serde_json::json!({ "source_id": "EX-1", "target_id": "EX-2", "predicate": "related" })
        }
        "relation.query" => serde_json::json!({ "item_id": "EX-1" }),
        "source.push" => serde_json::json!({ "branch": "b", "bundle_b64": "AAAA" }),
        "source.pull" | "source.delete" => serde_json::json!({ "branch": "b" }),
        "hdd.run" | "hdd.run.status" => serde_json::json!({ "experiment_id": "EXPR-1" }),
        "hdd.run.complete" => serde_json::json!({ "experiment_id": "EXPR-1", "run": 1 }),
        // Everything else takes no required fields (or ignores the payload).
        _ => serde_json::json!({}),
    };
    serde_json::to_vec(&v).expect("encode payload")
}

/// The three representative payloads for a verb: valid, empty, malformed JSON.
fn payloads_for(verb: &str) -> Vec<Vec<u8>> {
    vec![
        valid_payload(verb),
        b"{}".to_vec(),
        b"{ this is not valid json".to_vec(),
    ]
}

/// `dispatch` output, taken through the full wire path (subject prefix included).
fn via_dispatch(engine: &mut KanbanEngine, verb: &str, payload: &[u8]) -> Vec<u8> {
    engine.dispatch(&format!("kanban.cmd.{verb}"), payload)
}

/// The same command taken through `parse` → `apply` → `into_bytes` by hand.
fn via_parse_apply(engine: &mut KanbanEngine, verb: &str, payload: &[u8]) -> Vec<u8> {
    match engine::parse(verb, payload) {
        Ok(cmd) => engine.apply(cmd),
        Err(reply) => reply,
    }
    .into_bytes()
}

/// Mask the one intrinsically non-deterministic response field: `relation.add`
/// mints a fresh UUID `relation_id` inside the handler, identically down BOTH
/// the `dispatch` and `parse`+`apply` paths — so the UUID differs run to run but
/// is orthogonal to the codec under test. For every other verb the response is a
/// pure function of (verb, payload, seed), so `normalize` returns the bytes
/// UNCHANGED and the assertion stays an exact byte-for-byte comparison.
fn normalize(bytes: &[u8]) -> Vec<u8> {
    if let Ok(serde_json::Value::Object(mut map)) =
        serde_json::from_slice::<serde_json::Value>(bytes)
        && map.contains_key("relation_id")
    {
        map.insert(
            "relation_id".to_string(),
            serde_json::Value::String("<uuid>".into()),
        );
        return serde_json::to_vec(&serde_json::Value::Object(map)).expect("re-encode");
    }
    bytes.to_vec()
}

/// THE property. Exhaustive over every compiled verb × {valid, empty, malformed}.
#[test]
fn dispatch_equals_parse_then_apply_for_every_verb() {
    assert!(!engine::ALL_VERBS.is_empty(), "ALL_VERBS must not be empty");

    for verb in engine::ALL_VERBS {
        for payload in payloads_for(verb) {
            // Two independently-rooted, identically-seeded engines.
            let dir_a = tempfile::tempdir().expect("tempdir a");
            let dir_b = tempfile::tempdir().expect("tempdir b");
            let mut a = fresh_engine(dir_a.path());
            let mut b = fresh_engine(dir_b.path());

            let out_a = via_dispatch(&mut a, verb, &payload);
            let out_b = via_parse_apply(&mut b, verb, &payload);

            assert_eq!(
                normalize(&out_a),
                normalize(&out_b),
                "codec divergence for verb '{verb}' on payload {:?}:\n  dispatch      = {}\n  parse+apply   = {}",
                String::from_utf8_lossy(&payload),
                String::from_utf8_lossy(&out_a),
                String::from_utf8_lossy(&out_b),
            );

            // Post-state: the two engines must be in the same observable state.
            // `list` is timestamp-free (id/title/status/priority/assignee), so a
            // mutation that landed identically down both paths reads identically.
            let state_a = via_dispatch(&mut a, "list", b"{}");
            let state_b = via_dispatch(&mut b, "list", b"{}");
            assert_eq!(
                state_a,
                state_b,
                "post-state divergence after verb '{verb}' on payload {:?}",
                String::from_utf8_lossy(&payload),
            );
        }
    }
}

/// Every listed verb is a REAL command (never `UNKNOWN_COMMAND`), and its decoded
/// command round-trips back to the same verb string. This is what keeps
/// `ALL_VERBS` honest against the parse/verb_str mapping.
#[test]
fn every_verb_is_known_and_round_trips() {
    for verb in engine::ALL_VERBS {
        // A codec-valid payload must decode, and decode back to THIS verb.
        match engine::parse(verb, &valid_payload(verb)) {
            Ok(cmd) => assert_eq!(
                cmd.verb_str(),
                *verb,
                "verb '{verb}' decoded to a command that names itself '{}'",
                cmd.verb_str()
            ),
            Err(reply) => panic!(
                "verb '{verb}' failed to decode a codec-valid payload: {}",
                String::from_utf8_lossy(&reply.into_bytes())
            ),
        }

        // Even the empty payload must not be reported UNKNOWN for a listed verb
        // (a required-field verb yields INVALID_PAYLOAD, which is fine — the
        // point is the verb itself is recognised).
        let bytes = via_parse_apply_bytes(verb, b"{}");
        assert!(
            !String::from_utf8_lossy(&bytes).contains("UNKNOWN_COMMAND"),
            "verb '{verb}' is in ALL_VERBS but parse reports UNKNOWN_COMMAND"
        );
    }
}

/// Negative control: an unlisted verb IS `UNKNOWN_COMMAND` — so the check above
/// is not vacuous.
#[test]
fn unknown_verb_is_reported_unknown() {
    let bytes = via_parse_apply_bytes("definitely-not-a-verb", b"{}");
    let s = String::from_utf8_lossy(&bytes);
    assert!(
        s.contains("UNKNOWN_COMMAND"),
        "an unlisted verb must be UNKNOWN_COMMAND, got: {s}"
    );
}

/// `parse` → `into_bytes` for the error path (no engine needed for these).
fn via_parse_apply_bytes(verb: &str, payload: &[u8]) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = fresh_engine(dir.path());
    via_parse_apply(&mut engine, verb, payload)
}
