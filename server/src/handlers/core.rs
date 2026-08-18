// SPDX-License-Identifier: MIT
//! Core CRUD and analytics handlers.

use super::{error_response, states_as_strings};
use crate::engine::KanbanReply;
use arrow_kanban::crud::{CreateItemInput, KanbanStore};
use arrow_kanban::item_type::ItemType;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban::{critical_path, display, export};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct CreateRequest {
    title: String,
    item_type: String,
    priority: Option<String>,
    assignee: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    related: Vec<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    body: Option<String>,
    /// Typed relationships as `predicate:TARGET-ID`, applied at create time.
    #[serde(default)]
    relate: Vec<String>,
    /// Who issued this command. Stamped into every request envelope by the
    /// client, which is the only party that knows the caller's identity; the
    /// server never resolves it locally (one process would stamp its own host
    /// on every fleet mutation). `None` is honest — never fabricate an actor.
    #[serde(default)]
    actor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateResponse {
    id: String,
    title: String,
    item_type: String,
    status: String,
    /// The typed edges actually recorded — so the caller sees what landed rather than
    /// assuming their --relate flags were honoured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    relationships: Vec<String>,
}

/// Parse `predicate:TARGET` specs and validate each against the vocabulary.
///
/// Validation happens BEFORE any mutation, and the first bad edge rejects the whole
/// request — a partially-related item is worse than a refused one, because the caller
/// cannot tell which half landed.
///
/// The target's type is taken from the store when it resolves, else inferred from the ID
/// prefix. An unresolvable target does NOT fail: the research trio (H/M/EXPR) is
/// cross-board, and refusing edges to items this store cannot see would make the whole
/// feature unusable for its main use case.
fn parse_and_validate(
    specs: &[String],
    source_type: &str,
    source_id: Option<&str>,
) -> Result<Vec<(String, String)>, Vec<u8>> {
    use arrow_kanban::relation_vocab as vocab;
    let mut out = Vec::new();
    for spec in specs {
        let (pred, target) = vocab::parse_spec(spec).ok_or_else(|| {
            error_response(
                &format!(
                    "malformed --relate '{spec}' — expected <predicate>:<TARGET-ID>, e.g. implements:VY-1234"
                ),
                "INVALID_RELATION",
            )
        })?;

        // The ID prefix IS the type in this scheme (EX-/VY-/H-/EXPR-/M-/PAPER-), so no
        // store lookup is needed — and an unknown prefix yields None, which skips RANGE
        // validation rather than rejecting a cross-board target we cannot classify.
        let target_type = vocab::type_from_id(target);

        let p = vocab::validate_edge(
            pred,
            source_id.unwrap_or("<new>"),
            source_type,
            target,
            target_type,
        )
        .map_err(|e| error_response(&format!("{e}"), "INVALID_RELATION"))?;

        out.push((p.name.to_string(), target.to_string()));
    }
    Ok(out)
}

pub(crate) fn handle_create_typed(
    req: CreateRequest,
    store: &mut KanbanStore,
    relations: &mut arrow_kanban::relations::RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    let item_type = ItemType::from_str_loose(&req.item_type).ok_or_else(|| {
        error_response(
            &format!("Unknown item type: {}", req.item_type),
            "INVALID_TYPE",
        )
    })?;

    // Parse and VALIDATE every typed edge BEFORE creating the item, so a bad
    // relationship never leaves a half-related item behind. All-or-nothing.
    let edges = parse_and_validate(&req.relate, item_type.as_str(), None)?;

    // `related`/`dependsOn` edges ALSO project into the flat columns, so roadmap /
    // critical-path / worklist / arrow-kanban show keep reading exactly what they read today. The
    // flat lists are a projection of the typed edges, written in the same operation —
    // never a second independently-authored source of truth.
    let mut related = req.related;
    let mut depends_on = req.depends_on;
    for (pred, target) in &edges {
        match arrow_kanban::relation_vocab::flat_column_for(pred) {
            Some("related") if !related.contains(target) => related.push(target.clone()),
            Some("depends_on") if !depends_on.contains(target) => depends_on.push(target.clone()),
            _ => {}
        }
    }

    let input = CreateItemInput {
        title: req.title.clone(),
        item_type,
        priority: req.priority,
        assignee: req.assignee,
        tags: req.tags,
        related,
        depends_on,
        body: req.body,
    };

    let id = store
        .create_item(&input)
        .map_err(|e| error_response(&format!("{e}"), "CREATE_FAILED"))?;

    for (pred, target) in &edges {
        if let Err(e) = relations.add_relation(&id, target, pred) {
            eprintln!("kanban: warning — failed to record {id} -{pred}-> {target}: {e}");
        }
    }

    Ok(KanbanReply::Create(CreateResponse {
        id,
        title: req.title,
        item_type: req.item_type,
        status: "backlog".to_string(),
        relationships: edges.iter().map(|(p, t)| format!("{p}:{t}")).collect(),
    }))
}

// ── Move ────────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct MoveRequest {
    id: String,
    status: String,
    assignee: Option<String>,
    #[serde(default)]
    force: bool,
    reason: Option<String>,
    resolution: Option<String>,
    closed_by: Option<String>,
    /// Who issued this command. Stamped into every request envelope by the
    /// client, which is the only party that knows the caller's identity; the
    /// server never resolves it locally (one process would stamp its own host
    /// on every fleet mutation). `None` is honest — never fabricate an actor.
    #[serde(default)]
    actor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MoveResponse {
    id: String,
    from: String,
    to: String,
    resolution: Option<String>,
}

impl MoveResponse {
    /// The pre-move status — read by the bounce verb to shape its own reply.
    pub(crate) fn pre_move_status(&self) -> &str {
        &self.from
    }
}

pub(crate) fn handle_move_typed(
    req: MoveRequest,
    store: &mut KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    // Validate resolution before the move
    arrow_kanban::state_machine::validate_resolution(req.resolution.as_deref(), &req.status)
        .map_err(|e| error_response(&format!("{e}"), "INVALID_RESOLUTION"))?;

    // Get current status before the move
    let item = store
        .get_item(&req.id)
        .map_err(|e| error_response(&format!("{e}"), "NOT_FOUND"))?;

    let from_status = {
        use arrow::array::Array;
        let col = item
            .column(arrow_kanban::schema::items_col::STATUS)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .expect("status column");
        col.value(0).to_string()
    };

    // The lifecycle MODEL enforces the transition (states + transitions live in
    // ontology/kanban.ttl; the loader is fail-closed). This is the served chokepoint —
    // the model consult in `validate_transition_for_type` has no caller on this path,
    // so without this block the model described transitions while the server accepted
    // anything (definition and behaviour as unrelated things that happen to agree).
    // Scope mirrors the consult's own guard: the development board's DEFAULT lifecycle.
    // UnknownState falls through (a custom config state keeps its semantics), and
    // `--force` bypasses with the same audited semantics as the WIP-limit override —
    // repair paths (store recovery) need an escape valve, and forced moves are already
    // recorded in the status history.
    // STATUS EXISTENCE: any string used to be accepted, so a one-character typo
    // silently removed an item from every status-filtered view — no error, no
    // column, no frontier, while the store reported it healthy. The vocabulary is
    // the same cross-board union the --status filter validates against (which the
    // model-coherence test keeps in step with the lifecycle data), so a state added
    // to the ontology becomes movable-to with no second list to update. --force
    // bypasses as the audited repair valve, same as the transition check below.
    if !req.force && arrow_kanban::state_machine::validate_status_filter(&req.status).is_err() {
        return Err(error_response(
            &format!(
                "unknown status '{}' — a typo here would silently hide the item from every view. Valid: {} (--force bypasses, audited)",
                req.status,
                arrow_kanban::state_machine::VALID_STATUSES.join(", ")
            ),
            "UNKNOWN_STATUS",
        ));
    }

    {
        let board = {
            use arrow::array::Array;
            let col = item
                .column(arrow_kanban::schema::items_col::BOARD)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("board column");
            col.value(0).to_string()
        };
        if board == "development" && !req.force {
            use arrow_kanban::state_model::{TransitionVerdict, is_legal_transition};
            if let TransitionVerdict::Illegal { legal_targets } =
                is_legal_transition(&from_status, &req.status)
            {
                return Err(error_response(
                    &format!(
                        "Invalid transition: '{}' → '{}' is not in the lifecycle model. \
                         Legal from '{}': {} (--force bypasses, audited)",
                        from_status,
                        req.status,
                        from_status,
                        legal_targets.join(", ")
                    ),
                    "ILLEGAL_TRANSITION",
                ));
            }
        }
    }

    // Atomic exclusive claim: a `move … in_progress --assign X` is a claim. Reject it
    // if the item is already in_progress under a different agent (without --force) — turns
    // assignment into a true mutex instead of last-writer-wins.
    if req.status == "in_progress"
        && let Some(assignee) = req.assignee.as_deref()
    {
        store
            .check_exclusive_claim(&req.id, assignee, req.force)
            .map_err(|e| error_response(&format!("{e}"), "CLAIM_CONFLICT"))?;
    }

    store
        .update_status(
            &req.id,
            &req.status,
            req.assignee.as_deref(),
            req.force,
            req.reason.as_deref(),
        )
        .map_err(|e| error_response(&format!("{e}"), "MOVE_FAILED"))?;

    // Make the claim STICK — `move --assign` must set the assignee column, not just the
    // run-provenance agent (previously it didn't, hence the `arrow-kanban update --assign` follow-up).
    if let Some(assignee) = req.assignee.as_deref() {
        store
            .update_assignee(&req.id, Some(assignee))
            .map_err(|e| error_response(&format!("{e}"), "ASSIGN_FAILED"))?;
    }

    // Apply resolution if provided
    if let Some(ref res) = req.resolution {
        store
            .update_resolution(&req.id, Some(res))
            .map_err(|e| error_response(&format!("{e}"), "RESOLUTION_FAILED"))?;
    }

    // Apply closed_by if provided
    if let Some(ref cb) = req.closed_by {
        store
            .update_closed_by(&req.id, Some(cb))
            .map_err(|e| error_response(&format!("{e}"), "CLOSED_BY_FAILED"))?;
    }

    Ok(KanbanReply::Move(MoveResponse {
        id: req.id,
        from: from_status,
        to: req.status,
        resolution: req.resolution,
    }))
}

// ── Atomic claim / bounce (issue #76) ───────────────────────────────────────
//
// Claims used to be COMMENT CONVENTIONS raced on comment order, and bounce
// state lived in external scripts. These verbs promote both to the single
// writer — the natural compare-and-set point — with typed refusals that name
// the holder instead of asking agents to adjudicate comment timestamps.
//
// Both delegate their state change to `handle_move_typed`, so the committed
// event stream sees an ordinary `Moved` and every replay/snapshot/outbox
// consumer works unchanged.

#[derive(Clone, Serialize, Deserialize)]
pub struct ClaimRequest {
    id: String,
    /// The claiming agent — factual identity, the CAS comparand.
    agent: String,
}

pub(crate) fn handle_claim_typed(
    req: ClaimRequest,
    store: &mut KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    let (_status, holder) = store
        .status_and_assignee(&req.id)
        .map_err(|e| error_response(&format!("{e}"), "NOT_FOUND"))?;

    // CAS: an item held by ANOTHER agent refuses, naming the holder — the
    // loser needs no comment adjudication, the refusal IS the adjudication.
    // (Stricter than the in_progress-only move guard: a claim takes UNCLAIMED
    // work, so any other holder refuses regardless of status. Re-claiming your
    // own item is idempotent and allowed.)
    if let Some(holder) = holder.as_deref()
        && !holder.is_empty()
        && holder != req.agent
    {
        return Err(error_response(
            &format!(
                "'{}' is already claimed by {holder} — first claim wins; pick another item or                  coordinate a handover with the holder",
                req.id
            ),
            "CLAIM_HELD",
        ));
    }

    // A bounce is a refused CONTRACT: the item is only re-claimable once its
    // body has been edited. The bounce verb stamped the body hash it refused
    // at; an unchanged hash means unrepaired, and the claim refuses natively —
    // no external check script required.
    //
    // The stamp is the NEWEST landing that PARSES AS A BOUNCE — filter first,
    // then take newest. Taking the newest landing overall would let any later
    // ordinary move to backlog (a board-hygiene sweep's normal job) displace
    // the stamp and silently disarm the gate; the un-gate must stay "the body
    // edit", never "any subsequent backlog move".
    if let Some(v) = store
        .run_reasons_landing(&req.id, "backlog")
        .iter()
        .rev()
        .filter_map(|r| r.as_deref())
        .filter_map(|r| serde_json::from_str::<serde_json::Value>(r).ok())
        .find(|v| v.get("bounce").and_then(|b| b.as_bool()) == Some(true))
    {
        let stamped = v
            .get("body_sha")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        let current = store
            .body_hash_of(&req.id)
            .map_err(|e| error_response(&format!("{e}"), "NOT_FOUND"))?;
        if stamped == current {
            return Err(error_response(
                &format!(
                    "'{}' carries an UNREPAIRED bounce (the body is unchanged since the bounce                      stamped it) — the contract was refused, and the filer/planner must edit the                      body before it is claimable again",
                    req.id
                ),
                "CLAIM_BOUNCED",
            ));
        }
    }

    handle_move_typed(
        MoveRequest {
            id: req.id,
            // The claimant is the actor. Cloned because `assignee` below moves it.
            actor: Some(req.agent.clone()),
            status: "in_progress".to_string(),
            assignee: Some(req.agent),
            force: false,
            reason: Some("claimed (atomic claim verb)".to_string()),
            resolution: None,
            closed_by: None,
        },
        store,
    )
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BounceRequest {
    id: String,
    agent: String,
    reason: String,
}

pub(crate) fn handle_bounce_typed(
    req: BounceRequest,
    store: &mut KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    let body_sha = store
        .body_hash_of(&req.id)
        .map_err(|e| error_response(&format!("{e}"), "NOT_FOUND"))?;

    // The structured stamp the claim verb reads back — the server parses its
    // OWN format here, never foreign prose.
    let stamp = serde_json::json!({
        "bounce": true,
        "body_sha": body_sha,
        "agent": req.agent,
        "reason": req.reason,
    })
    .to_string();

    let moved = handle_move_typed(
        MoveRequest {
            id: req.id.clone(),
            // The bouncer is the actor, even though the bounce CLEARS the assignee —
            // exactly the case where "who performed it" and "who it is for" diverge.
            actor: Some(req.agent.clone()),
            status: "backlog".to_string(),
            assignee: None,
            force: false,
            reason: Some(stamp.clone()),
            resolution: None,
            closed_by: None,
        },
        store,
    )?;
    let from = match &moved {
        KanbanReply::Move(m) => m.pre_move_status().to_string(),
        _ => String::new(),
    };

    // Unassign — the bounce returns the item to the pool. (A move with no
    // assignee leaves the column untouched, so this is explicit.)
    store
        .update_assignee(&req.id, None)
        .map_err(|e| error_response(&format!("{e}"), "ASSIGN_FAILED"))?;

    // The human notification, in the exact line-start marker format the
    // fleet's bounce counters already match on.
    let sha_display = body_sha.as_deref().unwrap_or("none");
    let comment = format!(
        "[workit bounce — {}, server-verb, body-sha:{}] {}",
        req.agent, sha_display, req.reason
    );
    store
        .add_comment(&req.id, &comment, Some(&req.agent))
        .map_err(|e| error_response(&format!("{e}"), "COMMENT_FAILED"))?;

    Ok(KanbanReply::Value(serde_json::json!({
        "id": req.id,
        "from": from,
        "to": "backlog",
        "body_sha": body_sha,
        "stamp": stamp,
    })))
}

// ── Requeue (issue #40) ──────────────────────────────────────────────────────

fn default_requeue_cap() -> i32 {
    3
}

fn default_dead_letter_tag() -> String {
    "dead-letter".to_string()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RequeueRequest {
    pub id: String,
    /// The agent reporting the failure (run provenance).
    pub agent: Option<String>,
    /// The executor session that failed (folded into the run reason).
    pub session: Option<String>,
    /// Why the attempt failed. Required — a requeue with no reason is
    /// exactly the unexplained-dangler the dead-letter path exists to stop.
    pub reason: String,
    /// Attempts at/above which the item dead-letters instead of releasing.
    #[serde(default = "default_requeue_cap")]
    pub cap: i32,
    #[serde(default = "default_dead_letter_tag")]
    pub dead_letter_tag: String,
}

#[derive(Debug, Serialize)]
pub struct RequeueResponse {
    pub id: String,
    /// The attempt count AFTER this requeue (absolute — replay sets this).
    pub attempts: i32,
    /// `backlog` (released) or `blocked` (dead-lettered).
    pub status: String,
    pub dead_lettered: bool,
    pub dead_letter_tag: Option<String>,
}

pub(crate) fn handle_requeue_typed(
    req: RequeueRequest,
    store: &mut KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    if req.reason.trim().is_empty() {
        return Err(error_response(
            "requeue requires a non-empty reason (failure provenance)",
            "REQUEUE_INVALID",
        ));
    }
    let outcome = store
        .requeue_item(
            &req.id,
            req.agent.as_deref(),
            req.session.as_deref(),
            &req.reason,
            req.cap,
            &req.dead_letter_tag,
        )
        .map_err(|e| error_response(&format!("{e}"), "REQUEUE_FAILED"))?;

    let resp = RequeueResponse {
        id: outcome.id,
        attempts: outcome.attempts,
        status: outcome.status,
        dead_lettered: outcome.dead_lettered,
        dead_letter_tag: outcome.dead_lettered.then(|| req.dead_letter_tag.clone()),
    };
    Ok(KanbanReply::Value(serde_json::to_value(&resp).map_err(
        |e| error_response(&format!("Serialization error: {e}"), "INTERNAL"),
    )?))
}

// ── Ratify ───────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct RatifyRequest {
    phase_tag: String,
    /// Who issued this command. Stamped into every request envelope by the
    /// client, which is the only party that knows the caller's identity; the
    /// server never resolves it locally (one process would stamp its own host
    /// on every fleet mutation). `None` is honest — never fabricate an actor.
    #[serde(default)]
    actor: Option<String>,
}

/// First-class phase ratification: strip `pending-ratification` from every item carrying the phase
/// tag, sprint-start its voyages, and verify none remain — atomically, server-side (no shell loop).
pub(crate) fn handle_ratify_typed(
    req: RatifyRequest,
    store: &mut KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    let report = store
        .ratify_phase(&req.phase_tag)
        .map_err(|e| error_response(&format!("{e}"), "RATIFY_FAILED"))?;
    // The built-in verify: a partial strip is a hard error, never a silent success.
    if report.remaining_pending != 0 {
        return Err(error_response(
            &format!(
                "ratify {}: {} item(s) still pending-ratification after strip (partial ratification)",
                req.phase_tag, report.remaining_pending
            ),
            "RATIFY_INCOMPLETE",
        ));
    }
    Ok(KanbanReply::Value(serde_json::json!({
        "phase_tag": req.phase_tag,
        "stripped": report.stripped,
        "voyages_started": report.voyages_started,
        "remaining_pending": report.remaining_pending,
    })))
}

// ── Update ──────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct UpdateRequest {
    id: String,
    title: Option<String>,
    priority: Option<String>,
    assignee: Option<String>,
    tags: Option<Vec<String>>,
    body: Option<String>,
    related: Option<Vec<String>>,
    depends_on: Option<Vec<String>>,
    /// ADD typed edges (`predicate:TARGET`). Additive — unlike `related` /
    /// `depends_on`, which replace.
    #[serde(default)]
    relate: Vec<String>,
    /// REMOVE typed edges (`predicate:TARGET`).
    #[serde(default)]
    unrelate: Vec<String>,
    /// Who issued this command. Stamped into every request envelope by the
    /// client, which is the only party that knows the caller's identity; the
    /// server never resolves it locally (one process would stamp its own host
    /// on every fleet mutation). `None` is honest — never fabricate an actor.
    #[serde(default)]
    actor: Option<String>,
}

/// The flat column a `related` / `dependsOn` predicate projects to. Only those
/// two predicates project; the rest live purely as typed edges.
fn flat_projection_col(predicate: &str) -> Option<usize> {
    use arrow_kanban::schema::items_col;
    match arrow_kanban::relation_vocab::flat_column_for(predicate) {
        Some("related") => Some(items_col::RELATED),
        Some("depends_on") => Some(items_col::DEPENDS_ON),
        _ => None,
    }
}

/// Mirror a `related` / `dependsOn` edge INTO its flat column (additive) so a typed
/// edge added on EDIT is visible to roadmap / critical-path / worklist / show.
fn add_flat_projection(store: &mut KanbanStore, id: &str, predicate: &str, target: &str) {
    let Some(col) = flat_projection_col(predicate) else {
        return;
    };
    if let Err(e) = store.add_to_list_field(id, col, target) {
        eprintln!("kanban: warning — failed to project {predicate} edge onto {id}: {e}");
    }
}

/// Remove a `related` / `dependsOn` edge FROM its flat column (subtractive).
/// Returns `true` if the flat column actually held the edge and it was removed.
///
/// This must run INDEPENDENTLY of the typed-relation removal. A flat-only
/// edge (written by `--depends-on`/`--related`, which create no typed edge) has no typed
/// relation to remove, so gating the flat removal behind `remove_relation` succeeding left
/// it stuck — `--unrelate` reported `relationships_removed: []` and changed nothing.
fn remove_flat_projection(
    store: &mut KanbanStore,
    id: &str,
    predicate: &str,
    target: &str,
) -> bool {
    let Some(col) = flat_projection_col(predicate) else {
        return false;
    };
    match store.remove_from_list_field(id, col, target) {
        Ok(changed) => changed,
        Err(e) => {
            eprintln!("kanban: warning — failed to remove {predicate} projection on {id}: {e}");
            false
        }
    }
}

pub(crate) fn handle_update_typed(
    req: UpdateRequest,
    store: &mut KanbanStore,
    relations: &mut arrow_kanban::relations::RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    // Subject resolution consults the ITEM store first. A subject the item
    // store does not hold may still be a graph-admitted NON-ITEM entity — a
    // proposal id types as `proposal` through its ontology-declared prefix.
    // Those subjects are RELATE-ONLY: they carry no item row here, so item
    // FIELD mutations refuse by name — but the FORWARD edge spelling works,
    // deleting the asymmetry where every proposal-anchored edge had to be
    // hand-written from the inverse side (an asymmetry that shipped backwards
    // into docs and refusal messages at least once because it is so easy to
    // spell wrong).
    let is_item = store.get_item(&req.id).is_ok();
    if !is_item {
        let t = arrow_kanban::relation_vocab::type_from_id(&req.id);
        let is_entity_class = t.is_some_and(|t| {
            ItemType::from_str_loose(t).is_none()
                && arrow_kanban::relation_vocab::classes()
                    .iter()
                    .any(|c| c.name == t)
        });
        if !is_entity_class {
            return Err(error_response(
                &format!("Item not found: {}", req.id),
                "NOT_FOUND",
            ));
        }
        let wants_fields = req.title.is_some()
            || req.priority.is_some()
            || req.assignee.is_some()
            || req.tags.is_some()
            || req.body.is_some()
            || req.related.is_some()
            || req.depends_on.is_some();
        if wants_fields {
            return Err(error_response(
                &format!(
                    "'{}' is a {} — a graph entity with no item row here; only                      --relate/--unrelate apply to it (its record lives in its own store)",
                    req.id,
                    t.unwrap_or("non-item entity")
                ),
                "ENTITY_RELATE_ONLY",
            ));
        }
    }

    // Validate every edge BEFORE mutating anything, so a bad relationship never
    // leaves the item half-updated.
    let source_type = arrow_kanban::relation_vocab::type_from_id(&req.id).unwrap_or("");
    let add_edges = parse_and_validate(&req.relate, source_type, Some(&req.id))?;
    let del_edges = parse_and_validate(&req.unrelate, source_type, Some(&req.id))?;

    let mut updated = Vec::new();

    if let Some(ref title) = req.title {
        store
            .update_title(&req.id, title)
            .map_err(|e| error_response(&format!("{e}"), "UPDATE_FAILED"))?;
        updated.push("title");
    }
    if let Some(ref priority) = req.priority {
        store
            .update_priority(&req.id, Some(priority))
            .map_err(|e| error_response(&format!("{e}"), "UPDATE_FAILED"))?;
        updated.push("priority");
    }
    if let Some(ref assignee) = req.assignee {
        store
            .update_assignee(&req.id, Some(assignee))
            .map_err(|e| error_response(&format!("{e}"), "UPDATE_FAILED"))?;
        updated.push("assignee");
    }
    if let Some(ref tags) = req.tags {
        store
            .update_tags(&req.id, tags)
            .map_err(|e| error_response(&format!("{e}"), "UPDATE_FAILED"))?;
        updated.push("tags");
    }
    if let Some(ref body) = req.body {
        store
            .update_body(&req.id, Some(body))
            .map_err(|e| error_response(&format!("{e}"), "UPDATE_FAILED"))?;
        updated.push("body");
    }
    if let Some(ref related) = req.related {
        store
            .update_related(&req.id, related)
            .map_err(|e| error_response(&format!("{e}"), "UPDATE_FAILED"))?;
        updated.push("related");
    }
    if let Some(ref depends_on) = req.depends_on {
        store
            .update_depends_on(&req.id, depends_on)
            .map_err(|e| error_response(&format!("{e}"), "UPDATE_FAILED"))?;
        updated.push("depends_on");
    }

    // Apply typed edges. Additive/subtractive — adding one never silently
    // deletes the others, which is the footgun --related/--depends-on still carry.
    let mut added = Vec::new();
    for (pred, target) in &add_edges {
        match relations.add_relation(&req.id, target, pred) {
            Ok(_) => {
                // Keep the flat projection in step. Without this, a typed related/dependsOn
                // edge added on EDIT is invisible to roadmap / critical-path / worklist /
                // show — the flat columns stop being a projection and become a stale
                // second opinion, which is the drift this design exists to prevent.
                // (A relate-only entity subject has no item row to project onto.)
                if is_item {
                    add_flat_projection(store, &req.id, pred, target);
                }
                added.push(format!("{pred}:{target}"));
            }
            Err(e) => eprintln!(
                "kanban: warning — failed to add {} -{pred}-> {target}: {e}",
                req.id
            ),
        }
    }
    let mut removed = Vec::new();
    for (pred, target) in &del_edges {
        // Remove BOTH projections INDEPENDENTLY. A flat-only edge (created
        // by --depends-on/--related, no typed edge) makes remove_relation error, so gating the
        // flat removal behind it left --unrelate a silent no-op. Attempt each; report removed
        // if EITHER held the edge.
        let typed_removed = relations.remove_relation(&req.id, target, pred).is_ok();
        let flat_removed = is_item && remove_flat_projection(store, &req.id, pred, target);
        if typed_removed || flat_removed {
            removed.push(format!("{pred}:{target}"));
        } else {
            eprintln!(
                "kanban: warning — no {} -{pred}-> {target} edge to remove (neither typed nor flat)",
                req.id
            );
        }
    }
    if !added.is_empty() {
        updated.push("relationships_added");
    }
    if !removed.is_empty() {
        updated.push("relationships_removed");
    }

    Ok(KanbanReply::Value(serde_json::json!({
        "id": req.id,
        "updated": updated,
        "relationships_added": added,
        "relationships_removed": removed,
    })))
}

// ── Comment ────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct CommentRequest {
    id: String,
    text: String,
    agent: Option<String>,
}

pub(crate) fn handle_comment_typed(
    req: CommentRequest,
    store: &mut KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    // Verify item exists
    store
        .get_item(&req.id)
        .map_err(|e| error_response(&format!("{e}"), "NOT_FOUND"))?;

    store
        .add_comment(&req.id, &req.text, req.agent.as_deref())
        .map_err(|e| error_response(&format!("{e}"), "COMMENT_FAILED"))?;

    Ok(KanbanReply::Value(serde_json::json!({
        "id": req.id,
        "comment": req.text,
    })))
}

// ── Rank ────────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct RankRequest {
    id: String,
    /// Manual rank. `Some(n)` sets it (lower = higher priority); `None` clears it.
    rank: Option<i32>,
    /// Who issued this command. Stamped into every request envelope by the
    /// client, which is the only party that knows the caller's identity; the
    /// server never resolves it locally (one process would stamp its own host
    /// on every fleet mutation). `None` is honest — never fabricate an actor.
    #[serde(default)]
    actor: Option<String>,
}

pub(crate) fn handle_rank_typed(
    req: RankRequest,
    store: &mut KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    // Verify item exists (so a typo'd ID errors instead of silently no-op'ing).
    store
        .get_item(&req.id)
        .map_err(|e| error_response(&format!("{e}"), "NOT_FOUND"))?;

    store
        .update_rank(&req.id, req.rank)
        .map_err(|e| error_response(&format!("{e}"), "RANK_FAILED"))?;

    Ok(KanbanReply::Value(serde_json::json!({
        "id": req.id,
        "rank": req.rank,
    })))
}

// ── List ────────────────────────────────────────────────────────────────────

#[derive(Clone, Deserialize)]
pub struct ListRequest {
    status: Option<String>,
    item_type: Option<String>,
    board: Option<String>,
    assignee: Option<String>,
    /// Post-filter by resolution (terminal states only — completed,
    /// superseded, wont_do, duplicate, obsolete, merged, refuted).
    #[serde(default)]
    resolution: Option<String>,
    /// Post-filter by priority (critical, high, medium, low).
    #[serde(default)]
    priority: Option<String>,
    /// Post-filter by tag (exact match, multiple = AND). The client
    /// sends `Vec<String>` under the `tags` key; default to empty so older
    /// clients without the field continue to work.
    #[serde(default)]
    tags: Vec<String>,
    /// Post-filter to items with all dependencies met (unblocked).
    #[serde(default)]
    ready: bool,
}

pub(crate) fn handle_list_typed(
    req: ListRequest,
    store: &KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    let mut items = store.query_items(
        req.status.as_deref(),
        req.item_type.as_deref(),
        req.board.as_deref(),
        req.assignee.as_deref(),
    );

    // Apply the post-filters that previously only existed in the
    // local-mode handler (`Commands::List` in arrow-kanban/src/main.rs:1992).
    // Server-mode requests were silently dropping these fields because they
    // were not on `ListRequest` at all, so `arrow-kanban list --tag X` returned the full
    // board regardless of tag.
    if let Some(ref res_filter) = req.resolution {
        items.retain(|batch| {
            let res_col = batch
                .column(arrow_kanban::schema::items_col::RESOLUTION)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("resolution column");
            !arrow::array::Array::is_null(res_col, 0) && res_col.value(0) == res_filter.as_str()
        });
    }

    if let Some(ref pri_filter) = req.priority {
        items.retain(|batch| {
            let pri_col = batch
                .column(arrow_kanban::schema::items_col::PRIORITY)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("priority column");
            !arrow::array::Array::is_null(pri_col, 0) && pri_col.value(0) == pri_filter.as_str()
        });
    }

    if !req.tags.is_empty() {
        let needles: Vec<&str> = req.tags.iter().map(String::as_str).collect();
        items.retain(|batch| {
            let tags_col = batch
                .column(arrow_kanban::schema::items_col::TAGS)
                .as_any()
                .downcast_ref::<arrow::array::ListArray>()
                .expect("tags column");
            // query_items returns one-row-per-batch slices, but be defensive
            // against future batching by checking every row.
            (0..batch.num_rows()).any(|i| {
                if arrow::array::Array::is_null(tags_col, i) {
                    return false;
                }
                let item_tags = tags_col.value(i);
                let tag_arr = item_tags
                    .as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .expect("tag values");
                let len = arrow::array::Array::len(tag_arr);
                let item_tag_set: std::collections::HashSet<&str> =
                    (0..len).map(|j| tag_arr.value(j)).collect();
                needles.iter().all(|t| item_tag_set.contains(*t))
            })
        });
    }

    if req.ready {
        // Recompute against the unfiltered store so dependency resolution sees
        // the full graph, then narrow to items whose ID is in the ready set.
        let all_batches = store.query_items(None, None, None, None);
        let extracted = critical_path::extract_items(&all_batches);
        match critical_path::compute_critical_path(&extracted) {
            Ok(cp) => {
                let ready_set: std::collections::HashSet<&str> =
                    cp.ready.iter().map(String::as_str).collect();
                items.retain(|batch| {
                    let ids = batch
                        .column(arrow_kanban::schema::items_col::ID)
                        .as_any()
                        .downcast_ref::<arrow::array::StringArray>()
                        .expect("id");
                    (0..batch.num_rows()).any(|i| ready_set.contains(ids.value(i)))
                });
            }
            Err(e) => {
                return Err(error_response(
                    &format!("ready filter failed: {e}"),
                    "READY_FILTER_FAILED",
                ));
            }
        }
    }

    let table = display::format_item_table(&items);
    // #28: structured rows ALONGSIDE the rendered table. `table` is a rendering —
    // a wire consumer that needs the data has nothing to parse without this, and
    // fails only at runtime ("missing 'items' field") while compiling clean.
    // `export_json` is the same serializer the `export` verb uses, so field names
    // stay one convention and an id is its own field, never something a caller
    // scrapes out of a rendered line. Bodies are large and `list` is a row
    // surface: strip `body` per row; `show`/`export` are the body surfaces.
    let items_rows: Vec<serde_json::Value> =
        serde_json::from_str::<serde_json::Value>(&export::export_json(&items))
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|mut row| {
                if let Some(obj) = row.as_object_mut() {
                    obj.remove("body");
                }
                row
            })
            .collect();
    Ok(KanbanReply::Value(serde_json::json!({
        "count": items.iter().map(|b| b.num_rows()).sum::<usize>(),
        "table": table,
        "items": items_rows,
    })))
}

// ── Show ────────────────────────────────────────────────────────────────────

#[derive(Clone, Deserialize)]
pub struct ShowRequest {
    id: String,
    format: Option<String>,
}

pub(crate) fn handle_show_typed(
    req: ShowRequest,
    store: &KanbanStore,
    relations: &RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    let item = store
        .get_item(&req.id)
        .map_err(|e| error_response(&format!("{e}"), "NOT_FOUND"))?;

    match req.format.as_deref() {
        Some("md") => {
            let md = export::item_to_markdown(&item, 0);
            Ok(KanbanReply::Value(serde_json::json!({
                "id": req.id,
                "markdown": md,
            })))
        }
        Some("json") => {
            // The FULL item as JSON — its own fields PLUS comments (with
            // timestamps) and the status-history transitions read from the runs
            // table. This is what makes the FA-E3 time-to-orient measure computable
            // from the CLI (claim transition → first work artifact). Previously this
            // emitted only the item's own fields, and the client dropped it entirely.
            let comments = store.list_comments(&req.id);
            let json_str =
                export::export_item_json_full(&item, &comments, store.runs_batches(), &req.id);
            Ok(KanbanReply::Value(serde_json::json!({
                "id": req.id,
                "json": json_str,
            })))
        }
        _ => {
            let mut detail = display::format_item_detail(&item);
            // Issue #26: the typed edges RENDER — `show` is the obvious verification
            // command, and it omitted relations entirely (an agent that had just
            // written an edge could not see it). Direction is spelled out per line;
            // the section is absent when there are no edges (a header over nothing
            // reads as a bug).
            detail.push_str(&format_edges(&req.id, relations));
            let item_comments = store.list_comments(&req.id);
            if !item_comments.is_empty() {
                detail.push_str(&arrow_kanban::comments::format_comments(&item_comments));
            }
            Ok(KanbanReply::Value(serde_json::json!({
                "id": req.id,
                "detail": detail,
            })))
        }
    }
}

/// The typed-edges section for `show` (issue #26). Outgoing then incoming, each
/// line naming the predicate and the far end; empty string when the item has no
/// edges, so an edge-free show stays byte-stable.
fn format_edges(item_id: &str, relations: &RelationsStore) -> String {
    use arrow::array::{Array, BooleanArray, StringArray};
    use arrow_kanban::schema::rel_col;
    let mut out_lines: Vec<String> = Vec::new();
    let mut in_lines: Vec<String> = Vec::new();
    for batch in relations.query_relations(item_id) {
        let col = |i: usize| {
            batch
                .column(i)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("relations string column")
                .value(0)
                .to_string()
        };
        let deleted = batch
            .column(rel_col::DELETED)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("deleted column");
        if batch.num_rows() != 1 || deleted.value(0) {
            continue;
        }
        let (source, target, predicate) = (
            col(rel_col::SOURCE_ID),
            col(rel_col::TARGET_ID),
            col(rel_col::PREDICATE),
        );
        if source == item_id {
            out_lines.push(format!("    {predicate} -> {target}\n"));
        } else {
            in_lines.push(format!("    {source} -> {predicate} -> (this)\n"));
        }
    }
    if out_lines.is_empty() && in_lines.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n  Edges\n");
    for l in out_lines.into_iter().chain(in_lines) {
        s.push_str(&l);
    }
    s
}

// ── Board ───────────────────────────────────────────────────────────────────

#[derive(Clone, Deserialize)]
pub struct BoardRequest {
    board: Option<String>,
}

pub(crate) fn handle_board_typed(
    req: BoardRequest,
    store: &KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    let board_name = req.board.as_deref().unwrap_or("development");

    let items = store.query_items(None, None, Some(board_name), None);
    let states = states_as_strings();
    let view = display::format_board_view(&items, &states);
    Ok(KanbanReply::Value(serde_json::json!({
        "board": board_name,
        "view": view,
    })))
}

// ── Stats ───────────────────────────────────────────────────────────────────

pub(crate) fn handle_stats_typed(
    payload: &[u8],
    store: &KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    // Parse optional flags from payload
    let req: serde_json::Value = serde_json::from_slice(payload).unwrap_or(serde_json::json!({}));

    let velocity = req
        .get("velocity")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let burndown = req
        .get("burndown")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let by_agent = req
        .get("by_agent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let weeks = req.get("weeks").and_then(|v| v.as_u64()).unwrap_or(4) as u32;
    let since = req.get("since").and_then(|v| v.as_str());

    // Dispatch to the appropriate stats function
    if velocity {
        let data = arrow_kanban::stats::compute_velocity(store.runs_batches(), weeks);
        let formatted = arrow_kanban::stats::format_velocity(&data);
        return Ok(KanbanReply::Value(
            serde_json::json!({ "stats": formatted }),
        ));
    }

    if burndown {
        let since_ms = since
            .and_then(arrow_kanban::stats::parse_date_to_ms)
            .unwrap_or_else(|| {
                chrono::Utc::now().timestamp_millis() - (weeks as i64 * 7 * 24 * 60 * 60 * 1000)
            });
        let data = arrow_kanban::stats::compute_burndown(
            store.items_batches(),
            store.runs_batches(),
            since_ms,
        );
        let formatted = arrow_kanban::stats::format_burndown(&data);
        return Ok(KanbanReply::Value(
            serde_json::json!({ "stats": formatted }),
        ));
    }

    if by_agent {
        let data = arrow_kanban::stats::compute_agent_stats(store.runs_batches());
        let formatted = arrow_kanban::stats::format_agent_stats(&data);
        return Ok(KanbanReply::Value(
            serde_json::json!({ "stats": formatted }),
        ));
    }

    // Default: basic stats
    let states = states_as_strings();
    let stats = display::format_stats(store.items_batches(), &states);
    Ok(KanbanReply::Value(serde_json::json!({
        "stats": stats,
    })))
}

// ── Delete ──────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct DeleteRequest {
    id: String,
    /// Who issued this command. Stamped into every request envelope by the
    /// client, which is the only party that knows the caller's identity; the
    /// server never resolves it locally (one process would stamp its own host
    /// on every fleet mutation). `None` is honest — never fabricate an actor.
    #[serde(default)]
    actor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DeleteResponse {
    id: String,
    deleted: bool,
}

pub(crate) fn handle_delete_typed(
    req: DeleteRequest,
    store: &mut KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    store
        .delete_item(&req.id)
        .map_err(|e| error_response(&format!("{e}"), "DELETE_FAILED"))?;

    Ok(KanbanReply::Delete(DeleteResponse {
        id: req.id,
        deleted: true,
    }))
}

// ── Validate (HDD) ─────────────────────────────────────────────────────────

pub(crate) fn handle_validate_typed(
    store: &KanbanStore,
    relations: &RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    let issues = arrow_kanban::validate_hdd(store, relations);

    Ok(KanbanReply::Value(serde_json::json!({
        "valid": issues.is_empty(),
        "issue_count": issues.len(),
        "issues": issues,
    })))
}

// ── Export ───────────────────────────────────────────────────────────────────

/// `id` is `Option` (the client sends `id: null` for a board-wide export; the
/// old non-optional `String` rejected it with "invalid type: null, expected a string").
/// `format`/`board`/`item_type`/`status` carry the board-export query + the requested
/// format so `--format json` is honored instead of always falling back to markdown.
#[derive(Clone, Deserialize)]
pub struct ExportRequest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    board: Option<String>,
    #[serde(default)]
    item_type: Option<String>,
    #[serde(default)]
    status: Option<String>,
    /// Board-wide pagination — the row offset of the requested page.
    #[serde(default)]
    offset: Option<usize>,
    /// Board-wide pagination — the max rows in the page. `None` = whole board
    /// in one reply (the pre-pagination behavior; only safe for small boards, since a full
    /// ~4577-item JSON exceeds the NATS max_payload and the reply never arrives).
    #[serde(default)]
    limit: Option<usize>,
}

/// Slice a paginated window `[offset, offset+limit)` out of a batched query result, spanning
/// `RecordBatch` boundaries. Returns the batches (each a zero-copy `slice`) whose rows
/// fall in the window, in order. An out-of-range window yields an empty vec. This is what keeps a
/// board-wide export page under the NATS max_payload — the client requests fixed-size pages and
/// reassembles them, instead of the server building one oversized reply that never sends.
fn slice_batches(
    batches: &[arrow::array::RecordBatch],
    offset: usize,
    limit: usize,
) -> Vec<arrow::array::RecordBatch> {
    let mut out = Vec::new();
    let end = offset.saturating_add(limit);
    let mut base = 0usize; // global row index at the start of the current batch
    for b in batches {
        let n = b.num_rows();
        let (batch_start, batch_end) = (base, base + n);
        // Intersect the requested window [offset, end) with this batch's [batch_start, batch_end).
        let lo = offset.max(batch_start);
        let hi = end.min(batch_end);
        if lo < hi {
            out.push(b.slice(lo - batch_start, hi - lo));
        }
        base = batch_end;
        if base >= end {
            break;
        }
    }
    out
}

pub(crate) fn handle_export_typed(
    req: ExportRequest,
    store: &KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    let format = req.format.as_deref().unwrap_or("item");

    // Single-item export — honor the requested format. Previously this ALWAYS returned
    // markdown, so `arrow-kanban export --id X --format json` silently fell back to YAML client-side.
    if let Some(id) = &req.id {
        let item = store
            .get_item(id)
            .map_err(|e| error_response(&format!("{e}"), "NOT_FOUND"))?;
        let (out_format, content) = match format {
            "json" => ("json", export::export_json(std::slice::from_ref(&item))),
            _ => ("markdown", export::item_to_markdown(&item, 0)),
        };
        return Ok(KanbanReply::Value(serde_json::json!({
            "id": id,
            "format": out_format,
            "content": content,
        })));
    }

    // Board-wide export (no id → the client sends `id: null`). Reuses the same
    // `query_items` path as `handle_list`, then the proven `export_json` /
    // `export_markdown_table` formatters (which already null-guard priority/assignee).
    let items = store.query_items(
        req.status.as_deref(),
        req.item_type.as_deref(),
        req.board.as_deref(),
        None,
    );
    let total: usize = items.iter().map(|b| b.num_rows()).sum();
    // Page the result when the client requests a `limit`, so no single reply exceeds the
    // NATS max_payload (a full ~4577-item JSON does, and the reply then never arrives → the client
    // deadline elapses). `limit == None` keeps the whole-board reply for backward compatibility.
    let offset = req.offset.unwrap_or(0);
    let page: Vec<arrow::array::RecordBatch> = match req.limit {
        Some(limit) => slice_batches(&items, offset, limit),
        None => items,
    };
    let count: usize = page.iter().map(|b| b.num_rows()).sum();
    let content = match format {
        "json" => export::export_json(&page),
        "markdown" => export::export_markdown_table(&page),
        other => {
            return Err(error_response(
                &format!(
                    "board-wide export format '{other}' is not supported server-side \
                     (use json or markdown); other formats require a single --id"
                ),
                "INVALID_PAYLOAD",
            ));
        }
    };
    // `total`/`offset`/`count` let the client loop pages until `offset + count >= total`.
    Ok(KanbanReply::Value(serde_json::json!({
        "id": serde_json::Value::Null,
        "format": format,
        "content": content,
        "total": total,
        "offset": offset,
        "count": count,
    })))
}

// ── Roadmap / Critical Path / Worklist ───────────────────────────────────────
//
// Graph computation on the server's RecordBatches — no data leaves the server.

#[derive(Clone, Deserialize)]
pub struct FlowRequest {
    /// wip | aging | throughput | cfd
    pub mode: Option<String>,
    pub board: Option<String>,
}

/// Flow & WIP charting — everything derives from the runs audit trail
/// joined to items (arrow_kanban::flow); no new persistence. Config comes from
/// the deployment's config file under the data dir (the engine convention, with
/// the compiled default as the announced fallback — never silent).
pub(crate) fn handle_flow_typed(
    req: FlowRequest,
    store: &KanbanStore,
    data_dir: &std::path::Path,
) -> Result<KanbanReply, Vec<u8>> {
    use arrow_kanban::flow;
    let cfg_path = data_dir.join(".arrow-kanban/config.yaml");
    let (config, cfg_src) =
        match arrow_kanban::config::ConfigFile::from_path(&cfg_path) {
            Ok(c) => (c, "deployment config".to_string()),
            Err(_) => (
                arrow_kanban::config::ConfigFile::from_yaml(
                    arrow_kanban::config::default_config_yaml(),
                )
                .map_err(|e| {
                    error_response(&format!("default config unparseable: {e}"), "FLOW_CONFIG")
                })?,
                "COMPILED DEFAULT (deployment config absent)".to_string(),
            ),
        };
    let board_name = req.board.as_deref().unwrap_or("development");
    let board = config
        .board(board_name)
        .map_err(|e| error_response(&format!("{e}"), "FLOW_CONFIG"))?;

    let items = flow::flow_items(store);
    let trans = flow::transitions(store);
    // Read from the one source (state_machine::TERMINAL_STATES) rather than
    // re-hardcoding — the drift surface flagged in CH-8304.
    let terminal: std::collections::BTreeSet<String> = arrow_kanban::state_machine::TERMINAL_STATES
        .iter()
        .map(|s| s.to_string())
        .collect();
    let day_ms: i64 = 86_400_000;

    let mode = req.mode.as_deref().unwrap_or("wip");
    let view = match mode {
        "wip" => {
            let (rows, per_agent) = flow::wip(&items, board);
            flow::render_wip(&rows, &per_agent)
        }
        "aging" => {
            let now = chrono::Utc::now().timestamp_millis();
            let rows = flow::aging(&items, &trans, &terminal, now);
            flow::render_aging(&rows)
        }
        "throughput" => {
            let buckets = flow::throughput(&trans, &items, &terminal, day_ms);
            flow::render_throughput(&buckets, day_ms)
        }
        "cfd" => {
            let series = flow::status_series(&trans, &items, day_ms, board_name);
            flow::render_cfd(&series)
        }
        other => {
            return Err(error_response(
                &format!("unknown flow mode '{other}' — valid: wip, aging, throughput, cfd"),
                "FLOW_MODE",
            ));
        }
    };
    Ok(KanbanReply::Value(serde_json::json!({
        "view": format!("{view}config: {cfg_src}
    "),
    })))
}

#[cfg(test)]
mod flow_terminal_tests {
    //! CH-8304: handle_flow_typed's terminal set must be READ from
    //! state_machine::TERMINAL_STATES, never re-hardcoded — a hardcoded copy
    //! that drifts from the one source silently misclassifies items.
    use super::*;
    use arrow_kanban::crud::{CreateItemInput, KanbanStore};
    use arrow_kanban::item_type::ItemType;

    fn store_with_one_item_in(status: &str) -> KanbanStore {
        let mut store = KanbanStore::new();
        let id = store
            .create_item(&CreateItemInput {
                title: "t".into(),
                item_type: ItemType::Chore,
                priority: None,
                assignee: None,
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: None,
            })
            .unwrap();
        // Bypass lifecycle transition rules — this test only exercises the
        // flow charts' terminal classification, not the state machine.
        store.update_status(&id, status, None, true, None).unwrap();
        store
    }

    /// Every status the ONE source calls terminal must be excluded from aging
    /// and counted by throughput — proving handle_flow_typed reads the source
    /// rather than a separately-maintained list that could drift from it.
    #[test]
    fn every_state_machine_terminal_status_is_terminal_in_flow_charts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for status in arrow_kanban::state_machine::TERMINAL_STATES.iter().copied() {
            let store = store_with_one_item_in(status);
            let aging = handle_flow_typed(
                FlowRequest {
                    mode: Some("aging".into()),
                    board: None,
                },
                &store,
                tmp.path(),
            )
            .expect("aging ok");
            let aging_view = aging_view_str(&aging);
            assert!(
                aging_view.contains("no measurable non-terminal items"),
                "status '{status}' (state_machine::TERMINAL_STATES) leaked into aging \
                 as non-terminal: {aging_view}"
            );

            let throughput = handle_flow_typed(
                FlowRequest {
                    mode: Some("throughput".into()),
                    board: None,
                },
                &store,
                tmp.path(),
            )
            .expect("throughput ok");
            let throughput_view = aging_view_str(&throughput);
            assert!(
                !throughput_view.contains("no terminal transitions recorded"),
                "status '{status}' (state_machine::TERMINAL_STATES) was NOT counted as a \
                 terminal arrival by throughput: {throughput_view}"
            );
        }
    }

    fn aging_view_str(reply: &KanbanReply) -> String {
        reply.to_value()["view"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

#[derive(Clone, Deserialize)]
pub struct RoadmapRequest {
    #[serde(default)]
    flat: bool,
    #[serde(default)]
    ready: bool,
    /// Roll up a campaign (CA-XXXX) — its partOf member voyages, %-done, and aggregate.
    #[serde(default)]
    campaign: Option<String>,
}

pub(crate) fn handle_roadmap_typed(
    req: RoadmapRequest,
    store: &KanbanStore,
    relations: &RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    let all_batches = store.query_items(None, None, None, None);

    if all_batches.is_empty() {
        return Ok(KanbanReply::Value(
            serde_json::json!({ "view": "No items found.\n" }),
        ));
    }

    let mut items = critical_path::extract_items(&all_batches);
    // Resolve the typed `implements`/`spawns` expedition→voyage edges into `related`
    // so an `implements`-only expedition counts toward its voyage — identically to the local CLI.
    critical_path::fold_typed_voyage_memberships(&mut items, relations);

    // Campaign rollup — the SAME shared library renderer the local CLI uses, so
    // `arrow-kanban roadmap --campaign` renders identically in local and --server mode.
    if let Some(camp_id) = &req.campaign {
        let members = relations.incoming_by_predicate(camp_id, "partOf");
        let view = critical_path::format_campaign_roadmap(camp_id, &members, &items);
        return Ok(KanbanReply::Value(serde_json::json!({ "view": view })));
    }

    let cp = critical_path::compute_critical_path(&items)
        .map_err(|e| error_response(&e, "CYCLE_DETECTED"))?;

    let view = if req.flat {
        let mut backlog: Vec<_> = items.iter().filter(|i| i.status == "backlog").collect();
        backlog.sort_by_key(|i| critical_path::sort_key(i));
        let mut lines = vec![format!(
            "Roadmap (flat, ranked by manual rank then priority — {} backlog items):\n",
            backlog.len()
        )];
        lines.push(format!(
            "  {:<14}{:<50}{:<10}{:<10}",
            "ID", "TITLE", "PRIORITY", "ASSIGNEE"
        ));
        for item in &backlog {
            let title = critical_path::truncate(&item.title, 48);
            lines.push(format!(
                "  {:<14}{:<50}{:<10}{:<10}",
                item.id, title, item.priority, item.assignee
            ));
        }
        lines.join("\n")
    } else if req.ready {
        let ready_items: Vec<_> = items.iter().filter(|i| cp.ready.contains(&i.id)).collect();
        let mut lines = vec![format!("Ready Items ({}):\n", ready_items.len())];
        for item in &ready_items {
            lines.push(format!(
                "  {} {} [{}] ({})",
                item.id, item.title, item.status, item.priority
            ));
        }
        if ready_items.is_empty() {
            lines.push("  (none)".into());
        }
        lines.join("\n")
    } else {
        let (groups, orphans) = critical_path::group_by_voyage(&items);
        critical_path::format_roadmap(&items, &groups, &orphans, &cp)
    };

    Ok(KanbanReply::Value(serde_json::json!({ "view": view })))
}

pub(crate) fn handle_critical_path_typed(store: &KanbanStore) -> Result<KanbanReply, Vec<u8>> {
    let all_batches = store.query_items(None, None, None, None);

    if all_batches.is_empty() {
        return Ok(KanbanReply::Value(
            serde_json::json!({ "view": "No items found.\n" }),
        ));
    }

    let items = critical_path::extract_items(&all_batches);
    let cp = critical_path::compute_critical_path(&items)
        .map_err(|e| error_response(&e, "CYCLE_DETECTED"))?;
    let view = critical_path::format_critical_path(&items, &cp);

    Ok(KanbanReply::Value(serde_json::json!({ "view": view })))
}

#[derive(Clone, Deserialize)]
pub struct WorklistRequest {
    #[serde(default = "default_agents")]
    agents: String,
    #[serde(default = "default_depth")]
    depth: usize,
    /// Agent capabilities for capability-aware routing, as `Agent=cap1,cap2;Agent2=cap3`
    /// (same wire format as the CLI `--capabilities` flag). An item tagged
    /// `<cap>-required` is routed only to agents that provide `<cap>`. Consumer policy;
    /// default empty = no capability constraints.
    #[serde(default)]
    capabilities: String,
}

pub(crate) fn default_agents() -> String {
    String::new()
}

pub(crate) fn default_depth() -> usize {
    3
}

pub(crate) fn handle_worklist_typed(
    req: WorklistRequest,
    store: &KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    let all_batches = store.query_items(None, None, None, None);

    if all_batches.is_empty() {
        return Ok(KanbanReply::Value(
            serde_json::json!({ "view": "No items found.\n" }),
        ));
    }

    let items = critical_path::extract_items(&all_batches);
    let cp = critical_path::compute_critical_path(&items)
        .map_err(|e| error_response(&e, "CYCLE_DETECTED"))?;
    let agent_list: Vec<String> = if req.agents.trim().is_empty() {
        critical_path::agents_from_items(&items)
    } else {
        req.agents
            .split(',')
            .map(|s| s.trim().to_string())
            .collect()
    };
    let routing = critical_path::CapabilityRouting {
        item_requirements: critical_path::item_requirements_from_batches(&all_batches),
        agent_capabilities: critical_path::parse_agent_capabilities(&req.capabilities),
    };
    let worklist = critical_path::generate_worklist(&items, &cp, &agent_list, &routing, req.depth);
    let view = critical_path::format_worklist(&worklist);

    Ok(KanbanReply::Value(serde_json::json!({ "view": view })))
}

// ── Next ID ─────────────────────────────────────────────────────────────────

#[derive(Clone, Deserialize)]
pub struct NextIdRequest {
    item_type: String,
}

pub(crate) fn handle_next_id_typed(
    req: NextIdRequest,
    store: &KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    let item_type = ItemType::from_str_loose(&req.item_type).ok_or_else(|| {
        error_response(
            &format!("Unknown item type: {}", req.item_type),
            "INVALID_TYPE",
        )
    })?;

    let next = arrow_kanban::allocate_id(store.items_batches(), item_type);
    Ok(KanbanReply::Value(serde_json::json!({
        "next_id": next,
    })))
}

// ── History ─────────────────────────────────────────────────────────────────

#[derive(Clone, Deserialize)]
pub struct HistoryRequest {
    #[serde(default)]
    week: bool,
    #[serde(default)]
    month: bool,
    since: Option<String>,
    by_assignee: Option<String>,
}

pub(crate) fn handle_history_typed(
    req: HistoryRequest,
    store: &KanbanStore,
) -> Result<KanbanReply, Vec<u8>> {
    // Use the new filter_history when any enhanced flag is set
    if req.month || req.since.is_some() || req.by_assignee.is_some() {
        let since_ms = if let Some(ref date) = req.since {
            arrow_kanban::stats::parse_date_to_ms(date)
                .ok_or_else(|| error_response("Invalid date format (use YYYY-MM-DD)", "BAD_DATE"))?
        } else if req.month {
            chrono::Utc::now().timestamp_millis() - (30 * 24 * 60 * 60 * 1000)
        } else {
            chrono::Utc::now().timestamp_millis() - (7 * 24 * 60 * 60 * 1000)
        };

        let entries = arrow_kanban::stats::filter_history(
            store.items_batches(),
            store.runs_batches(),
            since_ms,
            req.by_assignee.as_deref(),
        );
        let formatted = arrow_kanban::stats::format_history_entries(&entries);
        return Ok(KanbanReply::Value(
            serde_json::json!({ "history": formatted }),
        ));
    }

    if req.week {
        let cutoff = chrono::Utc::now().timestamp_millis() - (7 * 24 * 60 * 60 * 1000);
        let done_items = store.query_items(Some("done"), None, None, None);
        let recent = filter_recently_completed(store, &done_items, cutoff);

        let table = display::format_item_table(&recent);
        let history = if recent.is_empty() {
            "No items completed this week.\n".to_string()
        } else {
            format!("Completed this week ({}):\n{}", recent.len(), table)
        };
        Ok(KanbanReply::Value(
            serde_json::json!({ "history": history }),
        ))
    } else {
        // Default: show 20 most recently completed items (not all 1000+)
        let since_ms = chrono::Utc::now().timestamp_millis() - (30 * 24 * 60 * 60 * 1000);
        let mut entries = arrow_kanban::stats::filter_history(
            store.items_batches(),
            store.runs_batches(),
            since_ms,
            None,
        );
        entries.truncate(20);
        let formatted = if entries.is_empty() {
            "No recently completed items.\n".to_string()
        } else {
            arrow_kanban::stats::format_history_entries(&entries)
        };
        Ok(KanbanReply::Value(
            serde_json::json!({ "history": formatted }),
        ))
    }
}

/// Filter done items to those that transitioned to "done" after `cutoff_ms`.
///
/// Checks the runs table for `to_status == "done"` transitions. Falls back to
/// item creation timestamp if no matching run exists.
pub(crate) fn filter_recently_completed(
    store: &KanbanStore,
    done_items: &[arrow::array::RecordBatch],
    cutoff_ms: i64,
) -> Vec<arrow::array::RecordBatch> {
    use arrow::array::{Array, StringArray, TimestampMillisecondArray};
    use arrow_kanban::schema::{items_col, runs_col};

    // Build a set of item IDs that transitioned to "done" after cutoff
    let mut recent_ids = std::collections::HashSet::new();
    for batch in store.runs_batches() {
        let item_ids = batch
            .column(runs_col::ITEM_ID)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("item_id");
        let to_statuses = batch
            .column(runs_col::TO_STATUS)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("to_status");
        let timestamps = batch
            .column(runs_col::TIMESTAMP)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .expect("timestamp");

        for i in 0..batch.num_rows() {
            if to_statuses.value(i) == "done"
                && !timestamps.is_null(i)
                && timestamps.value(i) > cutoff_ms
            {
                recent_ids.insert(item_ids.value(i).to_string());
            }
        }
    }

    // If runs table has matches, use those. Otherwise fall back to creation time.
    if !recent_ids.is_empty() {
        done_items
            .iter()
            .filter(|batch| {
                let ids = batch
                    .column(items_col::ID)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("id");
                (0..batch.num_rows()).any(|i| recent_ids.contains(ids.value(i)))
            })
            .cloned()
            .collect()
    } else {
        // Fallback: filter by creation timestamp (less accurate but works
        // when runs table is empty/sparse)
        done_items
            .iter()
            .filter(|batch| {
                let created = batch
                    .column(items_col::CREATED)
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .expect("created");
                (0..batch.num_rows()).any(|i| !created.is_null(i) && created.value(i) > cutoff_ms)
            })
            .cloned()
            .collect()
    }
}

// ── Blocked ─────────────────────────────────────────────────────────────────

pub(crate) fn handle_blocked_typed(store: &KanbanStore) -> Result<KanbanReply, Vec<u8>> {
    use arrow::array::{Array, BooleanArray, ListArray, StringArray};
    use arrow_kanban::schema::items_col;

    // Build set of done item IDs
    let mut done_ids = std::collections::HashSet::new();
    for batch in store.items_batches() {
        let ids = batch
            .column(items_col::ID)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("id");
        let statuses = batch
            .column(items_col::STATUS)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("status");
        let deleted = batch
            .column(items_col::DELETED)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("deleted");
        for i in 0..batch.num_rows() {
            // A dependency is satisfied by ANY terminal state, not just `done`.
            // Research-board items terminate at `complete` (experiment/literature) or
            // `retired` (hypothesis/measure) and NEVER reach `done`, so a bare `== "done"` here
            // reported every item depending on a finished research item as blocked. The five
            // experiments that were `complete` with unmet deps
            // appeared in the blocked-item listing when they should not.
            if !deleted.value(i)
                && arrow_kanban::state_machine::is_terminal_state(statuses.value(i))
            {
                done_ids.insert(ids.value(i).to_string());
            }
        }
    }

    // Filter to items with unmet dependencies
    let all = store.query_items(None, None, None, None);
    let blocked: Vec<arrow::array::RecordBatch> = all
        .into_iter()
        .filter(|batch| {
            let depends = batch
                .column(items_col::DEPENDS_ON)
                .as_any()
                .downcast_ref::<ListArray>()
                .expect("depends_on");
            let statuses = batch
                .column(items_col::STATUS)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("status");
            (0..batch.num_rows()).any(|i| {
                // An item in ANY terminal state is not itself blocked (the SUBJECT side).
                // Same vocabulary as the satisfied-set above: complete/retired items from the
                // research board should not appear in blocked output.
                if arrow_kanban::state_machine::is_terminal_state(statuses.value(i)) {
                    return false;
                }
                if depends.is_null(i) || depends.value(i).is_empty() {
                    return false;
                }
                let dep_list = depends.value(i);
                let dep_strings = dep_list
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("dep values");
                // An empty-string dep is not a real dependency — skip it,
                // so a legacy `[""]` row (or any stray empty element) is not reported blocked
                // here while `--ready` (critical_path) already treats `""` as satisfied. Left
                // unfiltered, the same item read BOTH blocked and ready, which cannot be true.
                (0..dep_strings.len()).any(|j| {
                    let d = dep_strings.value(j);
                    !d.trim().is_empty() && !done_ids.contains(d)
                })
            })
        })
        .collect();

    let table = display::format_item_table(&blocked);
    Ok(KanbanReply::Value(serde_json::json!({
        "count": blocked.iter().map(|b| b.num_rows()).sum::<usize>(),
        "table": if blocked.is_empty() {
            "No blocked items.\n".to_string()
        } else {
            format!("Blocked Items ({}):\n{}", blocked.len(), table)
        },
    })))
}

// ── HDD Create ──────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct HddCreateRequest {
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    related: Vec<String>,
    body: Option<String>,
    /// Paper number to link a hypothesis to (e.g. 130 for PAPER-130).
    paper: Option<u32>,
    /// Hypothesis ID to link an experiment to (e.g. H130.1).
    hypothesis: Option<String>,
    /// Experiment ID to link a measure to (optional, e.g. EXPR-130.1).
    experiment: Option<String>,
    /// Who issued this command (request envelope, client-stamped).
    #[serde(default)]
    actor: Option<String>,
}

pub(crate) fn handle_hdd_create_typed(
    req: HddCreateRequest,
    store: &mut KanbanStore,
    relations: &mut RelationsStore,
    item_type: ItemType,
) -> Result<KanbanReply, Vec<u8>> {
    // Each HDD-typed arm produces (id, body_needs_separate_update). The
    // typed helpers (create_paper/create_hypothesis/...) don't accept a
    // body, so the caller-supplied body has to be applied via update_body
    // after the create. The fallback arm sets body inline via
    // CreateItemInput, so it doesn't need a second pass.
    let (id, body_needs_update) = match item_type {
        ItemType::Paper => {
            let result = arrow_kanban::create_paper(
                store,
                &req.title,
                req.tags.clone(),
                req.related.clone(),
            )
            .map_err(|e| error_response(&format!("{e}"), "HDD_CREATE_FAILED"))?;
            (result.id, true)
        }
        ItemType::Hypothesis => {
            let paper = req.paper.ok_or_else(|| {
                error_response(
                    "hdd.hypothesis requires `paper` (paper number to link to)",
                    "HDD_CREATE_FAILED",
                )
            })?;
            let result = arrow_kanban::create_hypothesis(
                store,
                relations,
                &req.title,
                paper,
                req.tags.clone(),
                req.related.clone(),
            )
            .map_err(|e| error_response(&format!("{e}"), "HDD_CREATE_FAILED"))?;
            (result.id, true)
        }
        ItemType::Experiment => {
            let hyp = req.hypothesis.as_deref().ok_or_else(|| {
                error_response(
                    "hdd.experiment requires `hypothesis` (hypothesis ID to link to)",
                    "HDD_CREATE_FAILED",
                )
            })?;
            let result = arrow_kanban::create_experiment(
                store,
                relations,
                &req.title,
                hyp,
                req.tags.clone(),
                req.related.clone(),
            )
            .map_err(|e| error_response(&format!("{e}"), "HDD_CREATE_FAILED"))?;
            (result.id, true)
        }
        ItemType::Measure => {
            let result = arrow_kanban::create_measure(
                store,
                relations,
                &req.title,
                req.experiment.as_deref(),
                req.tags.clone(),
                req.related.clone(),
            )
            .map_err(|e| error_response(&format!("{e}"), "HDD_CREATE_FAILED"))?;
            (result.id, true)
        }
        ItemType::Idea => {
            let result =
                arrow_kanban::create_idea(store, &req.title, req.tags.clone(), req.related.clone())
                    .map_err(|e| error_response(&format!("{e}"), "HDD_CREATE_FAILED"))?;
            (result.id, true)
        }
        ItemType::Literature => {
            let result = arrow_kanban::create_literature(
                store,
                &req.title,
                req.tags.clone(),
                req.related.clone(),
            )
            .map_err(|e| error_response(&format!("{e}"), "HDD_CREATE_FAILED"))?;
            (result.id, true)
        }
        _ => {
            let input = CreateItemInput {
                title: req.title.clone(),
                item_type,
                priority: None,
                assignee: None,
                tags: req.tags,
                related: req.related,
                depends_on: vec![],
                body: req.body.clone(),
            };
            let id = store
                .create_item(&input)
                .map_err(|e| error_response(&format!("{e}"), "HDD_CREATE_FAILED"))?;
            (id, false)
        }
    };

    if body_needs_update
        && let Some(body) = req.body.as_deref()
        && let Err(e) = store.update_body(&id, Some(body))
    {
        return Err(error_response(
            &format!("created {id} but failed to set body: {e}"),
            "HDD_CREATE_FAILED",
        ));
    }

    Ok(KanbanReply::Create(CreateResponse {
        id,
        title: req.title,
        item_type: item_type.as_str().to_string(),
        status: "backlog".to_string(),
        relationships: Vec::new(),
    }))
}

// ── HDD Validate ────────────────────────────────────────────────────────────

pub(crate) fn handle_hdd_validate_typed(
    store: &KanbanStore,
    relations: &RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    let result = arrow_kanban::validate_hdd(store, relations);
    Ok(KanbanReply::Value(serde_json::json!({
        "valid": result.is_empty(),
        "issues": result,
    })))
}

// ── HDD Registry ────────────────────────────────────────────────────────────

pub(crate) fn handle_hdd_registry_typed(
    store: &KanbanStore,
    relations: &RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    let chains = arrow_kanban::build_registry(store, relations);

    // Manually serialize since RegistryChain doesn't derive Serialize
    let registry: Vec<serde_json::Value> = chains
        .iter()
        .map(|chain| {
            serde_json::json!({
                "paper_id": chain.paper_id,
                "paper_title": chain.paper_title,
                "hypotheses": chain.hypotheses.iter().map(|h| {
                    serde_json::json!({
                        "id": h.id,
                        "title": h.title,
                        "experiments": h.experiments.iter().map(|e| {
                            serde_json::json!({
                                "id": e.id,
                                "title": e.title,
                            })
                        }).collect::<Vec<_>>(),
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(KanbanReply::Value(
        serde_json::json!({ "registry": registry }),
    ))
}

// ── Templates ───────────────────────────────────────────────────────────────
pub(crate) fn handle_templates_typed(
    payload: &[u8],
    root: &std::path::Path,
) -> Result<KanbanReply, Vec<u8>> {
    let params: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|e| error_response(&e.to_string(), "INVALID_JSON"))?;

    let loader = arrow_kanban::templates::ShapeLoader::new(root);
    let generator = arrow_kanban::templates::TemplateGenerator::new(loader);

    if let Some(type_str) = params.get("item_type").and_then(|v| v.as_str()) {
        if let Some(it) = arrow_kanban::ItemType::from_str_loose(type_str) {
            let template = generator.generate(&it, "<Title>");
            Ok(KanbanReply::Value(
                serde_json::json!({ "template": template }),
            ))
        } else {
            Err(error_response(
                &format!("Unknown item type: {type_str}"),
                "UNKNOWN_TYPE",
            ))
        }
    } else {
        let summaries = generator.list_all();
        let types: Vec<serde_json::Value> = summaries
            .iter()
            .map(|s| {
                serde_json::json!({
                    "type": s.item_type.as_str(),
                    "description": s.description
                })
            })
            .collect();
        Ok(KanbanReply::Value(serde_json::json!({ "types": types })))
    }
}

#[cfg(test)]
mod ratify_tests {
    //! First-class phase ratification through the server handler.
    use super::*;
    use arrow_kanban::crud::{CreateItemInput, KanbanStore};
    use arrow_kanban::item_type::ItemType;

    fn item(store: &mut KanbanStore, ty: ItemType, tags: &[&str]) -> String {
        store
            .create_item(&CreateItemInput {
                title: "r".into(),
                item_type: ty,
                priority: None,
                assignee: None,
                tags: tags.iter().map(|s| s.to_string()).collect(),
                related: vec![],
                depends_on: vec![],
                body: None,
            })
            .unwrap()
    }

    #[test]
    fn handle_ratify_strips_phase_and_reports_zero_remaining() {
        let mut store = KanbanStore::new();
        item(
            &mut store,
            ItemType::Expedition,
            &["v19-phase-q", "pending-ratification"],
        );
        item(
            &mut store,
            ItemType::Voyage,
            &["v19-phase-q", "pending-ratification"],
        );

        let payload =
            serde_json::to_vec(&serde_json::json!({ "phase_tag": "v19-phase-q" })).unwrap();
        let req: RatifyRequest = serde_json::from_slice(&payload).unwrap();
        let resp = handle_ratify_typed(req, &mut store)
            .expect("ratify ok")
            .into_bytes();
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();

        assert_eq!(v["remaining_pending"], 0);
        assert_eq!(v["stripped"].as_array().unwrap().len(), 2);
        assert_eq!(v["voyages_started"].as_array().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod claim_tests {
    //! Atomic exclusive claim through the server's single-writer move handler.
    use super::*;
    use arrow_kanban::crud::{CreateItemInput, KanbanStore};
    use arrow_kanban::item_type::ItemType;

    fn new_item(store: &mut KanbanStore, title: &str) -> String {
        store
            .create_item(&CreateItemInput {
                title: title.into(),
                item_type: ItemType::Expedition,
                priority: None,
                assignee: None,
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: None,
            })
            .expect("create")
    }

    fn move_req(id: &str, status: &str, assignee: &str, force: bool) -> MoveRequest {
        let payload = serde_json::to_vec(&serde_json::json!({
            "id": id, "status": status, "assignee": assignee, "force": force
        }))
        .unwrap();
        serde_json::from_slice(&payload).unwrap()
    }

    fn assignee_of(store: &KanbanStore, id: &str) -> Option<String> {
        store.status_and_assignee(id).unwrap().1
    }

    #[test]
    fn move_claim_sticks_then_blocks_cross_agent_and_force_overrides() {
        let mut store = KanbanStore::new();
        let id = new_item(&mut store, "Contested");

        // Mini claims it: move succeeds AND the assignee column sticks.
        assert!(handle_move_typed(move_req(&id, "in_progress", "Mini", false), &mut store).is_ok());
        assert_eq!(assignee_of(&store, &id).as_deref(), Some("Mini"));

        // DGX1's claim is rejected with CLAIM_CONFLICT — and the owner is unchanged.
        let err = handle_move_typed(move_req(&id, "in_progress", "DGX1", false), &mut store)
            .expect_err("cross-agent claim must be rejected");
        assert!(
            String::from_utf8_lossy(&err).contains("CLAIM_CONFLICT"),
            "expected CLAIM_CONFLICT, got {}",
            String::from_utf8_lossy(&err)
        );
        assert_eq!(
            assignee_of(&store, &id).as_deref(),
            Some("Mini"),
            "owner unchanged"
        );

        // --force is the audited handoff escape hatch.
        assert!(handle_move_typed(move_req(&id, "in_progress", "DGX1", true), &mut store).is_ok());
        assert_eq!(assignee_of(&store, &id).as_deref(), Some("DGX1"));
    }
}

#[cfg(test)]
mod ch6443_export_request_tests {
    //! `arrow-kanban export --format json` (board-wide) sent `id: null`, which the old
    //! non-optional `ExportRequest { id: String }` rejected with
    //! "invalid type: null, expected a string". These pin the deserialization fix.
    use super::ExportRequest;

    #[test]
    fn board_export_payload_with_null_id_deserializes() {
        // The EXACT payload the client sends for `export --format json --board development`
        // (main.rs dispatch: id=None → null, the other filters None → null).
        let payload =
            br#"{"id":null,"format":"json","board":"development","item_type":null,"status":null}"#;
        let req: ExportRequest =
            serde_json::from_slice(payload).expect("board-export payload must deserialize");
        assert!(req.id.is_none(), "board export has no id");
        assert_eq!(req.format.as_deref(), Some("json"));
        assert_eq!(req.board.as_deref(), Some("development"));
    }

    #[test]
    fn single_item_export_payload_still_deserializes() {
        let payload =
            br#"{"id":"CH-6443","format":"json","board":null,"item_type":null,"status":null}"#;
        let req: ExportRequest =
            serde_json::from_slice(payload).expect("single-item payload must deserialize");
        assert_eq!(req.id.as_deref(), Some("CH-6443"));
        assert_eq!(req.format.as_deref(), Some("json"));
    }

    #[test]
    fn missing_optional_fields_default_to_none() {
        // A minimal payload (e.g. a bare board export) must not require every field.
        let req: ExportRequest = serde_json::from_slice(br#"{}"#).expect("empty payload defaults");
        assert!(req.id.is_none() && req.format.is_none() && req.board.is_none());
    }
}

#[cfg(test)]
mod ch6555_pagination_tests {
    //! `slice_batches` must window `[offset, offset+limit)` correctly ACROSS
    //! `RecordBatch` boundaries — the mechanism that keeps a board-wide export page under
    //! the NATS max_payload (the full ~4577-item reply exceeds it and never arrives).
    use super::slice_batches;
    use arrow::array::{Array, ArrayRef, RecordBatch, StringArray};
    use std::sync::Arc;

    /// A one-column batch of `n` rows (`row-0`, `row-1`, …). `slice_batches` is schema-agnostic
    /// (it only reads `num_rows()` + `slice`), so a single column is a faithful stand-in.
    fn batch(prefix: &str, n: usize) -> RecordBatch {
        let vals: Vec<String> = (0..n).map(|i| format!("{prefix}-{i}")).collect();
        let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
        let col = Arc::new(StringArray::from(refs)) as ArrayRef;
        RecordBatch::try_new(
            Arc::new(arrow::datatypes::Schema::new(vec![
                arrow::datatypes::Field::new("id", arrow::datatypes::DataType::Utf8, false),
            ])),
            vec![col],
        )
        .expect("batch")
    }

    fn rows(batches: &[RecordBatch]) -> Vec<String> {
        let mut out = Vec::new();
        for b in batches {
            let ids = b
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("ids");
            for i in 0..b.num_rows() {
                out.push(ids.value(i).to_string());
            }
        }
        out
    }

    #[test]
    fn window_spans_batch_boundaries() {
        // Three batches: a-0,a-1 | b-0,b-1,b-2 | c-0,c-1  (7 rows total).
        let bs = vec![batch("a", 2), batch("b", 3), batch("c", 2)];
        // offset 1, limit 4 → global rows 1..5 = a-1, b-0, b-1, b-2 (spans batch 0 tail + all of 1).
        assert_eq!(
            rows(&slice_batches(&bs, 1, 4)),
            vec!["a-1", "b-0", "b-1", "b-2"]
        );
        // first page
        assert_eq!(rows(&slice_batches(&bs, 0, 3)), vec!["a-0", "a-1", "b-0"]);
        // last partial page: offset 6, limit 5 → only row 6 (c-1), clamped to the end.
        assert_eq!(rows(&slice_batches(&bs, 6, 5)), vec!["c-1"]);
    }

    #[test]
    fn out_of_range_and_exact_end() {
        let bs = vec![batch("a", 2), batch("b", 2)]; // 4 rows
        // offset past the end → empty (not a panic).
        assert!(slice_batches(&bs, 10, 5).is_empty());
        // a full-cover page returns every row, in order.
        assert_eq!(
            rows(&slice_batches(&bs, 0, 100)),
            vec!["a-0", "a-1", "b-0", "b-1"]
        );
        // a page landing exactly on a boundary.
        assert_eq!(rows(&slice_batches(&bs, 2, 2)), vec!["b-0", "b-1"]);
    }
}

#[cfg(test)]
mod ch6742_tests {
    //! `arrow-kanban update` dependency-edit defects, at the server handler.
    use super::*;
    use arrow_kanban::crud::{CreateItemInput, KanbanStore};
    use arrow_kanban::item_type::ItemType;
    use arrow_kanban::schema::items_col;

    fn item_with_flat_depends(store: &mut KanbanStore, deps: &[&str]) -> String {
        store
            .create_item(&CreateItemInput {
                title: "t".into(),
                item_type: ItemType::Expedition,
                priority: None,
                assignee: None,
                tags: vec![],
                related: vec![],
                depends_on: deps.iter().map(|s| s.to_string()).collect(),
                body: None,
            })
            .expect("create")
    }

    #[test]
    fn unrelate_removes_a_flat_only_dependency() {
        // DEFECT 2: an edge existing ONLY in the flat depends_on column (written by
        // --depends-on, no typed relation) must be removable via --unrelate. Before the fix
        // the flat removal was gated behind remove_relation() succeeding, so a flat-only edge
        // was unremovable and --unrelate reported "relationships_removed": [] (a silent no-op).
        let mut store = KanbanStore::new();
        let mut relations = RelationsStore::new();
        let id = item_with_flat_depends(&mut store, &["EX-1", "EX-2"]);

        let payload = serde_json::to_vec(&serde_json::json!({
            "id": id,
            "unrelate": ["dependsOn:EX-1"],
        }))
        .unwrap();
        let req: UpdateRequest = serde_json::from_slice(&payload).unwrap();
        let resp = handle_update_typed(req, &mut store, &mut relations)
            .expect("update ok")
            .into_bytes();
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();

        let deps = store.get_list_field(&id, items_col::DEPENDS_ON).unwrap();
        assert_eq!(
            deps,
            vec!["EX-2".to_string()],
            "flat-only edge must be removed, got {deps:?}"
        );
        let removed = v["relationships_removed"].as_array().unwrap();
        assert!(
            removed.iter().any(|r| r == "dependsOn:EX-1"),
            "flat-only removal must be reported, got {removed:?}"
        );
    }

    #[test]
    fn blocked_ignores_empty_string_dependency_matching_ready() {
        // DEFECT 3: an item whose only dep is the empty string (legacy [""] corruption from
        // defect 1) must NOT be reported blocked — critical_path's --ready already treats "" as
        // satisfied, so counting it here made the SAME item read both blocked and ready.
        let mut store = KanbanStore::new();
        let empty_dep = item_with_flat_depends(&mut store, &[""]); // legacy corruption
        let not_done_target = item_with_flat_depends(&mut store, &[]); // an active (not-done) item
        let really_blocked = item_with_flat_depends(&mut store, &[not_done_target.as_str()]);

        let resp = handle_blocked_typed(&store)
            .expect("blocked ok")
            .into_bytes();
        let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        let table = v["table"].as_str().unwrap();

        assert!(
            !table.contains(&empty_dep),
            "an empty-string-only dep must NOT be blocked; table=\n{table}"
        );
        assert!(
            table.contains(&really_blocked),
            "a real unmet dep MUST still be blocked; table=\n{table}"
        );
    }
}
