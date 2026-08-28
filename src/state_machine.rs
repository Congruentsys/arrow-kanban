// SPDX-License-Identifier: MIT
//! State machine — valid transitions and WIP limit enforcement.
//!
//! Each board has its own state graph. Transitions are validated
//! against the graph. WIP limits are enforced per-state-category.

use crate::config::BoardConfig;

/// Errors from state machine operations.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("Invalid transition: '{from}' → '{to}' is not allowed on board '{board}'")]
    InvalidTransition {
        from: String,
        to: String,
        board: String,
    },

    #[error(
        "Invalid transition: '{from}' → '{to}' is not in the lifecycle model. Legal from '{from}': {targets}"
    )]
    IllegalPerModel {
        from: String,
        to: String,
        targets: String,
    },

    #[error("Invalid state '{state}' for board '{board}'")]
    InvalidState { state: String, board: String },

    #[error("WIP limit reached: {current}/{limit} items at '{status}' (use --force to override)")]
    WipLimitReached {
        status: String,
        current: u32,
        limit: u32,
    },

    #[error(
        "Invalid resolution '{resolution}'. Valid values: completed, superseded, wont_do, duplicate, obsolete, merged, refuted"
    )]
    InvalidResolution { resolution: String },

    #[error(
        "Resolution can only be set on terminal states (done, complete, abandoned, retired), not '{status}'"
    )]
    ResolutionOnNonTerminal { status: String },
}

pub type Result<T> = std::result::Result<T, StateError>;

/// Check if a state transition is valid for the given board.
///
/// The rule is TERMINALITY, not direction: forward moves and idempotent re-moves are
/// always valid, a NON-terminal state may also move backward within its own lifecycle
/// (release / re-plan / rework), and a TERMINAL state is absorbing. `--force` is the
/// audited valve at every chokepoint that consults this.
pub fn validate_transition(board: &BoardConfig, from: &str, to: &str) -> Result<()> {
    validate_transition_for_type(board, from, to, None)
}

/// Check if a state transition is valid for a specific item type on the board.
///
/// If `item_type` is provided and the board has `type_states` for that type,
/// validation uses the type-specific state list. Otherwise falls back to
/// board-level states.
pub fn validate_transition_for_type(
    board: &BoardConfig,
    from: &str,
    to: &str,
    item_type: Option<&str>,
) -> Result<()> {
    // The EXPLICIT, data-driven transition graph (ontology/kanban.ttl via
    // crate::state_model) is consulted FIRST for the development board's DEFAULT
    // lifecycle. It is the one mechanism, now fed by data:
    //   Legal        -> Ok, even if the deployment's config.yaml states list lags the
    //                   model (planning/ready become usable the moment the binary
    //                   carries them — config is presentation, the model is law);
    //   Illegal      -> refused NAMING the legal targets (the --relate refusal shape);
    //   UnknownState -> fall through to the positional rule below unchanged — a custom
    //                   config state the model does not know keeps today's semantics,
    //                   and per-type research lifecycles (type_states) never enter the
    //                   graph path at all.
    let uses_type_override = item_type.is_some_and(|t| board.type_states.contains_key(t));
    if board.name == "development" && !uses_type_override {
        use crate::state_model::{TransitionVerdict, is_legal_transition};
        match is_legal_transition(from, to) {
            TransitionVerdict::Legal => return Ok(()),
            TransitionVerdict::Illegal { legal_targets } => {
                return Err(StateError::IllegalPerModel {
                    from: from.to_string(),
                    to: to.to_string(),
                    targets: legal_targets.join(", "),
                });
            }
            TransitionVerdict::UnknownState => {}
        }
    }

    let states = match item_type {
        Some(t) => board.states_for_type(t),
        None => &board.states,
    };

    let from_valid = states.iter().any(|s| s == from);
    let to_valid = states.iter().any(|s| s == to);

    if !from_valid {
        return Err(StateError::InvalidState {
            state: from.to_string(),
            board: board.name.clone(),
        });
    }
    if !to_valid {
        return Err(StateError::InvalidState {
            state: to.to_string(),
            board: board.name.clone(),
        });
    }

    let from_idx = states
        .iter()
        .position(|s| s == from)
        .expect("already validated");
    let to_idx = states
        .iter()
        .position(|s| s == to)
        .expect("already validated");

    // Forward transitions are always valid.
    if to_idx > from_idx {
        return Ok(());
    }

    // An idempotent re-move is bookkeeping, not an error. The transition MODEL already
    // ruled this (`state_model::is_legal_transition` answers Legal for `from == to`,
    // because a re-move onto the state an item already holds is routine); the positional
    // rule refused it, so the two enforcement paths disagreed about the same move.
    if from_idx == to_idx {
        return Ok(());
    }

    // BACKWARD, and this is the half a forward-only rule cannot express.
    //
    // Per-type lifecycles are ordered lists whose tail is terminal, so under forward-only
    // every legal target from a mid-lifecycle state was a CLOSE. An item claimed out of
    // its queue and then released had nowhere to be released TO: returning it to the
    // re-offerable head of its own lifecycle was not merely refused, it was inexpressible,
    // and the only expressible "release" was closing the record. A queue you may enter and
    // leave only by closing is not a lifecycle.
    //
    // So the rule is TERMINALITY, not direction: a live (non-terminal) state may move
    // anywhere within its own lifecycle — release, re-plan, rework — while a TERMINAL
    // state is ABSORBING. Releasing is not resurrecting: a closed record stays closed
    // unless an operator forces it, which is what keeps `complete -> running` refused.
    if !is_terminal_state(from) {
        return Ok(());
    }

    // Backward FROM a terminal state is a resurrection — refused (--force is the valve).
    Err(StateError::InvalidTransition {
        from: from.to_string(),
        to: to.to_string(),
        board: board.name.clone(),
    })
}

/// The verdict of a PER-TYPE transition query, three-valued for the same reason
/// [`crate::state_model::TransitionVerdict`] is: a state that is not part of the type's
/// lifecycle at all is not the same answer as an illegal move between two states that are.
///
/// The distinction is what makes this safe to enforce on a live store. Items are created
/// at the board-level intake state and a deployment's own tooling parks them at states no
/// per-type list contains, so treating "outside this lifecycle" as ILLEGAL would refuse
/// moves that every existing caller makes today. It is reported as its own verdict and the
/// caller owns the migration posture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeTransitionVerdict {
    /// Both states are in this type's lifecycle and the move is allowed.
    Legal,
    /// Both states are in this type's lifecycle and the move is not; carries the legal
    /// targets so a refusal can NAME them.
    Illegal { legal_targets: Vec<String> },
    /// The type has no lifecycle of its own, or one of the two states is not in it.
    /// Never silently legal, never silently illegal.
    OutsideLifecycle,
}

/// Answer `from -> to` against ONE item type's own lifecycle, three-valued.
///
/// This is the query surface a chokepoint needs: [`validate_transition_for_type`] collapses
/// "not in this lifecycle" into the same `Err` as "illegal move", which is right for a
/// caller that owns its own data and wrong for one enforcing over a store it inherited.
pub fn type_transition_verdict(
    board: &BoardConfig,
    from: &str,
    to: &str,
    item_type: &str,
) -> TypeTransitionVerdict {
    let Some(states) = board.type_states.get(item_type) else {
        return TypeTransitionVerdict::OutsideLifecycle;
    };
    if !states.iter().any(|s| s == from) || !states.iter().any(|s| s == to) {
        return TypeTransitionVerdict::OutsideLifecycle;
    }
    if validate_transition_for_type(board, from, to, Some(item_type)).is_ok() {
        return TypeTransitionVerdict::Legal;
    }
    let legal_targets = states
        .iter()
        .filter(|t| {
            t.as_str() != from
                && validate_transition_for_type(board, from, t, Some(item_type)).is_ok()
        })
        .cloned()
        .collect();
    TypeTransitionVerdict::Illegal { legal_targets }
}

/// Check WIP limits for a target status.
///
/// Returns Ok if under limit, Err if at or over limit.
/// `item_type` is checked against `wip_exempt_types` (e.g., voyages).
pub fn check_wip_limit(
    board: &BoardConfig,
    target_status: &str,
    current_count: u32,
    item_type: &str,
) -> Result<()> {
    // WIP-exempt types (e.g., voyages) bypass limits
    if board.is_wip_exempt(item_type) {
        return Ok(());
    }

    // Map status to WIP category
    let category = status_to_wip_category(target_status, &board.name);

    if let Some(limit) = board.wip_limit(category)
        && current_count >= limit
    {
        return Err(StateError::WipLimitReached {
            status: target_status.to_string(),
            current: current_count,
            limit,
        });
    }

    Ok(())
}

/// Map a status to its WIP limit category.
///
/// Development board:
///   - backlog, planning, ready → "provisioning"
///   - in_progress → "underway"
///   - review → "approaching"
///   - done → no limit
///
/// Research board:
///   - active → "active"
///   - others → no limit
fn status_to_wip_category<'a>(status: &str, board_name: &str) -> &'a str {
    match board_name {
        "development" => match status {
            "backlog" | "planning" | "ready" => "provisioning",
            "in_progress" => "underway",
            "review" => "approaching",
            _ => "",
        },
        "research" => match status {
            "active" => "active",
            _ => "",
        },
        _ => "",
    }
}

/// Valid resolution values.
///
/// `refuted` supports hypothesis closure on negative evidence
/// (`move H-1234 retired --resolution refuted`) — it was missing here, so the CLI
/// rejected a resolution the research lifecycle legitimately needs.
const VALID_RESOLUTIONS: &[&str] = &[
    "completed",
    "superseded",
    "wont_do",
    "duplicate",
    "obsolete",
    "merged",
    "refuted",
];

/// Terminal states where resolution can be set.
const TERMINAL_STATES: &[&str] = &["done", "complete", "abandoned", "retired"];

/// Validate a resolution value. Returns Ok if valid or None.
pub fn validate_resolution(resolution: Option<&str>, target_status: &str) -> Result<()> {
    let Some(res) = resolution else {
        return Ok(());
    };

    if !VALID_RESOLUTIONS.contains(&res) {
        return Err(StateError::InvalidResolution {
            resolution: res.to_string(),
        });
    }

    if !TERMINAL_STATES.contains(&target_status) {
        return Err(StateError::ResolutionOnNonTerminal {
            status: target_status.to_string(),
        });
    }

    Ok(())
}

/// Check if a status is terminal (resolution-eligible).
pub fn is_terminal_state(status: &str) -> bool {
    TERMINAL_STATES.contains(&status)
}

/// Every status any configured lifecycle can produce, across both boards and every
/// per-type override.
///
/// This is the UNION rather than a per-board set, and that is deliberate: a `--status`
/// filter is refused only when the value can match *nothing anywhere*. Narrowing the
/// refusal per board would reject values that are legitimate on the other one, and a
/// refusal that rejects a legitimate query is a worse bug than the one being fixed.
pub const VALID_STATUSES: &[&str] = &[
    "backlog",
    "in_progress",
    "review",
    "done",
    "blocked",
    "draft",
    "active",
    "retired",
    "complete",
    "abandoned",
    "planned",
    "running",
    "outline",
    "writing",
    "captured",
    "formalized",
    // From the lifecycle model (PR #87): the planning-funnel states. Without these a
    // `--status planning` filter is refused while `move X planning` is accepted — the
    // filter and the state machine disagreeing about what a state is. The coherence
    // test below pins the whole set against the LOADED model so this cannot drift again.
    "planning",
    "ready",
    // The deliberate operator park (pre-backlog holding for superseded-era work);
    // 132 live items rode in on it while status was unvalidated. In the model.
    "stale",
    // The idea's review-and-refine stage, between capture and filing. NOT in the
    // lifecycle model above: that model is the DEVELOPMENT board's default lifecycle
    // (its states carry transitions), and an idea state has no business as a
    // transition target there. Research statuses have always lived here and only
    // here — `captured`, `formalized`, `draft`, `active` are all in this const and
    // none is a kb:LifecycleState — so this row follows the established shape rather
    // than inventing one.
    //
    // ⚠ The coherence test below runs model -> const and CANNOT cover this direction:
    // it asserts every modelled state is a valid filter, which says nothing about a
    // status that is deliberately const-only. That is why this entry carries its own
    // pin (`refining_is_a_valid_status`); without it, removing this line is a silent
    // regression the whole suite stays green through.
    "refining",
];

/// Refuse a `--status` filter value that no lifecycle can produce, naming the valid set.
///
/// Without this, a filter that CANNOT match and a filter that legitimately matched
/// nothing are byte-identical: both print `No items found.` and exit 0. That silence is
/// what let a one-character typo (`in-progress` for `in_progress`) sit undetected in a
/// caller whose empty result was its only safety input — every record read as unclaimed.
/// A refusal would have failed that script loudly the first time it ran.
///
/// Mirrors the unknown-`--board` refusal: name the offending value, then the valid set.
///
/// A VALID status that simply has no matches is NOT an error — it still returns its empty
/// result normally. Both directions matter; a refusal that also rejects legitimate empties
/// would be a regression, not a fix.
pub fn validate_status_filter(status: &str) -> std::result::Result<(), String> {
    if VALID_STATUSES.contains(&status) {
        return Ok(());
    }
    Err(format!(
        "unknown --status '{status}' — valid statuses: {}",
        VALID_STATUSES.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// PR #88: the filter const and the LOADED lifecycle model must agree. PR #87 added
    /// planning/ready to the model while this const predated them, so `--status planning`
    /// was refused while `move X planning` was accepted. A const cannot read runtime data,
    /// so the coherence is pinned here instead: every state the model loads (data or
    /// fallback) is a valid filter value. Goes RED the next time a state is added to the
    /// ontology without joining VALID_STATUSES.
    #[test]
    fn every_model_state_is_a_valid_status_filter() {
        for state in crate::state_model::states() {
            assert!(
                VALID_STATUSES.contains(&state.name),
                "state '{}' is in the lifecycle model but missing from VALID_STATUSES — \
                 the --status filter would refuse a state the state machine accepts",
                state.name
            );
        }
    }

    /// The idea pipeline's refine stage must be MOVABLE-TO, and nothing else pins it.
    ///
    /// The coherence test above runs model -> const, so a const-only research status is
    /// outside its reach by construction; `refining` is const-only on purpose (it is not
    /// a development-board transition target). Deleting the const entry would make
    /// `move IDEA-X refining` refuse with UNKNOWN_STATUS while every other test stayed
    /// green — the pipeline's claim step, dead, silently.
    ///
    /// The negative arm is the half that matters: it fails if someone "fixes" the
    /// existence check by accepting anything, which would restore the typo hazard the
    /// check was added for.
    #[test]
    fn refining_is_a_valid_status() {
        assert!(
            validate_status_filter("refining").is_ok(),
            "the idea pipeline's refine stage must be a legal status and filter value"
        );
        assert!(
            validate_status_filter("refinning").is_err(),
            "a near-miss must still refuse — this pin must not be satisfiable by opening \
             the vocabulary"
        );
    }

    fn dev_board() -> BoardConfig {
        BoardConfig {
            name: "development".to_string(),
            preset: "nautical".to_string(),
            path: "kanban-work/".to_string(),
            scan_paths: vec!["kanban-work/expeditions/".to_string()],
            ignore: vec![],
            wip_exempt_types: vec!["voyage".to_string()],
            wip_limits: HashMap::from([
                ("provisioning".to_string(), 50),
                ("underway".to_string(), 4),
                ("approaching".to_string(), 3),
            ]),
            states: vec![
                "backlog".to_string(),
                "planning".to_string(),
                "ready".to_string(),
                "in_progress".to_string(),
                "review".to_string(),
                "done".to_string(),
            ],
            phases: vec![],
            type_states: HashMap::new(),
        }
    }

    fn research_board() -> BoardConfig {
        BoardConfig {
            name: "research".to_string(),
            preset: "hdd".to_string(),
            path: "research/".to_string(),
            scan_paths: vec!["research/hypotheses/".to_string()],
            ignore: vec![],
            wip_exempt_types: vec![],
            wip_limits: HashMap::from([("active".to_string(), 5)]),
            states: vec![
                "draft".to_string(),
                "active".to_string(),
                "complete".to_string(),
                "abandoned".to_string(),
            ],
            phases: vec![],
            type_states: HashMap::new(),
        }
    }

    #[test]
    fn test_valid_forward_transitions() {
        let board = dev_board();
        assert!(validate_transition(&board, "backlog", "in_progress").is_ok());
        assert!(validate_transition(&board, "in_progress", "review").is_ok());
        assert!(validate_transition(&board, "review", "done").is_ok());
        assert!(validate_transition(&board, "backlog", "done").is_ok()); // skip allowed (forward)
    }

    #[test]
    fn test_invalid_backward_transition() {
        let board = dev_board();
        // DELIBERATE INVARIANT CHANGE, stated: the positional forward-only rule
        // refused three moves that are measured live practice, and the fleet server never
        // enforced the refusal (zero call sites in the composition), so the rule
        // contradicted reality without protecting anything:
        //   done -> backlog        reopen (a done signal reopened during a cord clear)
        //   review -> in_progress  rework after request-changes
        //   in_progress -> backlog the bounce (workit-bounce.sh's revert)
        // The explicit model legalizes those and refuses what nothing legitimate does:
        assert!(validate_transition(&board, "done", "backlog").is_ok());
        assert!(validate_transition(&board, "review", "in_progress").is_ok());
        assert!(validate_transition(&board, "in_progress", "backlog").is_ok());
        // Still illegal per the model, and the refusal NAMES the legal targets:
        match validate_transition(&board, "planning", "in_progress") {
            Err(StateError::IllegalPerModel { targets, .. }) => {
                assert!(
                    targets.contains("ready"),
                    "refusal must name 'ready': {targets}"
                );
            }
            other => panic!("planning->in_progress must be IllegalPerModel, got {other:?}"),
        }
        assert!(validate_transition(&board, "backlog", "review").is_err());
        assert!(validate_transition(&board, "ready", "backlog").is_err());
    }

    #[test]
    fn test_invalid_state() {
        let board = dev_board();
        let err = validate_transition(&board, "nonexistent", "done");
        assert!(err.is_err());
        match err.unwrap_err() {
            StateError::InvalidState { state, .. } => assert_eq!(state, "nonexistent"),
            _ => panic!("Expected InvalidState"),
        }
    }

    #[test]
    fn test_research_transitions() {
        let board = research_board();
        assert!(validate_transition(&board, "draft", "active").is_ok());
        assert!(validate_transition(&board, "active", "complete").is_ok());
        assert!(validate_transition(&board, "draft", "abandoned").is_ok());
        assert!(validate_transition(&board, "complete", "draft").is_err());
    }

    #[test]
    fn test_wip_limit_under() {
        let board = dev_board();
        assert!(check_wip_limit(&board, "in_progress", 3, "expedition").is_ok());
    }

    #[test]
    fn test_wip_limit_at_capacity() {
        let board = dev_board();
        let err = check_wip_limit(&board, "in_progress", 4, "expedition");
        assert!(err.is_err());
        match err.unwrap_err() {
            StateError::WipLimitReached { current, limit, .. } => {
                assert_eq!(current, 4);
                assert_eq!(limit, 4);
            }
            _ => panic!("Expected WipLimitReached"),
        }
    }

    #[test]
    fn test_wip_exempt_voyage() {
        let board = dev_board();
        // Voyages bypass WIP limits even when at capacity
        assert!(check_wip_limit(&board, "in_progress", 4, "voyage").is_ok());
        assert!(check_wip_limit(&board, "in_progress", 100, "voyage").is_ok());
    }

    #[test]
    fn test_wip_no_limit_for_done() {
        let board = dev_board();
        // No WIP limit on "done" status
        assert!(check_wip_limit(&board, "done", 1000, "expedition").is_ok());
    }

    fn research_board_with_type_states() -> BoardConfig {
        BoardConfig {
            name: "research".to_string(),
            preset: "hdd".to_string(),
            path: "research/".to_string(),
            scan_paths: vec!["research/".to_string()],
            ignore: vec![],
            wip_exempt_types: vec![],
            wip_limits: HashMap::from([("active".to_string(), 5)]),
            states: vec![
                "draft".to_string(),
                "active".to_string(),
                "complete".to_string(),
                "abandoned".to_string(),
                "retired".to_string(),
            ],
            phases: vec![],
            type_states: HashMap::from([
                (
                    "hypothesis".to_string(),
                    vec![
                        "draft".to_string(),
                        "active".to_string(),
                        "retired".to_string(),
                    ],
                ),
                (
                    "measure".to_string(),
                    vec![
                        "draft".to_string(),
                        "active".to_string(),
                        "retired".to_string(),
                    ],
                ),
                (
                    "experiment".to_string(),
                    vec![
                        "planned".to_string(),
                        "running".to_string(),
                        "complete".to_string(),
                        "abandoned".to_string(),
                    ],
                ),
                (
                    "paper".to_string(),
                    vec![
                        "draft".to_string(),
                        "outline".to_string(),
                        "writing".to_string(),
                        "review".to_string(),
                        "complete".to_string(),
                        "abandoned".to_string(),
                    ],
                ),
                (
                    "idea".to_string(),
                    vec![
                        "captured".to_string(),
                        "formalized".to_string(),
                        "abandoned".to_string(),
                    ],
                ),
            ]),
        }
    }

    #[test]
    fn test_hypothesis_cannot_complete() {
        let board = research_board_with_type_states();
        // Hypotheses go draft → active → retired, never "complete"
        assert!(
            validate_transition_for_type(&board, "draft", "active", Some("hypothesis")).is_ok()
        );
        assert!(
            validate_transition_for_type(&board, "active", "retired", Some("hypothesis")).is_ok()
        );
        assert!(
            validate_transition_for_type(&board, "active", "complete", Some("hypothesis")).is_err()
        );
    }

    #[test]
    fn test_measure_cannot_complete() {
        let board = research_board_with_type_states();
        // Measures go draft → active → retired, never "complete"
        assert!(validate_transition_for_type(&board, "draft", "active", Some("measure")).is_ok());
        assert!(validate_transition_for_type(&board, "active", "retired", Some("measure")).is_ok());
        assert!(
            validate_transition_for_type(&board, "active", "complete", Some("measure")).is_err()
        );
    }

    #[test]
    fn test_experiment_follows_run_lifecycle() {
        let board = research_board_with_type_states();
        assert!(
            validate_transition_for_type(&board, "planned", "running", Some("experiment")).is_ok()
        );
        assert!(
            validate_transition_for_type(&board, "running", "complete", Some("experiment")).is_ok()
        );
        assert!(
            validate_transition_for_type(&board, "planned", "abandoned", Some("experiment"))
                .is_ok()
        );
        // Can't go backward
        assert!(
            validate_transition_for_type(&board, "complete", "running", Some("experiment"))
                .is_err()
        );
    }

    #[test]
    fn test_paper_follows_work_lifecycle() {
        let board = research_board_with_type_states();
        assert!(validate_transition_for_type(&board, "draft", "outline", Some("paper")).is_ok());
        assert!(validate_transition_for_type(&board, "outline", "writing", Some("paper")).is_ok());
        assert!(validate_transition_for_type(&board, "writing", "review", Some("paper")).is_ok());
        assert!(validate_transition_for_type(&board, "review", "complete", Some("paper")).is_ok());
        assert!(validate_transition_for_type(&board, "draft", "abandoned", Some("paper")).is_ok());
    }

    #[test]
    fn test_idea_captured_to_formalized() {
        let board = research_board_with_type_states();
        assert!(
            validate_transition_for_type(&board, "captured", "formalized", Some("idea")).is_ok()
        );
        assert!(
            validate_transition_for_type(&board, "captured", "abandoned", Some("idea")).is_ok()
        );
        // Backward within a LIVE lifecycle is the release/rework edge and is legal —
        // `formalized -> captured` is structurally the same move as `running -> planned`,
        // so a rule that admits one and refuses the other would be a carve-out, not a
        // rule. What stays refused is backward from a TERMINAL state (`abandoned`), which
        // `a_terminal_state_is_absorbing` pins for every type.
        assert!(
            validate_transition_for_type(&board, "formalized", "captured", Some("idea")).is_ok()
        );
        assert!(
            validate_transition_for_type(&board, "abandoned", "captured", Some("idea")).is_err()
        );
    }

    /// A claimed-then-released item must have somewhere to go BACK to.
    ///
    /// The per-type lifecycles are ordered LISTS and the positional rule used to be
    /// forward-only, so from `running` every legal target was terminal (`complete`,
    /// `abandoned`): returning a released experiment to the re-offerable head of its own
    /// queue was not merely refused, it was inexpressible. A queue you can enter and
    /// never leave except by closing is not a lifecycle.
    #[test]
    fn a_release_returns_a_running_item_to_its_queue() {
        let board = research_board_with_type_states();
        assert!(
            validate_transition_for_type(&board, "running", "planned", Some("experiment")).is_ok(),
            "running -> planned is the un-claim: the ONLY non-terminal target from 'running'"
        );
        // The same shape one lifecycle over — this is a property of live lifecycles, not
        // a special case carved for one type.
        assert!(
            validate_transition_for_type(&board, "review", "writing", Some("paper")).is_ok(),
            "review -> writing is the same edge wearing a different name"
        );
        assert!(
            validate_transition_for_type(&board, "active", "draft", Some("hypothesis")).is_ok(),
            "active -> draft likewise"
        );
    }

    /// The other half of the same rule, and the half that must NOT widen: a terminal
    /// state is ABSORBING. Releasing is not resurrecting — a closed record stays closed
    /// unless an operator forces it.
    #[test]
    fn a_terminal_state_is_absorbing() {
        let board = research_board_with_type_states();
        for (from, to, ty) in [
            ("complete", "running", "experiment"),
            ("complete", "planned", "experiment"),
            ("abandoned", "planned", "experiment"),
            ("retired", "active", "hypothesis"),
            ("retired", "draft", "measure"),
            ("complete", "writing", "paper"),
        ] {
            assert!(
                validate_transition_for_type(&board, from, to, Some(ty)).is_err(),
                "{from} -> {to} ({ty}) is a resurrection and must stay refused"
            );
        }
    }

    /// An idempotent re-move is bookkeeping, not an error — the transition MODEL already
    /// ruled this (`is_legal_transition` returns Legal for `from == to`) while the
    /// positional rule refused it, so the two enforcement paths disagreed about the same
    /// move. They agree now, and the direction is the permissive one: turning a no-op
    /// into an error is the change no caller expects.
    #[test]
    fn an_idempotent_re_move_is_legal_at_every_state_including_terminal() {
        let board = research_board_with_type_states();
        for (state, ty) in [
            ("planned", "experiment"),
            ("running", "experiment"),
            ("complete", "experiment"),
            ("abandoned", "experiment"),
            ("retired", "hypothesis"),
        ] {
            assert!(
                validate_transition_for_type(&board, state, state, Some(ty)).is_ok(),
                "{state} -> {state} ({ty}) is a no-op re-move, not a refusal"
            );
        }
    }

    /// The three-valued surface, arm by arm. The middle value is the one that carries
    /// the design: "not part of this type's lifecycle" is a DIFFERENT answer from
    /// "illegal move", and collapsing the two is what would make enforcement over a live
    /// store an outage rather than a gate.
    #[test]
    fn type_transition_verdict_separates_outside_from_illegal() {
        let board = research_board_with_type_states();

        assert_eq!(
            type_transition_verdict(&board, "running", "planned", "experiment"),
            TypeTransitionVerdict::Legal,
            "the un-claim"
        );

        // Illegal, and it NAMES what is legal so a refusal can quote them.
        match type_transition_verdict(&board, "complete", "running", "experiment") {
            TypeTransitionVerdict::Illegal { legal_targets } => assert_eq!(
                legal_targets,
                vec!["abandoned".to_string()],
                "from a terminal state only the forward tail remains"
            ),
            other => panic!("complete -> running must be Illegal, got {other:?}"),
        }

        // OUTSIDE, three ways it arises on a real store:
        for (from, to, ty, why) in [
            (
                "backlog",
                "planned",
                "experiment",
                "the intake state every item is created at",
            ),
            (
                "running",
                "backlog",
                "experiment",
                "the release target existing tooling uses",
            ),
            (
                "complete",
                "done",
                "experiment",
                "a board-level terminal an item was parked at",
            ),
            (
                "captured",
                "refining",
                "idea",
                "a deliberate const-only status with no lifecycle place",
            ),
        ] {
            assert_eq!(
                type_transition_verdict(&board, from, to, ty),
                TypeTransitionVerdict::OutsideLifecycle,
                "{from} -> {to} ({ty}): {why}"
            );
        }

        // A type with no lifecycle of its own has nothing to enforce.
        assert_eq!(
            type_transition_verdict(&board, "draft", "active", "chore"),
            TypeTransitionVerdict::OutsideLifecycle,
            "a type the board declares no states for"
        );
    }

    /// THE OTHER WRITERS. The move chokepoint is not the only code that writes the status
    /// column: the requeue verb writes it directly (`backlog`, or `blocked` once the
    /// attempt cap is hit) and so does the ratify verb (`in_progress`, voyages only).
    /// Neither routes through the move handler, so neither consults the per-type gate.
    ///
    /// That is SOUND, but only because of a property of the shipped lifecycles rather
    /// than of those verbs: every state they target sits outside every per-type
    /// lifecycle, so the gate would have fallen through for them anyway. Add one of
    /// those states to a per-type lifecycle and the bypass opens silently — a gate whose
    /// coverage argument lives in a reviewer's head and nowhere in the suite. It lives
    /// here instead, over the SHIPPED config, and it goes red on the change that would
    /// open it.
    #[test]
    fn the_states_other_status_writers_target_are_outside_every_per_type_lifecycle() {
        let config = crate::config::ConfigFile::from_yaml(crate::config::default_config_yaml())
            .expect("the shipped default config parses");
        let board = config.board("research").expect("research board");

        // Counted INSIDE the loops, so the floor below is what actually ran rather than a
        // constant asserted against itself.
        let mut checked = 0usize;
        let mut types_seen = 0usize;
        for (item_type, states) in &board.type_states {
            types_seen += 1;
            for target in ["backlog", "blocked", "in_progress"] {
                for own in states {
                    for (from, to) in [(own.as_str(), target), (target, own.as_str())] {
                        assert_eq!(
                            type_transition_verdict(board, from, to, item_type),
                            TypeTransitionVerdict::OutsideLifecycle,
                            "{from} -> {to} ({item_type}): '{target}' is written directly by \
                             the requeue/ratify verbs, which never consult the per-type gate — \
                             it must not become part of a per-type lifecycle without that \
                             bypass being closed first"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert!(
            types_seen >= 6 && checked >= 120,
            "the loops must have run over the real shipped lifecycles: \
             types_seen={types_seen} checked={checked}"
        );
    }

    #[test]
    fn test_unknown_type_uses_board_states() {
        let board = research_board_with_type_states();
        // Unknown type falls back to board-level states
        assert!(validate_transition_for_type(&board, "draft", "active", Some("unknown")).is_ok());
        assert!(
            validate_transition_for_type(&board, "active", "complete", Some("unknown")).is_ok()
        );
    }

    #[test]
    fn test_research_wip_limit() {
        let board = research_board();
        assert!(check_wip_limit(&board, "active", 4, "hypothesis").is_ok());
        assert!(check_wip_limit(&board, "active", 5, "hypothesis").is_err());
    }

    // ── Resolution validation tests ──

    #[test]
    fn test_valid_resolutions_on_terminal_states() {
        assert!(validate_resolution(Some("completed"), "done").is_ok());
        assert!(validate_resolution(Some("superseded"), "done").is_ok());
        assert!(validate_resolution(Some("wont_do"), "done").is_ok());
        assert!(validate_resolution(Some("duplicate"), "done").is_ok());
        assert!(validate_resolution(Some("obsolete"), "done").is_ok());
        assert!(validate_resolution(Some("merged"), "done").is_ok());
        assert!(validate_resolution(Some("completed"), "complete").is_ok());
        assert!(validate_resolution(Some("wont_do"), "abandoned").is_ok());
        assert!(validate_resolution(Some("completed"), "retired").is_ok());
        // guardrail #6 — `arrow-kanban move H-XXX retired --resolution refuted` must be accepted.
        assert!(validate_resolution(Some("refuted"), "retired").is_ok());
        assert!(validate_resolution(Some("refuted"), "done").is_ok());
    }

    #[test]
    fn test_none_resolution_always_ok() {
        assert!(validate_resolution(None, "done").is_ok());
        assert!(validate_resolution(None, "in_progress").is_ok());
        assert!(validate_resolution(None, "backlog").is_ok());
    }

    #[test]
    fn test_invalid_resolution_value() {
        let err = validate_resolution(Some("cancelled"), "done");
        assert!(err.is_err());
        match err.unwrap_err() {
            StateError::InvalidResolution { resolution } => {
                assert_eq!(resolution, "cancelled");
            }
            _ => panic!("Expected InvalidResolution"),
        }
    }

    #[test]
    fn test_resolution_on_non_terminal_state() {
        let err = validate_resolution(Some("completed"), "in_progress");
        assert!(err.is_err());
        match err.unwrap_err() {
            StateError::ResolutionOnNonTerminal { status } => {
                assert_eq!(status, "in_progress");
            }
            _ => panic!("Expected ResolutionOnNonTerminal"),
        }
    }

    #[test]
    fn test_is_terminal_state() {
        assert!(is_terminal_state("done"));
        assert!(is_terminal_state("complete"));
        assert!(is_terminal_state("abandoned"));
        assert!(is_terminal_state("retired"));
        assert!(!is_terminal_state("in_progress"));
        assert!(!is_terminal_state("backlog"));
        assert!(!is_terminal_state("review"));
    }
}
