// SPDX-License-Identifier: MIT
//! The typed kanban engine — one semantics, JSON as a codec.
//!
//! [`KanbanEngine`] owns the stores and the write-durability gate. Every wire
//! command is *decoded* by [`parse`] into a typed [`KanbanCommand`], *applied*
//! by [`KanbanEngine::apply`], and *encoded* back to bytes by
//! [`KanbanReply::into_bytes`]. [`KanbanEngine::dispatch`] is exactly
//! `parse` → `apply` → `into_bytes`.
//!
//! This is a behavior-preserving refactor of the former string-dispatch that
//! lived in `handlers::dispatch`: the wire bytes are byte-identical. The
//! admission gate + persist-or-degrade flow is replicated
//! verbatim inside [`KanbanEngine::apply`].
//!
//! The command set is a *closed, exhaustive* enum: [`KanbanCommand::verb_str`],
//! [`KanbanCommand::is_mutation`], [`KanbanCommand::is_relation_mutation`] and
//! [`KanbanEngine::route`] all match without a wildcard, so a new command
//! cannot be added without the compiler forcing it to be routed, classified,
//! and named — which is what keeps the codec property test (over [`ALL_VERBS`])
//! exhaustive.

use crate::handlers::ErrorResponse;
use crate::handlers::core::{
    BoardRequest, CommentRequest, CreateRequest, CreateResponse, DeleteRequest, DeleteResponse,
    ExportRequest, HddCreateRequest, HistoryRequest, ListRequest, MoveRequest, MoveResponse,
    NextIdRequest, RankRequest, RatifyRequest, RoadmapRequest, ShowRequest, UpdateRequest,
    WorklistRequest,
};
use crate::handlers::relations::{RelationAddRequest, RelationQueryRequest};
use crate::health::HealthGate;
use arrow_kanban::crud::KanbanStore;
use arrow_kanban::item_type::ItemType;
use arrow_kanban::relations::RelationsStore;
use std::path::PathBuf;

/// A typed reply, encodable to the exact JSON bytes the former handlers wrote.
///
/// Each variant serializes with `serde_json::to_vec` — structs in field order,
/// `Value` in map order, `Error` in field order — which is byte-for-byte what
/// the old `serialize_response` / `error_response` produced.
#[derive(Debug)]
pub enum KanbanReply {
    Create(CreateResponse),
    Move(MoveResponse),
    Delete(DeleteResponse),
    Value(serde_json::Value),
    Error(ErrorResponse),
}

impl KanbanReply {
    /// The structured error reply, matching `error_response(msg, code)` byte-for-byte.
    pub fn error(msg: &str, code: &'static str) -> KanbanReply {
        KanbanReply::Error(ErrorResponse {
            error: msg.to_string(),
            code: code.to_string(),
        })
    }

    /// Wrap already-serialized error bytes (a handler's `error_response(...)?`
    /// arm) back into a typed reply. Every such byte string is a serialized
    /// [`ErrorResponse`], so the round-trip re-emits the identical bytes; the
    /// lossy fallback is unreachable for handler-produced errors and exists
    /// only so a stray non-conforming byte string cannot panic the engine.
    fn from_error_bytes(bytes: Vec<u8>) -> KanbanReply {
        match serde_json::from_slice::<ErrorResponse>(&bytes) {
            Ok(er) => KanbanReply::Error(er),
            Err(_) => KanbanReply::Error(ErrorResponse {
                error: String::from_utf8_lossy(&bytes).into_owned(),
                code: "INTERNAL".to_string(),
            }),
        }
    }

    /// Encode to wire bytes. Mirrors the old `serialize_response` fallback so an
    /// (unreachable) serialization failure yields the same INTERNAL error shape.
    pub fn into_bytes(self) -> Vec<u8> {
        fn enc<T: serde::Serialize>(value: &T) -> Vec<u8> {
            serde_json::to_vec(value).unwrap_or_else(|e| {
                serde_json::to_vec(&ErrorResponse {
                    error: format!("Serialization error: {e}"),
                    code: "INTERNAL".to_string(),
                })
                .unwrap_or_else(|_| {
                    br#"{"error":"serialization failed","code":"INTERNAL"}"#.to_vec()
                })
            })
        }
        match self {
            KanbanReply::Create(r) => enc(&r),
            KanbanReply::Move(r) => enc(&r),
            KanbanReply::Delete(r) => enc(&r),
            KanbanReply::Value(v) => enc(&v),
            KanbanReply::Error(e) => enc(&e),
        }
    }
}

/// One typed command per wire verb. The request payload is carried as its
/// decoded request struct where the handler used the standard
/// `parse_payload::<T>` path; verbs whose handlers parse leniently or by hand
/// (`stats`, `templates`, the `hdd.run.*` and `source.*` families) carry the
/// raw payload so their exact parsing — and thus their exact error bytes — is
/// preserved unchanged inside the handler.
pub enum KanbanCommand {
    // ── Core CRUD + analytics ──
    Create(CreateRequest),
    Move(MoveRequest),
    Ratify(RatifyRequest),
    Update(UpdateRequest),
    Comment(CommentRequest),
    Rank(RankRequest),
    List(ListRequest),
    Show(ShowRequest),
    /// `query` — unsupported in the open engine (cross-searches proposals).
    Query,
    Board(BoardRequest),
    Stats(Vec<u8>),
    Delete(DeleteRequest),
    Validate,
    Export(ExportRequest),
    Roadmap(RoadmapRequest),
    CriticalPath,
    /// `pipeline-health` — unsupported in the open engine (fleet analytics).
    PipelineHealth,
    Worklist(WorklistRequest),
    NextId(NextIdRequest),
    History(HistoryRequest),
    Blocked,
    Templates(Vec<u8>),
    // ── HDD create (verb selects the ItemType) ──
    HddCreate(ItemType, HddCreateRequest),
    HddValidate,
    HddRegistry,
    // ── Relations ──
    RelationAdd(RelationAddRequest),
    RelationQuery(RelationQueryRequest),
    // ── Git stubs (always present) ──
    /// `git.push` / `git.pull` / `git.clone` — carries the full verb.
    GitStubAck(&'static str),
    /// `git.log` / `git.blame` / `git.rebase` — carries the full verb.
    GitStubDetail(&'static str),
    // ── HDD experiment-run tracking (feature `research`) ──
    #[cfg(feature = "research")]
    HddRun(Vec<u8>),
    #[cfg(feature = "research")]
    HddRunStatus(Vec<u8>),
    #[cfg(feature = "research")]
    HddRunComplete(Vec<u8>),
    // ── Git-bundle source transport (feature `git`) ──
    #[cfg(feature = "git")]
    SourcePush(Vec<u8>),
    #[cfg(feature = "git")]
    SourcePull(Vec<u8>),
    #[cfg(feature = "git")]
    SourceBranches,
    #[cfg(feature = "git")]
    SourceDelete(Vec<u8>),
}

impl KanbanCommand {
    /// The wire verb this command decodes from / re-encodes to. Used verbatim in
    /// the persist-failure diagnostics, so it must match the former `command`
    /// string exactly (this is why `HddCreate` maps back through its `ItemType`).
    pub fn verb_str(&self) -> &'static str {
        use KanbanCommand as C;
        match self {
            C::Create(_) => "create",
            C::Move(_) => "move",
            C::Ratify(_) => "ratify",
            C::Update(_) => "update",
            C::Comment(_) => "comment",
            C::Rank(_) => "rank",
            C::List(_) => "list",
            C::Show(_) => "show",
            C::Query => "query",
            C::Board(_) => "board",
            C::Stats(_) => "stats",
            C::Delete(_) => "delete",
            C::Validate => "validate",
            C::Export(_) => "export",
            C::Roadmap(_) => "roadmap",
            C::CriticalPath => "critical-path",
            C::PipelineHealth => "pipeline-health",
            C::Worklist(_) => "worklist",
            C::NextId(_) => "next-id",
            C::History(_) => "history",
            C::Blocked => "blocked",
            C::Templates(_) => "templates",
            C::HddCreate(item_type, _) => match item_type {
                ItemType::Paper => "hdd.paper",
                ItemType::Hypothesis => "hdd.hypothesis",
                ItemType::Experiment => "hdd.experiment",
                ItemType::Measure => "hdd.measure",
                ItemType::Idea => "hdd.idea",
                ItemType::Literature => "hdd.literature",
                // Unreachable: `parse` only constructs HddCreate with the six above.
                _ => "hdd.create",
            },
            C::HddValidate => "hdd.validate",
            C::HddRegistry => "hdd.registry",
            C::RelationAdd(_) => "relation.add",
            C::RelationQuery(_) => "relation.query",
            C::GitStubAck(verb) => verb,
            C::GitStubDetail(verb) => verb,
            #[cfg(feature = "research")]
            C::HddRun(_) => "hdd.run",
            #[cfg(feature = "research")]
            C::HddRunStatus(_) => "hdd.run.status",
            #[cfg(feature = "research")]
            C::HddRunComplete(_) => "hdd.run.complete",
            #[cfg(feature = "git")]
            C::SourcePush(_) => "source.push",
            #[cfg(feature = "git")]
            C::SourcePull(_) => "source.pull",
            #[cfg(feature = "git")]
            C::SourceBranches => "source.branches",
            #[cfg(feature = "git")]
            C::SourceDelete(_) => "source.delete",
        }
    }

    /// Whether this command mutates persisted state. Mirrors the former
    /// `is_mutation` string list EXACTLY (the vestigial `pr.*` entries had no
    /// handler and are simply absent from the command set).
    pub fn is_mutation(&self) -> bool {
        use KanbanCommand as C;
        match self {
            C::Create(_)
            | C::Move(_)
            | C::Ratify(_)
            | C::Update(_)
            | C::Comment(_)
            | C::Rank(_)
            | C::Delete(_)
            | C::HddCreate(_, _)
            | C::RelationAdd(_) => true,
            #[cfg(feature = "research")]
            C::HddRun(_) | C::HddRunComplete(_) => true,
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
            | C::GitStubDetail(_) => false,
            #[cfg(feature = "research")]
            C::HddRunStatus(_) => false,
            #[cfg(feature = "git")]
            C::SourcePush(_) | C::SourcePull(_) | C::SourceBranches | C::SourceDelete(_) => false,
        }
    }

    /// Whether this command mutates the *relations* store (an extra persist).
    /// Mirrors the former `is_relation_mutation` — only `relation.add`.
    pub fn is_relation_mutation(&self) -> bool {
        matches!(self, KanbanCommand::RelationAdd(_))
    }
}

/// Every compiled wire verb, feature-gated to match the compiled command set.
/// The codec property test iterates this; the exhaustive matches above are what
/// force a new command to be handled, so a verb here can never be a dead string.
pub const ALL_VERBS: &[&str] = &[
    "create",
    "move",
    "ratify",
    "update",
    "comment",
    "rank",
    "list",
    "show",
    "query",
    "board",
    "stats",
    "delete",
    "validate",
    "export",
    "roadmap",
    "critical-path",
    "pipeline-health",
    "worklist",
    "next-id",
    "history",
    "blocked",
    "templates",
    "hdd.paper",
    "hdd.hypothesis",
    "hdd.experiment",
    "hdd.measure",
    "hdd.idea",
    "hdd.literature",
    "hdd.validate",
    "hdd.registry",
    "relation.add",
    "relation.query",
    "git.push",
    "git.pull",
    "git.clone",
    "git.log",
    "git.blame",
    "git.rebase",
    #[cfg(feature = "research")]
    "hdd.run",
    #[cfg(feature = "research")]
    "hdd.run.status",
    #[cfg(feature = "research")]
    "hdd.run.complete",
    #[cfg(feature = "git")]
    "source.push",
    #[cfg(feature = "git")]
    "source.pull",
    #[cfg(feature = "git")]
    "source.branches",
    #[cfg(feature = "git")]
    "source.delete",
];

/// Decode a wire verb + payload into a typed command.
///
/// - A regular verb is `parse_payload::<T>`'d into its variant; a decode error
///   is the existing `INVALID_PAYLOAD` reply, byte-for-byte (it reuses
///   `handlers::parse_payload`).
/// - Lenient / hand-parsed verbs carry the raw payload (their handler parses).
/// - An unknown verb is the existing `UNKNOWN_COMMAND` reply.
pub fn parse(verb: &str, payload: &[u8]) -> Result<KanbanCommand, KanbanReply> {
    use KanbanCommand as C;
    let cmd = match verb {
        "create" => C::Create(parse_req(payload)?),
        "move" => C::Move(parse_req(payload)?),
        "ratify" => C::Ratify(parse_req(payload)?),
        "update" => C::Update(parse_req(payload)?),
        "comment" => C::Comment(parse_req(payload)?),
        "rank" => C::Rank(parse_req(payload)?),
        "list" => C::List(parse_req(payload)?),
        "show" => C::Show(parse_req(payload)?),
        "query" => C::Query,
        "board" => C::Board(parse_req(payload)?),
        "stats" => C::Stats(payload.to_vec()),
        "delete" => C::Delete(parse_req(payload)?),
        "validate" => C::Validate,
        "export" => C::Export(parse_req(payload)?),
        "roadmap" => C::Roadmap(parse_req(payload)?),
        "critical-path" => C::CriticalPath,
        "pipeline-health" => C::PipelineHealth,
        "worklist" => C::Worklist(parse_req(payload)?),
        "next-id" => C::NextId(parse_req(payload)?),
        "history" => C::History(parse_req(payload)?),
        "blocked" => C::Blocked,
        "templates" => C::Templates(payload.to_vec()),
        "hdd.paper" => C::HddCreate(ItemType::Paper, parse_req(payload)?),
        "hdd.hypothesis" => C::HddCreate(ItemType::Hypothesis, parse_req(payload)?),
        "hdd.experiment" => C::HddCreate(ItemType::Experiment, parse_req(payload)?),
        "hdd.measure" => C::HddCreate(ItemType::Measure, parse_req(payload)?),
        "hdd.idea" => C::HddCreate(ItemType::Idea, parse_req(payload)?),
        "hdd.literature" => C::HddCreate(ItemType::Literature, parse_req(payload)?),
        "hdd.validate" => C::HddValidate,
        "hdd.registry" => C::HddRegistry,
        "relation.add" => C::RelationAdd(parse_req(payload)?),
        "relation.query" => C::RelationQuery(parse_req(payload)?),
        "git.push" => C::GitStubAck("git.push"),
        "git.pull" => C::GitStubAck("git.pull"),
        "git.clone" => C::GitStubAck("git.clone"),
        "git.log" => C::GitStubDetail("git.log"),
        "git.blame" => C::GitStubDetail("git.blame"),
        "git.rebase" => C::GitStubDetail("git.rebase"),
        #[cfg(feature = "research")]
        "hdd.run" => C::HddRun(payload.to_vec()),
        #[cfg(feature = "research")]
        "hdd.run.status" => C::HddRunStatus(payload.to_vec()),
        #[cfg(feature = "research")]
        "hdd.run.complete" => C::HddRunComplete(payload.to_vec()),
        #[cfg(feature = "git")]
        "source.push" => C::SourcePush(payload.to_vec()),
        #[cfg(feature = "git")]
        "source.pull" => C::SourcePull(payload.to_vec()),
        #[cfg(feature = "git")]
        "source.branches" => C::SourceBranches,
        #[cfg(feature = "git")]
        "source.delete" => C::SourceDelete(payload.to_vec()),
        other => {
            return Err(KanbanReply::error(
                &format!("Unknown command: {other}"),
                "UNKNOWN_COMMAND",
            ));
        }
    };
    Ok(cmd)
}

/// `parse_payload` with its error mapped back into a typed reply (byte-identical
/// to the former INVALID_PAYLOAD error).
fn parse_req<T: for<'de> serde::Deserialize<'de>>(payload: &[u8]) -> Result<T, KanbanReply> {
    crate::handlers::parse_payload(payload).map_err(KanbanReply::from_error_bytes)
}

/// The stores + write-durability gate — the whole server semantics.
///
/// Reads and writes both go through the synchronous [`apply`](Self::apply); there
/// is no queue, actor, or replica here (that is later PR work).
pub struct KanbanEngine {
    pub store: KanbanStore,
    pub relations: RelationsStore,
    pub data_dir: PathBuf,
    /// Write-durability gate: refuses mutations when the store is not
    /// accepting durable writes, instead of acking writes a restart would lose.
    pub health: HealthGate,
}

impl KanbanEngine {
    pub fn new(
        store: KanbanStore,
        relations: RelationsStore,
        data_dir: PathBuf,
        health: HealthGate,
    ) -> Self {
        Self {
            store,
            relations,
            data_dir,
            health,
        }
    }

    /// Decode → apply → encode. Strips the `kanban.cmd.` subject prefix first.
    pub fn dispatch(&mut self, subject: &str, payload: &[u8]) -> Vec<u8> {
        let verb = subject.strip_prefix("kanban.cmd.").unwrap_or(subject);
        match parse(verb, payload) {
            Ok(cmd) => self.apply(cmd),
            Err(reply) => reply,
        }
        .into_bytes()
    }

    /// Apply a decoded command, replicating the former `dispatch` flow:
    /// (a) admit the mutation before it touches memory; (b) run the logic;
    /// (c) persist-or-degrade after a successful mutation.
    pub fn apply(&mut self, cmd: KanbanCommand) -> KanbanReply {
        let verb = cmd.verb_str();
        let is_mut = cmd.is_mutation();
        let is_rel_mut = cmd.is_relation_mutation();

        // (a) Admit mutations BEFORE they touch memory. Once the store
        // is not durable, refusing up front leaves nothing to roll back. Reads
        // are deliberately unaffected: a degraded board is still worth reading.
        if is_mut
            && let Err(reason) = self
                .health
                .admit_mutation(&crate::health::store_dir(&self.data_dir))
        {
            return KanbanReply::error(
                &format!(
                    "REFUSED — the kanban store is not accepting durable writes ({reason}). Your \
                     change was NOT applied. Reads still work. Free the underlying problem (usually a \
                     full disk); the server re-probes and recovers automatically."
                ),
                "STORE_DEGRADED",
            );
        }

        // (b) run the command's logic.
        let result = self.route(cmd);

        // (c) A persist failure is NOT a warning to step over.
        // The mutation is in memory but not on disk, so report the failure to the
        // caller instead of acking a write we could not keep, and degrade the gate
        // so the NEXT mutation is refused before it ever touches memory.
        match result {
            Ok(reply) => {
                if is_mut {
                    let mut persist_error: Option<String> = None;
                    if let Err(e) = arrow_kanban::persist::save_store(&self.data_dir, &self.store) {
                        persist_error = Some(format!("store: {e}"));
                    }
                    if is_rel_mut
                        && let Err(e) =
                            arrow_kanban::persist::save_relations(&self.data_dir, &self.relations)
                    {
                        persist_error.get_or_insert(format!("relations: {e}"));
                    }
                    match persist_error {
                        Some(err) => {
                            self.health.record_persist_failure(verb, &err);
                            return KanbanReply::error(
                                &format!(
                                    "'{verb}' was applied in memory but COULD NOT BE PERSISTED ({err}). \
                                     Treat this write as LOST — it will not survive a restart. The server is \
                                     now DEGRADED and further mutations are refused until the store accepts \
                                     writes again."
                                ),
                                "STORE_NOT_DURABLE",
                            );
                        }
                        None => self.health.record_persist_success(),
                    }
                }
                reply
            }
            Err(bytes) => KanbanReply::from_error_bytes(bytes),
        }
    }

    /// Route a command to its handler. Exhaustive (no wildcard) so a new command
    /// cannot be added without being wired here.
    fn route(&mut self, cmd: KanbanCommand) -> Result<KanbanReply, Vec<u8>> {
        #[cfg(feature = "research")]
        use crate::handlers::research;
        #[cfg(feature = "git")]
        use crate::handlers::source;
        use crate::handlers::{core, relations};
        use KanbanCommand as C;

        match cmd {
            C::Create(req) => core::handle_create_typed(req, &mut self.store, &mut self.relations),
            C::Move(req) => core::handle_move_typed(req, &mut self.store),
            C::Ratify(req) => core::handle_ratify_typed(req, &mut self.store),
            C::Update(req) => core::handle_update_typed(req, &mut self.store, &mut self.relations),
            C::Comment(req) => core::handle_comment_typed(req, &mut self.store),
            C::Rank(req) => core::handle_rank_typed(req, &mut self.store),
            C::List(req) => core::handle_list_typed(req, &self.store),
            C::Show(req) => core::handle_show_typed(req, &self.store),
            C::Query => Ok(KanbanReply::error(
                "query cross-searches proposals, which are not part of the open kanban engine",
                "UNSUPPORTED",
            )),
            C::Board(req) => core::handle_board_typed(req, &self.store),
            C::Stats(payload) => core::handle_stats_typed(&payload, &self.store),
            C::Delete(req) => core::handle_delete_typed(req, &mut self.store),
            C::Validate => core::handle_validate_typed(&self.store, &self.relations),
            C::Export(req) => core::handle_export_typed(req, &self.store),
            C::Roadmap(req) => core::handle_roadmap_typed(req, &self.store, &self.relations),
            C::CriticalPath => core::handle_critical_path_typed(&self.store),
            C::PipelineHealth => Ok(KanbanReply::error(
                "pipeline-health is a fleet analytics command, not part of the open kanban engine",
                "UNSUPPORTED",
            )),
            C::Worklist(req) => core::handle_worklist_typed(req, &self.store),
            C::NextId(req) => core::handle_next_id_typed(req, &self.store),
            C::History(req) => core::handle_history_typed(req, &self.store),
            C::Blocked => core::handle_blocked_typed(&self.store),
            C::Templates(payload) => core::handle_templates_typed(&payload, &self.data_dir),
            C::HddCreate(item_type, req) => {
                core::handle_hdd_create_typed(req, &mut self.store, &mut self.relations, item_type)
            }
            C::HddValidate => core::handle_hdd_validate_typed(&self.store, &self.relations),
            C::HddRegistry => core::handle_hdd_registry_typed(&self.store, &self.relations),
            C::RelationAdd(req) => relations::handle_relation_add_typed(req, &mut self.relations),
            C::RelationQuery(req) => relations::handle_relation_query_typed(req, &self.relations),
            C::GitStubAck(verb) => Ok(KanbanReply::Value(serde_json::json!({
                "message": format!(
                    "git.{} acknowledged. Graph git stores are currently per-agent \
                     (local per-agent git-store directories). Server-managed git \
                     state is planned.",
                    verb.strip_prefix("git.").unwrap_or(verb)
                ),
            }))),
            C::GitStubDetail(verb) => Ok(KanbanReply::Value(serde_json::json!({
                "detail": format!(
                    "git.{}: operates on local graph store. Use --store to specify path. \
                     Server-side git operations are planned for a future release.",
                    verb.strip_prefix("git.").unwrap_or(verb)
                ),
            }))),
            #[cfg(feature = "research")]
            C::HddRun(payload) => {
                research::handle_hdd_run_typed(&payload, &mut self.store, &self.data_dir)
            }
            #[cfg(feature = "research")]
            C::HddRunStatus(payload) => {
                research::handle_hdd_run_status_typed(&payload, &self.data_dir)
            }
            #[cfg(feature = "research")]
            C::HddRunComplete(payload) => {
                research::handle_hdd_run_complete_typed(&payload, &mut self.store, &self.data_dir)
            }
            #[cfg(feature = "git")]
            C::SourcePush(payload) => source::handle_source_push_typed(&payload, &self.data_dir),
            #[cfg(feature = "git")]
            C::SourcePull(payload) => source::handle_source_pull_typed(&payload, &self.data_dir),
            #[cfg(feature = "git")]
            C::SourceBranches => source::handle_source_branches_typed(&self.data_dir),
            #[cfg(feature = "git")]
            C::SourceDelete(payload) => {
                source::handle_source_delete_typed(&payload, &self.data_dir)
            }
        }
    }
}
