// SPDX-License-Identifier: MIT
//! Event broadcasting for mutations.
//!
//! After every mutation (create, move, delete), the server publishes an event
//! to `kanban.event.{type}`. Consumers like Command Deck can subscribe for
//! real-time updates.

use serde::{Deserialize, Serialize};

/// Event published after an item is created.
#[derive(Debug, Serialize)]
pub struct ItemCreated {
    pub id: String,
    pub title: String,
    pub item_type: String,
    pub board: String,
    pub agent: Option<String>,
}

/// Event published after an item is moved to a new status.
#[derive(Debug, Serialize)]
pub struct ItemMoved {
    pub id: String,
    pub from: String,
    pub to: String,
    pub agent: Option<String>,
}

/// Event published after an item is deleted.
#[derive(Debug, Serialize)]
pub struct ItemDeleted {
    pub id: String,
}

/// Periodic stats broadcast.
#[derive(Debug, Serialize)]
pub struct StatsSnapshot {
    pub total_items: usize,
    pub active_items: usize,
    pub by_status: Vec<(String, usize)>,
    pub timestamp: String,
}

/// Subject names for events.
pub mod subjects {
    pub const CREATED: &str = "kanban.event.created";
    pub const MOVED: &str = "kanban.event.moved";
    pub const DELETED: &str = "kanban.event.deleted";
    pub const STATS: &str = "kanban.event.stats";
}

/// Serialize an event to JSON bytes for NATS publishing.
pub fn to_event_bytes<T: Serialize>(event: &T) -> Vec<u8> {
    serde_json::to_vec(event).unwrap_or_else(|_| b"{}".to_vec())
}

/// Detect mutation events from a dispatch response.
///
/// Given a command name and the JSON response bytes, returns
/// `Some((event_type_suffix, event_bytes))` if the command was a mutation
/// that succeeded. Used by NatsServiceBuilder's mutation callback.
pub fn detect_mutation(command: &str, response: &[u8]) -> Option<(String, Vec<u8>)> {
    let resp: serde_json::Value = serde_json::from_slice(response).ok()?;
    if resp.get("error").is_some() {
        return None;
    }

    match command {
        "create" | "hdd.paper" | "hdd.hypothesis" | "hdd.experiment" | "hdd.measure"
        | "hdd.idea" | "hdd.literature" => {
            let item_type_str = resp
                .get("item_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let board = arrow_kanban::ItemType::from_str_loose(item_type_str)
                .map(|t| t.board().to_string())
                .unwrap_or_else(|| "development".to_string());
            Some((
                "created".to_string(),
                to_event_bytes(&ItemCreated {
                    id: resp
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    title: resp
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    item_type: item_type_str.to_string(),
                    board,
                    agent: None,
                }),
            ))
        }
        "move" => Some((
            "moved".to_string(),
            to_event_bytes(&ItemMoved {
                id: resp
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                from: resp
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                to: resp
                    .get("to")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                agent: None,
            }),
        )),
        "delete" => Some((
            "deleted".to_string(),
            to_event_bytes(&ItemDeleted {
                id: resp
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }),
        )),
        _ => None,
    }
}

// ── Typed mutation → event derivation (E3 PR-2) ──────────────────────────────
//
// `detect_mutation` above re-parses the response JSON and covers only
// create/move/delete — the other seven mutating commands
// (ratify/update/comment/rank/relation.add + the two run commands) emitted
// nothing. The typed derivation below is EXHAUSTIVE over the command set: the
// `match` in [`mutation_event`] is wildcard-free, so a new mutating command
// cannot be added without the compiler forcing it to declare its event (exactly
// like `KanbanCommand::is_mutation` / `KanbanEngine::route`). The resulting
// [`MutationEvent`] is what the commit log stores and the outbox relay projects.

use crate::engine::{KanbanCommand, KanbanReply};

/// The event-contract version stamped on a published envelope. The three legacy
/// events keep the frozen v1.0 shape (existing `KANBAN_EVENTS` consumers parse
/// them unchanged); every event the outbox adds ships v1.1, additively.
pub const CONTRACT_V1_0: &str = "1.0";
/// The additive contract version for the events completed by E3 PR-2.
pub const CONTRACT_V1_1: &str = "1.1";
/// The contract version stamped on extension events (E3 3c-i) — a distinct family
/// from the core v1.x line, so an extension's event schema versions independently.
pub const CONTRACT_EXT_1: &str = "ext.1";

/// A typed, committed mutation event. Carries enough to (a) project the outbox
/// notification and (b) replay the write onto a checkpoint during recovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum MutationEvent {
    Created(CreatedEvent),
    Moved(MovedEvent),
    Deleted(DeletedEvent),
    Ratified(RatifiedEvent),
    Updated(UpdatedEvent),
    Commented(CommentedEvent),
    Ranked(RankedEvent),
    RelationAdded(RelationAddedEvent),
    #[cfg(feature = "research")]
    RunStarted(RunEvent),
    #[cfg(feature = "research")]
    RunCompleted(RunEvent),
    /// An extension-derived event (E3 3c-i). Internally-tagged like its siblings
    /// (`"kind":"Extension"`); the extension's OWN event name travels in the
    /// `ext_kind` field — renamed off the serde tag key `kind` so the tag and the
    /// field never collide. Adding this variant leaves every existing variant's
    /// bytes untouched (the legacy-projection + log round-trip tests stay green).
    Extension {
        namespace: String,
        #[serde(rename = "ext_kind")]
        kind: String,
        payload: serde_json::Value,
    },
}

/// Replay-complete record of a create (either `create` or an `hdd.*` create).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatedEvent {
    pub id: String,
    pub title: String,
    pub item_type: String,
    pub board: String,
    pub priority: Option<String>,
    pub assignee: Option<String>,
    pub tags: Vec<String>,
    pub related: Vec<String>,
    pub depends_on: Vec<String>,
    pub body: Option<String>,
    /// Typed edges recorded at create time, as `predicate:TARGET`.
    pub relationships: Vec<String>,
}

/// Replay-complete record of a status move.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MovedEvent {
    pub id: String,
    pub from: String,
    pub to: String,
    pub assignee: Option<String>,
    pub force: bool,
    pub reason: Option<String>,
    pub resolution: Option<String>,
    pub closed_by: Option<String>,
}

/// Replay-complete record of a delete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeletedEvent {
    pub id: String,
}

/// Replay-complete record of a phase ratification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatifiedEvent {
    pub phase_tag: String,
    pub stripped: Vec<String>,
    pub voyages_started: Vec<String>,
    pub remaining_pending: u64,
}

/// Replay-complete record of a field/relationship update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdatedEvent {
    pub id: String,
    pub title: Option<String>,
    pub priority: Option<String>,
    pub assignee: Option<String>,
    pub tags: Option<Vec<String>>,
    pub body: Option<String>,
    pub related: Option<Vec<String>>,
    pub depends_on: Option<Vec<String>>,
    pub relate: Vec<String>,
    pub unrelate: Vec<String>,
    /// The reply's summary of what changed (for the outbox notification).
    pub updated: Vec<String>,
    pub relationships_added: Vec<String>,
    pub relationships_removed: Vec<String>,
}

/// Replay-complete record of a comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentedEvent {
    pub id: String,
    pub text: String,
    pub agent: Option<String>,
}

/// Replay-complete record of a rank set/clear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedEvent {
    pub id: String,
    pub rank: Option<i32>,
}

/// Replay-complete record of a typed-relation add.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationAddedEvent {
    pub source: String,
    pub target: String,
    pub predicate: String,
}

/// Notification record of an experiment-run start/complete. The run store is
/// persisted by the handler itself (outside the item checkpoint), so this event
/// carries no item-store replay — it is a durable, re-publishable notification.
#[cfg(feature = "research")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub experiment_id: String,
    pub run_id: Option<String>,
    pub run: Option<u32>,
    pub status: String,
}

// ── serde-Value field pluckers (read the typed request / reply as JSON) ──────
fn ostr(v: &serde_json::Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string)
}
fn vstr(v: &serde_json::Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
fn ovstr(v: &serde_json::Value, k: &str) -> Option<Vec<String>> {
    v.get(k).filter(|x| x.is_array()).map(|x| {
        x.as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.as_str().map(str::to_string))
            .collect()
    })
}

/// Derive the typed mutation event from a decoded command and its successful
/// reply. Returns `None` for every non-mutating command; the `match` is
/// wildcard-free so a new mutating command must declare its event here.
///
/// Reply-derived fields (the allocated id, the prior status, the minted
/// relation id) come from the reply's JSON — the tested wire contract — while
/// the command supplies the inputs a checkpoint replay needs.
pub fn mutation_event(cmd: &KanbanCommand, reply: &KanbanReply) -> Option<MutationEvent> {
    use KanbanCommand as C;
    let rv = reply.to_value();
    match cmd {
        C::Create(req) => {
            let q = serde_json::to_value(req).ok()?;
            let item_type = ostr(&rv, "item_type").or_else(|| ostr(&q, "item_type"))?;
            let board = arrow_kanban::ItemType::from_str_loose(&item_type)
                .map(|t| t.board().to_string())
                .unwrap_or_else(|| "development".to_string());
            Some(MutationEvent::Created(CreatedEvent {
                id: ostr(&rv, "id")?,
                title: ostr(&rv, "title")
                    .or_else(|| ostr(&q, "title"))
                    .unwrap_or_default(),
                item_type,
                board,
                priority: ostr(&q, "priority"),
                assignee: ostr(&q, "assignee"),
                tags: vstr(&q, "tags"),
                related: vstr(&q, "related"),
                depends_on: vstr(&q, "depends_on"),
                body: ostr(&q, "body"),
                relationships: vstr(&rv, "relationships"),
            }))
        }
        C::HddCreate(item_type, req) => {
            let q = serde_json::to_value(req).ok()?;
            Some(MutationEvent::Created(CreatedEvent {
                id: ostr(&rv, "id")?,
                title: ostr(&rv, "title")
                    .or_else(|| ostr(&q, "title"))
                    .unwrap_or_default(),
                item_type: item_type.as_str().to_string(),
                board: item_type.board().to_string(),
                priority: None,
                assignee: None,
                tags: vstr(&q, "tags"),
                related: vstr(&q, "related"),
                depends_on: Vec::new(),
                body: ostr(&q, "body"),
                relationships: Vec::new(),
            }))
        }
        C::Move(req) => {
            let q = serde_json::to_value(req).ok()?;
            Some(MutationEvent::Moved(MovedEvent {
                id: ostr(&rv, "id").or_else(|| ostr(&q, "id"))?,
                from: ostr(&rv, "from").unwrap_or_default(),
                to: ostr(&rv, "to")
                    .or_else(|| ostr(&q, "status"))
                    .unwrap_or_default(),
                assignee: ostr(&q, "assignee"),
                force: q.get("force").and_then(|x| x.as_bool()).unwrap_or(false),
                reason: ostr(&q, "reason"),
                resolution: ostr(&q, "resolution"),
                closed_by: ostr(&q, "closed_by"),
            }))
        }
        C::Delete(req) => {
            let q = serde_json::to_value(req).ok()?;
            Some(MutationEvent::Deleted(DeletedEvent {
                id: ostr(&rv, "id").or_else(|| ostr(&q, "id"))?,
            }))
        }
        C::Ratify(req) => {
            let q = serde_json::to_value(req).ok()?;
            Some(MutationEvent::Ratified(RatifiedEvent {
                phase_tag: ostr(&q, "phase_tag").or_else(|| ostr(&rv, "phase_tag"))?,
                stripped: vstr(&rv, "stripped"),
                voyages_started: vstr(&rv, "voyages_started"),
                remaining_pending: rv
                    .get("remaining_pending")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
            }))
        }
        C::Update(req) => {
            let q = serde_json::to_value(req).ok()?;
            Some(MutationEvent::Updated(UpdatedEvent {
                id: ostr(&q, "id").or_else(|| ostr(&rv, "id"))?,
                title: ostr(&q, "title"),
                priority: ostr(&q, "priority"),
                assignee: ostr(&q, "assignee"),
                tags: ovstr(&q, "tags"),
                body: ostr(&q, "body"),
                related: ovstr(&q, "related"),
                depends_on: ovstr(&q, "depends_on"),
                relate: vstr(&q, "relate"),
                unrelate: vstr(&q, "unrelate"),
                updated: vstr(&rv, "updated"),
                relationships_added: vstr(&rv, "relationships_added"),
                relationships_removed: vstr(&rv, "relationships_removed"),
            }))
        }
        C::Comment(req) => {
            let q = serde_json::to_value(req).ok()?;
            Some(MutationEvent::Commented(CommentedEvent {
                id: ostr(&q, "id").or_else(|| ostr(&rv, "id"))?,
                text: ostr(&q, "text").unwrap_or_default(),
                agent: ostr(&q, "agent"),
            }))
        }
        C::Rank(req) => {
            let q = serde_json::to_value(req).ok()?;
            Some(MutationEvent::Ranked(RankedEvent {
                id: ostr(&q, "id").or_else(|| ostr(&rv, "id"))?,
                rank: q.get("rank").and_then(|x| x.as_i64()).map(|n| n as i32),
            }))
        }
        C::RelationAdd(req) => {
            let q = serde_json::to_value(req).ok()?;
            Some(MutationEvent::RelationAdded(RelationAddedEvent {
                source: ostr(&q, "source_id").or_else(|| ostr(&rv, "source"))?,
                target: ostr(&q, "target_id").or_else(|| ostr(&rv, "target"))?,
                predicate: ostr(&q, "predicate").or_else(|| ostr(&rv, "predicate"))?,
            }))
        }
        #[cfg(feature = "research")]
        C::HddRun(_) => Some(MutationEvent::RunStarted(RunEvent {
            experiment_id: ostr(&rv, "experiment_id")?,
            run_id: ostr(&rv, "run_id"),
            run: None,
            status: ostr(&rv, "status").unwrap_or_else(|| "running".to_string()),
        })),
        #[cfg(feature = "research")]
        C::HddRunComplete(_) => Some(MutationEvent::RunCompleted(RunEvent {
            experiment_id: ostr(&rv, "experiment_id")?,
            run_id: None,
            run: rv.get("run").and_then(|x| x.as_u64()).map(|n| n as u32),
            status: ostr(&rv, "status").unwrap_or_else(|| "complete".to_string()),
        })),

        // An extension command derives its event through the extension itself
        // (`EngineExtension::derive_event`), never through the core derivation —
        // so the core mutation_event returns None and the engine routes around it.
        C::Extension(_) => None,

        // ── Non-mutating commands emit no event (wildcard-free by design). ──
        C::List(_)
        | C::Show(_)
        | C::Query
        | C::Board(_)
        | C::Stats(_)
        | C::Validate
        | C::Export(_)
        | C::Roadmap(_)
        | C::CriticalPath
        | C::PipelineHealth
        | C::Worklist(_)
        | C::NextId(_)
        | C::History(_)
        | C::Blocked
        | C::Templates(_)
        | C::HddValidate
        | C::HddRegistry
        | C::RelationQuery(_)
        | C::GitStubAck(_)
        | C::GitStubDetail(_) => None,
        #[cfg(feature = "research")]
        C::HddRunStatus(_) => None,
        #[cfg(feature = "git")]
        C::SourcePush(_) | C::SourcePull(_) | C::SourceBranches | C::SourceDelete(_) => None,
    }
}

impl MutationEvent {
    /// The `kanban.event.{suffix}` subject the event publishes to.
    pub fn subject_suffix(&self) -> &'static str {
        match self {
            MutationEvent::Created(_) => "created",
            MutationEvent::Moved(_) => "moved",
            MutationEvent::Deleted(_) => "deleted",
            MutationEvent::Ratified(_) => "ratified",
            MutationEvent::Updated(_) => "updated",
            MutationEvent::Commented(_) => "commented",
            MutationEvent::Ranked(_) => "ranked",
            MutationEvent::RelationAdded(_) => "relation_added",
            #[cfg(feature = "research")]
            MutationEvent::RunStarted(_) => "run_started",
            #[cfg(feature = "research")]
            MutationEvent::RunCompleted(_) => "run_completed",
            // Frozen &'static, unused for extension events — the routing subject
            // is `event_subject()` / `published_subject()` (they carry namespace+kind).
            MutationEvent::Extension { .. } => "ext",
        }
    }

    /// The event-contract version: the three legacy events stay frozen at v1.0;
    /// the outbox-completed events ship v1.1; extension events ship the `ext.1`
    /// family (distinct, so an extension versions its schema independently).
    pub fn contract_version(&self) -> &'static str {
        match self {
            MutationEvent::Created(_) | MutationEvent::Moved(_) | MutationEvent::Deleted(_) => {
                CONTRACT_V1_0
            }
            MutationEvent::Extension { .. } => CONTRACT_EXT_1,
            _ => CONTRACT_V1_1,
        }
    }

    /// True for an extension-derived event — published on the SEPARATE
    /// `kanban.ext.event.>` subject tree, never on the frozen `kanban.event.>`.
    pub fn is_extension(&self) -> bool {
        matches!(self, MutationEvent::Extension { .. })
    }

    /// The projected logical subject of this event. A core event keeps its frozen
    /// [`subject_suffix`](Self::subject_suffix) (e.g. `created`); an extension
    /// event projects to `ext.<namespace>.<kind>` (e.g. `ext.demo.propose`).
    pub fn event_subject(&self) -> String {
        match self {
            MutationEvent::Extension {
                namespace, kind, ..
            } => format!("ext.{namespace}.{kind}"),
            other => other.subject_suffix().to_string(),
        }
    }

    /// The FULL NATS subject the outbox publishes this event to, given the core
    /// event prefix (`kanban.event`) and the extension event prefix
    /// (`kanban.ext.event`). A core event stays on `<core_prefix>.<suffix>` —
    /// BYTE-IDENTICAL to the former construction — so the frozen `KANBAN_EVENTS`
    /// stream (which filters `kanban.event.>`) is unchanged. An extension event
    /// goes on `<ext_prefix>.<namespace>.<kind>`, a distinct tree the frozen
    /// stream never captures.
    pub fn published_subject(&self, core_prefix: &str, ext_prefix: &str) -> String {
        match self {
            MutationEvent::Extension {
                namespace, kind, ..
            } => format!("{ext_prefix}.{namespace}.{kind}"),
            other => format!("{core_prefix}.{}", other.subject_suffix()),
        }
    }

    /// The outbox notification payload. For the three legacy events this is the
    /// EXACT frozen `ItemCreated`/`ItemMoved`/`ItemDeleted` JSON existing
    /// consumers parse; the v1.1 events carry a natural summary.
    pub fn outbox_payload(&self) -> serde_json::Value {
        match self {
            MutationEvent::Created(e) => serde_json::to_value(ItemCreated {
                id: e.id.clone(),
                title: e.title.clone(),
                item_type: e.item_type.clone(),
                board: e.board.clone(),
                agent: None,
            })
            .unwrap_or(serde_json::Value::Null),
            MutationEvent::Moved(e) => serde_json::to_value(ItemMoved {
                id: e.id.clone(),
                from: e.from.clone(),
                to: e.to.clone(),
                agent: None,
            })
            .unwrap_or(serde_json::Value::Null),
            MutationEvent::Deleted(e) => serde_json::to_value(ItemDeleted { id: e.id.clone() })
                .unwrap_or(serde_json::Value::Null),
            MutationEvent::Ratified(e) => serde_json::json!({
                "phase_tag": e.phase_tag,
                "stripped": e.stripped,
                "voyages_started": e.voyages_started,
                "remaining_pending": e.remaining_pending,
            }),
            MutationEvent::Updated(e) => serde_json::json!({
                "id": e.id,
                "updated": e.updated,
                "relationships_added": e.relationships_added,
                "relationships_removed": e.relationships_removed,
            }),
            MutationEvent::Commented(e) => serde_json::json!({
                "id": e.id, "comment": e.text, "agent": e.agent,
            }),
            MutationEvent::Ranked(e) => serde_json::json!({ "id": e.id, "rank": e.rank }),
            MutationEvent::RelationAdded(e) => serde_json::json!({
                "source": e.source, "target": e.target, "predicate": e.predicate,
            }),
            #[cfg(feature = "research")]
            MutationEvent::RunStarted(e) => serde_json::json!({
                "experiment_id": e.experiment_id, "run_id": e.run_id, "status": e.status,
            }),
            #[cfg(feature = "research")]
            MutationEvent::RunCompleted(e) => serde_json::json!({
                "experiment_id": e.experiment_id, "run": e.run, "status": e.status,
            }),
            // The extension's own reply is the notification payload, verbatim.
            MutationEvent::Extension { payload, .. } => payload.clone(),
        }
    }

    /// Replay this committed event onto a loaded checkpoint, reconstructing the
    /// write. Called only for the log tail a torn checkpoint did not capture;
    /// uses the store's id-addressable API so the rebuild is faithful (`create`
    /// re-inserts under its ORIGINAL id via `create_item_with_id`).
    pub fn replay_into(
        &self,
        store: &mut arrow_kanban::crud::KanbanStore,
        relations: &mut arrow_kanban::relations::RelationsStore,
    ) -> Result<(), String> {
        use arrow_kanban::crud::CreateItemInput;
        match self {
            MutationEvent::Created(e) => {
                // Idempotent: the checkpoint marker can trail the checkpoint
                // parquet by one commit (they are two non-atomic files), so a
                // torn-checkpoint replay may re-see an already-present create.
                // Re-inserting would be a DuplicateId error; skip instead.
                if store.get_item(&e.id).is_ok() {
                    return Ok(());
                }
                let item_type = arrow_kanban::ItemType::from_str_loose(&e.item_type)
                    .ok_or_else(|| format!("replay create: unknown item type {}", e.item_type))?;
                // Re-merge the flat projection of related/dependsOn edges, exactly
                // as the create handler does, so the rebuilt row matches.
                let mut related = e.related.clone();
                let mut depends_on = e.depends_on.clone();
                let edges = parse_relationship_specs(&e.relationships);
                for (pred, target) in &edges {
                    match arrow_kanban::relation_vocab::flat_column_for(pred) {
                        Some("related") if !related.contains(target) => {
                            related.push(target.clone())
                        }
                        Some("depends_on") if !depends_on.contains(target) => {
                            depends_on.push(target.clone())
                        }
                        _ => {}
                    }
                }
                store
                    .create_item_with_id(
                        &e.id,
                        &CreateItemInput {
                            title: e.title.clone(),
                            item_type,
                            priority: e.priority.clone(),
                            assignee: e.assignee.clone(),
                            tags: e.tags.clone(),
                            related,
                            depends_on,
                            body: e.body.clone(),
                        },
                    )
                    .map_err(|err| format!("replay create {}: {err}", e.id))?;
                for (pred, target) in &edges {
                    let _ = relations.add_relation(&e.id, target, pred);
                }
                Ok(())
            }
            MutationEvent::Moved(e) => {
                store
                    .update_status(
                        &e.id,
                        &e.to,
                        e.assignee.as_deref(),
                        e.force,
                        e.reason.as_deref(),
                    )
                    .map_err(|err| format!("replay move {}: {err}", e.id))?;
                if let Some(a) = e.assignee.as_deref() {
                    let _ = store.update_assignee(&e.id, Some(a));
                }
                if let Some(r) = e.resolution.as_deref() {
                    let _ = store.update_resolution(&e.id, Some(r));
                }
                if let Some(cb) = e.closed_by.as_deref() {
                    let _ = store.update_closed_by(&e.id, Some(cb));
                }
                Ok(())
            }
            MutationEvent::Deleted(e) => {
                // Idempotent for the same marker-trails-checkpoint window: if the
                // item is already gone, the delete is already reflected.
                if store.get_item(&e.id).is_err() {
                    return Ok(());
                }
                store
                    .delete_item(&e.id)
                    .map_err(|err| format!("replay delete {}: {err}", e.id))
            }
            MutationEvent::Ratified(e) => store
                .ratify_phase(&e.phase_tag)
                .map(|_| ())
                .map_err(|err| format!("replay ratify {}: {err}", e.phase_tag)),
            MutationEvent::Updated(e) => {
                if let Some(t) = &e.title {
                    let _ = store.update_title(&e.id, t);
                }
                if let Some(p) = &e.priority {
                    let _ = store.update_priority(&e.id, Some(p));
                }
                if let Some(a) = &e.assignee {
                    let _ = store.update_assignee(&e.id, Some(a));
                }
                if let Some(tags) = &e.tags {
                    let _ = store.update_tags(&e.id, tags);
                }
                if let Some(b) = &e.body {
                    let _ = store.update_body(&e.id, Some(b));
                }
                if let Some(r) = &e.related {
                    let _ = store.update_related(&e.id, r);
                }
                if let Some(d) = &e.depends_on {
                    let _ = store.update_depends_on(&e.id, d);
                }
                for (pred, target) in parse_relationship_specs(&e.relate) {
                    // Idempotent: a trailing-checkpoint replay can re-see an
                    // already-added edge; `add_relation` appends unconditionally,
                    // so guard it (the flat projection dedups on its own).
                    if !relations.has_relation(&e.id, &target, &pred) {
                        let _ = relations.add_relation(&e.id, &target, &pred);
                    }
                    if let Some(col) = flat_col_for(&pred) {
                        let _ = store.add_to_list_field(&e.id, col, &target);
                    }
                }
                for (pred, target) in parse_relationship_specs(&e.unrelate) {
                    let _ = relations.remove_relation(&e.id, &target, &pred);
                    if let Some(col) = flat_col_for(&pred) {
                        let _ = store.remove_from_list_field(&e.id, col, &target);
                    }
                }
                Ok(())
            }
            MutationEvent::Commented(e) => {
                // Idempotent: a checkpoint marker/manifest that trails the parquet
                // by one commit re-presents an already-checkpointed comment, and
                // `add_comment` mints a fresh sequential id each call — so a naive
                // replay would duplicate it. `add_comment` resolves a missing agent
                // to "unknown"; match that so the dedup sees the stored author.
                let author = e.agent.as_deref().unwrap_or("unknown");
                if store.has_comment(&e.id, author, &e.text) {
                    return Ok(());
                }
                store
                    .add_comment(&e.id, &e.text, e.agent.as_deref())
                    .map(|_| ())
                    .map_err(|err| format!("replay comment {}: {err}", e.id))
            }
            MutationEvent::Ranked(e) => store
                .update_rank(&e.id, e.rank)
                .map_err(|err| format!("replay rank {}: {err}", e.id)),
            // Idempotent: rebuild the edge only if the trailing-checkpoint tail did
            // not already checkpoint it (`add_relation` appends unconditionally).
            MutationEvent::RelationAdded(e) => {
                if relations.has_relation(&e.source, &e.target, &e.predicate) {
                    return Ok(());
                }
                relations
                    .add_relation(&e.source, &e.target, &e.predicate)
                    .map(|_| ())
                    .map_err(|err| format!("replay relation.add {}->{}: {err}", e.source, e.target))
            }
            // The run store is persisted by the handler outside the item
            // checkpoint, so there is no item-store state to replay.
            #[cfg(feature = "research")]
            MutationEvent::RunStarted(_) | MutationEvent::RunCompleted(_) => Ok(()),
            // An extension event carries no CORE-store state — the core replay is
            // a no-op. The ENGINE will replay it onto the extension's own tables
            // (via `EngineExtension::replay`); that load-orchestration is 3c-ii, so
            // in 3c-i an ext event is durable-in-log but not yet restored on load.
            MutationEvent::Extension { .. } => Ok(()),
        }
    }
}

/// The flat item column a `related`/`dependsOn` predicate projects onto.
fn flat_col_for(predicate: &str) -> Option<usize> {
    use arrow_kanban::schema::items_col;
    match arrow_kanban::relation_vocab::flat_column_for(predicate) {
        Some("related") => Some(items_col::RELATED),
        Some("depends_on") => Some(items_col::DEPENDS_ON),
        _ => None,
    }
}

/// Parse `predicate:TARGET` specs, dropping any malformed entry (a replay must
/// not fail on a stray edge — the typed relations store is separately checkpointed).
fn parse_relationship_specs(specs: &[String]) -> Vec<(String, String)> {
    specs
        .iter()
        .filter_map(|s| {
            s.split_once(':')
                .map(|(p, t)| (p.to_string(), t.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_item_created_serialization() {
        let event = ItemCreated {
            id: "EXP-100".to_string(),
            title: "Test".to_string(),
            item_type: "expedition".to_string(),
            board: "development".to_string(),
            agent: Some("M5".to_string()),
        };
        let bytes = to_event_bytes(&event);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["id"], "EXP-100");
        assert_eq!(parsed["agent"], "M5");
    }

    #[test]
    fn test_item_moved_serialization() {
        let event = ItemMoved {
            id: "EXP-100".to_string(),
            from: "backlog".to_string(),
            to: "in_progress".to_string(),
            agent: None,
        };
        let bytes = to_event_bytes(&event);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["from"], "backlog");
        assert_eq!(parsed["to"], "in_progress");
        assert!(parsed["agent"].is_null());
    }

    #[test]
    fn test_stats_snapshot_serialization() {
        let event = StatsSnapshot {
            total_items: 42,
            active_items: 38,
            by_status: vec![("backlog".to_string(), 20), ("in_progress".to_string(), 10)],
            timestamp: "2026-03-14T20:00:00Z".to_string(),
        };
        let bytes = to_event_bytes(&event);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["total_items"], 42);
        assert_eq!(parsed["active_items"], 38);
    }

    // ── Mutation detection tests (Phase 4 — event emission via callback) ────

    #[test]
    fn test_detect_mutation_create_emits_created_event() {
        let response = serde_json::to_vec(&serde_json::json!({
            "id": "EX-3001",
            "title": "Test expedition",
            "item_type": "expedition",
        }))
        .unwrap();

        let result = detect_mutation("create", &response);
        assert!(result.is_some(), "create should emit an event");

        let (event_type, event_bytes) = result.unwrap();
        assert_eq!(event_type, "created");

        let parsed: serde_json::Value = serde_json::from_slice(&event_bytes).unwrap();
        assert_eq!(parsed["id"], "EX-3001");
        assert_eq!(parsed["title"], "Test expedition");
        assert_eq!(parsed["item_type"], "expedition");
        assert_eq!(parsed["board"], "development");
    }

    #[test]
    fn test_detect_mutation_move_emits_moved_event() {
        let response = serde_json::to_vec(&serde_json::json!({
            "id": "EX-3001",
            "from": "backlog",
            "to": "in_progress",
        }))
        .unwrap();

        let result = detect_mutation("move", &response);
        assert!(result.is_some(), "move should emit an event");

        let (event_type, event_bytes) = result.unwrap();
        assert_eq!(event_type, "moved");

        let parsed: serde_json::Value = serde_json::from_slice(&event_bytes).unwrap();
        assert_eq!(parsed["id"], "EX-3001");
        assert_eq!(parsed["from"], "backlog");
        assert_eq!(parsed["to"], "in_progress");
    }

    #[test]
    fn test_detect_mutation_delete_emits_deleted_event() {
        let response = serde_json::to_vec(&serde_json::json!({
            "id": "EX-3001",
        }))
        .unwrap();

        let result = detect_mutation("delete", &response);
        assert!(result.is_some());

        let (event_type, event_bytes) = result.unwrap();
        assert_eq!(event_type, "deleted");

        let parsed: serde_json::Value = serde_json::from_slice(&event_bytes).unwrap();
        assert_eq!(parsed["id"], "EX-3001");
    }

    #[test]
    fn test_detect_mutation_error_response_returns_none() {
        let response = serde_json::to_vec(&serde_json::json!({
            "error": "item not found",
            "code": "NOT_FOUND",
        }))
        .unwrap();

        assert!(detect_mutation("create", &response).is_none());
        assert!(detect_mutation("move", &response).is_none());
        assert!(detect_mutation("delete", &response).is_none());
    }

    #[test]
    fn test_detect_mutation_read_commands_return_none() {
        let response = serde_json::to_vec(&serde_json::json!({
            "items": [],
        }))
        .unwrap();

        assert!(detect_mutation("list", &response).is_none());
        assert!(detect_mutation("show", &response).is_none());
        assert!(detect_mutation("board", &response).is_none());
        assert!(detect_mutation("stats", &response).is_none());
        assert!(detect_mutation("query", &response).is_none());
    }

    // ── Typed MutationEvent: projection byte-compat, versioning, replay ──────

    fn created_ev() -> MutationEvent {
        MutationEvent::Created(CreatedEvent {
            id: "CH-9".into(),
            title: "T".into(),
            item_type: "chore".into(),
            board: "development".into(),
            priority: Some("high".into()),
            assignee: Some("A".into()),
            tags: vec!["x".into()],
            related: vec!["EX-1".into()],
            depends_on: vec![],
            body: Some("b".into()),
            relationships: vec![],
        })
    }

    /// The three legacy events project to the EXACT frozen v1.0 payload shapes
    /// (`ItemCreated`/`ItemMoved`/`ItemDeleted`), so existing consumers parse them
    /// unchanged. Assert the projection equals the legacy struct's JSON.
    #[test]
    fn legacy_projections_are_byte_identical_to_the_v1_0_structs() {
        let created = created_ev();
        assert_eq!(created.subject_suffix(), "created");
        assert_eq!(created.contract_version(), "1.0");
        assert_eq!(
            created.outbox_payload(),
            serde_json::to_value(ItemCreated {
                id: "CH-9".into(),
                title: "T".into(),
                item_type: "chore".into(),
                board: "development".into(),
                agent: None,
            })
            .unwrap(),
            "Created projects to the frozen ItemCreated shape (no priority/tags leak)"
        );

        let moved = MutationEvent::Moved(MovedEvent {
            id: "CH-9".into(),
            from: "backlog".into(),
            to: "in_progress".into(),
            assignee: Some("A".into()),
            force: false,
            reason: None,
            resolution: None,
            closed_by: None,
        });
        assert_eq!(moved.subject_suffix(), "moved");
        assert_eq!(moved.contract_version(), "1.0");
        assert_eq!(
            moved.outbox_payload(),
            serde_json::to_value(ItemMoved {
                id: "CH-9".into(),
                from: "backlog".into(),
                to: "in_progress".into(),
                agent: None,
            })
            .unwrap()
        );

        let deleted = MutationEvent::Deleted(DeletedEvent { id: "CH-9".into() });
        assert_eq!(deleted.subject_suffix(), "deleted");
        assert_eq!(deleted.contract_version(), "1.0");
        assert_eq!(
            deleted.outbox_payload(),
            serde_json::to_value(ItemDeleted { id: "CH-9".into() }).unwrap()
        );
    }

    /// Events added by the outbox ship the additive v1.1 contract.
    #[test]
    fn new_events_ship_the_v1_1_contract() {
        let commented = MutationEvent::Commented(CommentedEvent {
            id: "CH-9".into(),
            text: "hi".into(),
            agent: None,
        });
        assert_eq!(commented.subject_suffix(), "commented");
        assert_eq!(commented.contract_version(), "1.1");
        assert_eq!(commented.outbox_payload()["comment"], "hi");
    }

    /// The commit-log form round-trips (serialize → deserialize is identity), so
    /// a committed event reads back exactly for replay and re-publish.
    #[test]
    fn mutation_event_log_form_round_trips() {
        let ev = created_ev();
        let line = serde_json::to_string(&ev).expect("encode");
        let back: MutationEvent = serde_json::from_str(&line).expect("decode");
        assert_eq!(ev, back);
    }

    /// Replay reconstructs the write from the event: Created re-inserts under its
    /// ORIGINAL id with its fields; Moved changes status; Deleted removes it;
    /// and Created replay is idempotent (a re-seen create is a no-op, not a dup error).
    #[test]
    fn replay_reconstructs_and_is_idempotent() {
        use arrow_kanban::crud::KanbanStore;
        use arrow_kanban::relations::RelationsStore;
        let mut store = KanbanStore::new();
        let mut rels = RelationsStore::new();

        created_ev()
            .replay_into(&mut store, &mut rels)
            .expect("replay create");
        let item = store
            .get_item("CH-9")
            .expect("item present after create replay");
        assert_eq!(item.num_rows(), 1);

        // Idempotent: replaying the same create again is a no-op (no DuplicateId).
        created_ev()
            .replay_into(&mut store, &mut rels)
            .expect("idempotent create replay");

        MutationEvent::Moved(MovedEvent {
            id: "CH-9".into(),
            from: "backlog".into(),
            to: "in_progress".into(),
            assignee: None,
            force: false,
            reason: None,
            resolution: None,
            closed_by: None,
        })
        .replay_into(&mut store, &mut rels)
        .expect("replay move");

        MutationEvent::Deleted(DeletedEvent { id: "CH-9".into() })
            .replay_into(&mut store, &mut rels)
            .expect("replay delete");
        assert!(store.get_item("CH-9").is_err(), "delete replay removed it");

        // Idempotent delete: replaying delete on a gone item is a no-op.
        MutationEvent::Deleted(DeletedEvent { id: "CH-9".into() })
            .replay_into(&mut store, &mut rels)
            .expect("idempotent delete replay");
    }
}
