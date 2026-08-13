// SPDX-License-Identifier: MIT
//! The lifecycle state + transition model, loaded from the ontology.
//!
//! # Why this exists
//!
//! The development board's states were a flat LIST (`config.yaml: states: backlog ·
//! in_progress · review · done`) with **no transition model**: any state could follow any
//! state, and the only validation anywhere was `--resolution` being terminal-state-gated.
//! The missing planning axis was simulated by TAGS the ready-frontier query had to
//! subtract one by one — each tag arriving as its own item, its own canon paragraph, and
//! its own subtraction, because the axis it substituted for did not exist.
//!
//! This module makes the lifecycle DATA, in the same file (`ontology/kanban.ttl`) and
//! under the same fail-closed loader discipline as the relationship vocabulary in
//! [`crate::relation_vocab`]. The `.ttl` is the source; a malformed file falls back to the
//! compiled LEGACY model (the four historical states, fully permissive — today's observed
//! behaviour), never an opened-up or invented one.
//!
//! # What ships in this module and what deliberately does not
//!
//! This module is the MODEL and its query surface ([`states`], [`is_legal_transition`],
//! [`legal_targets`]). Enforcement — refusing an illegal `move` at the server, naming the
//! legal targets — is the consuming layer's act and lands separately, because the merge of
//! a model is not the rollout of a refusal (the validation actor is the single writer;
//! an un-rebuilt server enforces nothing, loudly or otherwise).
//!
//! # Parsing boundary
//!
//! States are typed `a kb:LifecycleState` and transitions use `kb:transitionsTo` — and
//! NEITHER is `owl:Class` / `owl:ObjectProperty`, deliberately: those two declaration
//! shapes feed the EDGE-vocabulary parsers, so an `owl:`-typed state would become a
//! relate-target class and an `owl:`-typed `transitionsTo` would become a `--relate`
//! predicate. A lifecycle state is neither an edge kind nor an edge target.

use crate::relation_vocab::{kb_local, logical_statements, quoted, split_outside_quotes};

/// One lifecycle state and its legal outgoing transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleState {
    /// Canonical name as stored in the `status` column (`backlog`, `planning`, …).
    pub name: &'static str,
    /// The board this state belongs to (`"development"` for the initial model).
    pub board: &'static str,
    /// Names of the states a legal transition may move TO from here.
    pub transitions_to: &'static [&'static str],
    /// One-line meaning, surfaced in refusal messages by the enforcing layer.
    pub doc: &'static str,
}

/// The verdict of a transition query. Three-valued on purpose: an UNKNOWN state is not
/// the same answer as an illegal move between known states, and the enforcing layer may
/// legitimately treat them differently during migration (an item whose stored status
/// predates the model must not be bricked by it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionVerdict {
    /// The move is in the model.
    Legal,
    /// Both states are known and the edge is absent; carries the legal targets so a
    /// refusal can NAME them (the `--relate` refusal shape).
    Illegal {
        legal_targets: &'static [&'static str],
    },
    /// One or both states are not in the model. Never silently legal, never silently
    /// illegal — the caller decides the migration posture and owns saying so.
    UnknownState,
}

/// The LEGACY fallback: the four historical states, fully permissive among themselves —
/// which is precisely today's observed behaviour (nothing validates a transition), so a
/// malformed `.ttl` degrades to the present, never to an invented stricter or looser
/// model. `planning`/`ready` deliberately do NOT appear here: they exist only in the
/// data, which is what the data-only guard test pins.
const FALLBACK_STATES: &[LifecycleState] = &[
    LifecycleState {
        name: "backlog",
        board: "development",
        transitions_to: &["planning", "ready", "in_progress", "review", "done"],
        doc: "raw intake (legacy fallback model)",
    },
    LifecycleState {
        name: "in_progress",
        board: "development",
        transitions_to: &["backlog", "planning", "ready", "review", "done"],
        doc: "claimed and being built (legacy fallback model)",
    },
    LifecycleState {
        name: "review",
        board: "development",
        transitions_to: &["backlog", "planning", "ready", "in_progress", "done"],
        doc: "under cross-agent review (legacy fallback model)",
    },
    LifecycleState {
        name: "done",
        board: "development",
        transitions_to: &["backlog", "planning", "ready", "in_progress", "review"],
        doc: "terminal with resolution (legacy fallback model)",
    },
];

const KANBAN_TTL: &str = include_str!("../ontology/kanban.ttl");

/// The live model: `kanban.ttl`'s `kb:LifecycleState` declarations, parsed once at first
/// use. Fail-closed — a malformed `.ttl` (or one declaring no states) falls back to
/// [`FALLBACK_STATES`] with a stderr warning.
pub fn states() -> &'static [LifecycleState] {
    static INIT: std::sync::OnceLock<&'static [LifecycleState]> = std::sync::OnceLock::new();
    INIT.get_or_init(|| match parse_lifecycle_states(KANBAN_TTL) {
        Ok(v) if !v.is_empty() => intern_states(v),
        Ok(_) => {
            eprintln!(
                "warning: kanban.ttl declares no kb:LifecycleState — using the compiled legacy state model"
            );
            FALLBACK_STATES
        }
        Err(e) => {
            eprintln!(
                "warning: kanban.ttl state model failed to parse ({e}) — using the compiled legacy state model"
            );
            FALLBACK_STATES
        }
    })
}

/// Look one state up by its canonical name.
pub fn state(name: &str) -> Option<&'static LifecycleState> {
    states().iter().find(|s| s.name == name)
}

/// Is `from -> to` a legal move? `from == to` is ALWAYS legal — idempotent re-moves are
/// live practice (a `move X review` on an item already at `review` happens routinely and
/// refusing it would turn bookkeeping into errors).
pub fn is_legal_transition(from: &str, to: &str) -> TransitionVerdict {
    if from == to {
        return TransitionVerdict::Legal;
    }
    let (Some(f), Some(_t)) = (state(from), state(to)) else {
        return TransitionVerdict::UnknownState;
    };
    if f.transitions_to.contains(&to) {
        TransitionVerdict::Legal
    } else {
        TransitionVerdict::Illegal {
            legal_targets: f.transitions_to,
        }
    }
}

/// The legal targets from a state, or `None` for a state the model does not know.
pub fn legal_targets(from: &str) -> Option<&'static [&'static str]> {
    state(from).map(|s| s.transitions_to)
}

// ── parsing ──────────────────────────────────────────────────────────────────────────

struct OwnedState {
    name: String,
    board: String,
    transitions_to: Vec<String>,
    doc: String,
}

fn parse_lifecycle_states(ttl: &str) -> Result<Vec<OwnedState>, String> {
    let statements = logical_statements(ttl)?;

    let mut out: Vec<OwnedState> = Vec::new();
    for stmt in &statements {
        let stmt = stmt.trim().trim_end_matches('.').trim();
        let mut clauses = split_outside_quotes(stmt, ';').into_iter();
        let head = clauses.next().unwrap_or("").trim();
        let subject = head.split_whitespace().next().unwrap_or("");
        if !subject.starts_with("kb:") || !head.contains("kb:LifecycleState") {
            continue;
        }
        let local = kb_local(subject)?;

        let mut board = String::new();
        let mut label: Option<String> = None;
        let mut doc: Option<String> = None;
        let mut transitions: Vec<String> = Vec::new();
        for clause in clauses {
            let clause = clause.trim();
            if clause.is_empty() {
                continue;
            }
            let (pred, rest) = clause
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("bad clause '{clause}' on kb:{local}"))?;
            match pred {
                "kb:board" => board = quoted(rest)?,
                "rdfs:label" => label = Some(quoted(rest)?),
                "rdfs:comment" => doc = Some(quoted(rest)?),
                "kb:transitionsTo" => {
                    for item in split_outside_quotes(rest, ',') {
                        transitions.push(kb_local(item.trim())?.to_string());
                    }
                }
                // Tolerate future metadata clauses — fail-closed is about the state SET
                // and the transition EDGES, not about extra annotations.
                _ => {}
            }
        }
        out.push(OwnedState {
            name: label.unwrap_or_else(|| local.to_string()),
            board: if board.is_empty() {
                "development".to_string()
            } else {
                board
            },
            transitions_to: transitions,
            doc: doc.unwrap_or_default(),
        });
    }

    // Referential sanity: every transition target must be a declared state. A dangling
    // target is a typo that would silently make a legal move illegal — refuse the whole
    // parse (the fallback takes over, loudly) rather than load a lopsided graph.
    for s in &out {
        for t in &s.transitions_to {
            if !out.iter().any(|q| &q.name == t) {
                return Err(format!(
                    "state '{}' transitions to undeclared state '{t}'",
                    s.name
                ));
            }
        }
    }
    Ok(out)
}

fn intern_states(v: Vec<OwnedState>) -> &'static [LifecycleState] {
    let interned: Vec<LifecycleState> = v
        .into_iter()
        .map(|s| LifecycleState {
            name: Box::leak(s.name.into_boxed_str()),
            board: Box::leak(s.board.into_boxed_str()),
            transitions_to: &*Box::leak(
                s.transitions_to
                    .into_iter()
                    .map(|t| &*Box::leak(t.into_boxed_str()))
                    .collect::<Vec<&'static str>>()
                    .into_boxed_slice(),
            ),
            doc: Box::leak(s.doc.into_boxed_str()),
        })
        .collect();
    &*Box::leak(interned.into_boxed_slice())
}

// ── tests ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The data-only guard (the `decision_obligation_pairs_are_loaded_from_data_only`
    /// shape): `planning` and `ready` exist ONLY in the `.ttl`, so a suite passing after
    /// the state block is deleted proves nothing — this test goes RED when the block is
    /// removed, because the fallback does not carry them.
    #[test]
    fn state_model_is_loaded_from_data_only() {
        let live = states();
        for data_only in ["planning", "ready"] {
            assert!(
                live.iter().any(|s| s.name == data_only),
                "'{data_only}' missing from the live model — the ttl state block was removed or failed to parse"
            );
            assert!(
                !FALLBACK_STATES.iter().any(|s| s.name == data_only),
                "'{data_only}' leaked into the compiled fallback — the data-only proof is void"
            );
        }
    }

    /// Every legacy state survives in the data — the model may grow, never shrink below
    /// the states live items already carry (fallback ⊆ loaded, the vocabulary parity
    /// discipline).
    #[test]
    fn every_fallback_state_is_in_the_loaded_model() {
        let live = states();
        for f in FALLBACK_STATES {
            assert!(
                live.iter().any(|s| s.name == f.name),
                "legacy state '{}' missing from the loaded model — live items carrying it would be bricked",
                f.name
            );
        }
    }

    /// The one transition whose ABSENCE is the feature: unplanned work cannot be
    /// claimed. This is the operator's exact complaint made structural.
    #[test]
    fn planning_to_in_progress_is_refused() {
        match is_legal_transition("planning", "in_progress") {
            TransitionVerdict::Illegal { legal_targets } => {
                assert!(
                    legal_targets.contains(&"ready"),
                    "the refusal must name 'ready' as the way out of planning"
                );
            }
            v => panic!("planning->in_progress must be Illegal with named targets, got {v:?}"),
        }
    }

    /// The release act is legal, and so is the claim from ready.
    #[test]
    fn the_planned_path_is_legal_end_to_end() {
        for (from, to) in [
            ("backlog", "planning"),
            ("planning", "ready"),
            ("ready", "in_progress"),
            ("in_progress", "review"),
            ("review", "done"),
        ] {
            assert_eq!(
                is_legal_transition(from, to),
                TransitionVerdict::Legal,
                "{from}->{to} must be legal"
            );
        }
    }

    /// Migration reality: the legacy claim stays legal; idempotent re-moves stay legal;
    /// an unknown stored status is UnknownState, never silently either verdict.
    #[test]
    fn migration_postures_hold() {
        assert_eq!(
            is_legal_transition("backlog", "in_progress"),
            TransitionVerdict::Legal,
            "the legacy claim path must stay legal during migration"
        );
        assert_eq!(
            is_legal_transition("review", "review"),
            TransitionVerdict::Legal
        );
        assert_eq!(
            is_legal_transition("archived", "done"),
            TransitionVerdict::UnknownState
        );
    }

    /// A dangling transition target refuses the whole parse (fallback takes over) —
    /// a typo must not load as a lopsided graph.
    #[test]
    fn dangling_transition_target_refuses_the_parse() {
        let bad = r#"
kb:alpha a kb:LifecycleState ;
    rdfs:label "alpha" ;
    kb:transitionsTo kb:ghost .
"#;
        assert!(parse_lifecycle_states(bad).is_err());
    }

    /// Removing the state block from the data yields an EMPTY parse (which the loader
    /// converts to the fallback, loudly) — the RED-first arm run against a mutated ttl
    /// rather than by deleting the shipped file.
    #[test]
    fn ttl_without_state_block_parses_empty() {
        let stripped: String = KANBAN_TTL
            .split("LIFECYCLE STATES + TRANSITIONS")
            .next()
            .unwrap()
            .to_string();
        let parsed = parse_lifecycle_states(&stripped).expect("pre-block ttl must still parse");
        assert!(
            parsed.is_empty(),
            "no kb:LifecycleState should survive the block's removal"
        );
    }
}
