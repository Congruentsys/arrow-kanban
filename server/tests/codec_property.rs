// SPDX-License-Identifier: MIT
//! Codec byte-compat test — the engine reproduces the PRE-refactor wire bytes.
//!
//! The E3 refactor moved the server's string-dispatch into the typed
//! [`KanbanEngine`] with the explicit contract that the wire bytes are
//! **byte-identical** to the former `handlers::dispatch`. This test *proves*
//! that contract against evidence that predates the refactor: a committed
//! fixture ([`GOLDENS_JSON`]) of the exact reply bytes captured from
//! `origin/main`'s string-dispatch, for every compiled verb × three payload
//! shapes (a valid one, the empty `{}`, and malformed JSON).
//!
//! For EVERY compiled wire verb (driven by [`engine::ALL_VERBS`], so a new
//! command cannot be added without appearing here) and each of those payloads,
//! it asserts:
//!
//! > `KanbanEngine::dispatch("kanban.cmd.{verb}", payload)` (normalized for the
//! > one non-deterministic field) **equals the committed golden**.
//!
//! This is *not* the old tautology. The prior version compared `dispatch` to its
//! own definition (`parse` → `apply` → `into_bytes`) — the same expression on
//! both sides, which cannot fail on a broken codec. The golden is an independent
//! artifact captured BEFORE this refactor existed, so a codec change now has to
//! justify itself against bytes it did not produce: corrupting `into_bytes`
//! turns this test RED (verified by mutation, EX-6805 review fix).
//!
//! The one intrinsically non-deterministic response field — `relation.add`'s
//! random `relation_id` UUID, minted identically down the old and new paths — is
//! normalized to `"<uuid>"` in BOTH the golden (at capture time) and the live
//! reply (here), so the comparison stays an exact byte-for-byte match everywhere
//! else.
//!
//! Exhaustiveness is anchored two ways: [`engine::ALL_VERBS`] is the iterated
//! list, and the engine's own matches (`route` / `is_mutation` / `verb_str`) are
//! wildcard-free, so the compiler rejects a new [`engine::KanbanCommand`] variant
//! until it is routed, classified, and named. A golden missing for a compiled
//! verb is a hard failure (never a silent skip).

use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::{self, KanbanEngine};
use arrow_kanban_server::health::HealthGate;
use std::collections::BTreeMap;

/// The committed goldens: reply bytes captured from the PRE-refactor
/// string-dispatch on `origin/main`, keyed `"<verb>|<payload-tag>"`. Compiled in
/// so the test is hermetic (no file I/O, no runtime path resolution).
const GOLDENS_JSON: &str = include_str!("fixtures/codec_goldens.json");

/// A fresh, empty engine rooted at `dir` — same store, relations, and healthy
/// gate the goldens were captured against (a fresh engine per (verb, payload)).
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
/// deliberately NOT required — an honest NOT_FOUND is a stable, captured reply
/// too. **Must stay identical to the capture harness that produced the goldens**
/// (same verb → same payload), or a golden won't match its live reply.
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

/// The three representative payloads for a verb, each with the tag the golden is
/// keyed by: valid, empty `{}`, malformed JSON.
fn payloads_for(verb: &str) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("valid", valid_payload(verb)),
        ("empty", b"{}".to_vec()),
        ("malformed", b"{ this is not valid json".to_vec()),
    ]
}

/// `dispatch` output, taken through the full wire path (subject prefix included).
fn via_dispatch(engine: &mut KanbanEngine, verb: &str, payload: &[u8]) -> Vec<u8> {
    engine.dispatch(&format!("kanban.cmd.{verb}"), payload)
}

/// Mask the one intrinsically non-deterministic response field: `relation.add`
/// mints a fresh UUID `relation_id` inside the handler — so the UUID differs run
/// to run but is orthogonal to the codec under test. Applied to BOTH the golden
/// (at capture time) and the live reply, so the comparison is exact everywhere
/// else. For every other verb the response is a pure function of (verb, payload,
/// seed), so `normalize` returns the bytes UNCHANGED.
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

/// Load the committed goldens as `"<verb>|<tag>" -> reply-bytes-string`.
fn goldens() -> BTreeMap<String, String> {
    serde_json::from_str(GOLDENS_JSON).expect("fixtures/codec_goldens.json is valid JSON")
}

/// THE byte-compat property. Exhaustive over every compiled verb ×
/// {valid, empty, malformed}: the NEW engine reproduces the PRE-refactor golden.
#[test]
fn dispatch_reproduces_pre_refactor_goldens_for_every_verb() {
    assert!(!engine::ALL_VERBS.is_empty(), "ALL_VERBS must not be empty");
    let goldens = goldens();

    for verb in engine::ALL_VERBS {
        for (tag, payload) in payloads_for(verb) {
            let key = format!("{verb}|{tag}");

            // A golden MUST exist for every compiled verb × payload. A missing
            // golden is a hard failure — never a silent skip — so a newly
            // compiled verb cannot slip through un-pinned.
            let golden = goldens.get(&key).unwrap_or_else(|| {
                panic!(
                    "no golden for '{key}' — regenerate fixtures/codec_goldens.json from the \
                     pre-refactor dispatch (a compiled verb must be pinned, never skipped)"
                )
            });

            // Fresh, empty engine per (verb, payload) — exactly how the golden
            // was captured.
            let dir = tempfile::tempdir().expect("tempdir");
            let mut engine = fresh_engine(dir.path());
            let live = via_dispatch(&mut engine, verb, &payload);
            let live_norm = normalize(&live);
            let live_str = String::from_utf8(live_norm).expect("reply is valid UTF-8 JSON");

            assert_eq!(
                &live_str,
                golden,
                "codec regression for '{key}' on payload {:?}:\n  golden (pre-refactor) = {golden}\n  engine (now)          = {live_str}",
                String::from_utf8_lossy(&payload),
            );
        }
    }
}

/// The goldens fixture must cover EXACTLY the compiled verb set — no golden for a
/// verb the engine no longer compiles (a stale, unverifiable pin), and (paired
/// with the missing-golden check above) no compiled verb without a golden.
#[test]
fn goldens_cover_exactly_the_compiled_verbs() {
    let goldens = goldens();
    let expected: std::collections::BTreeSet<String> = engine::ALL_VERBS
        .iter()
        .flat_map(|v| {
            ["valid", "empty", "malformed"]
                .iter()
                .map(move |t| format!("{v}|{t}"))
        })
        .collect();
    let actual: std::collections::BTreeSet<String> = goldens.keys().cloned().collect();

    let stale: Vec<&String> = actual.difference(&expected).collect();
    assert!(
        stale.is_empty(),
        "goldens contain keys for verbs the engine no longer compiles (stale pins): {stale:?}"
    );
    let uncovered: Vec<&String> = expected.difference(&actual).collect();
    assert!(
        uncovered.is_empty(),
        "compiled verbs with no golden: {uncovered:?}"
    );
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

/// `parse` → `apply` → `into_bytes` for the round-trip/unknown checks (no golden
/// needed for these — they assert a property of the parse layer, not wire bytes).
fn via_parse_apply_bytes(verb: &str, payload: &[u8]) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = fresh_engine(dir.path());
    match engine::parse(verb, payload) {
        Ok(cmd) => engine.apply(cmd),
        Err(reply) => reply,
    }
    .into_bytes()
}
