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

use crate::extension::{EngineExtension, ExtCommand};
use crate::handlers::ErrorResponse;
use crate::handlers::core::{
    BoardRequest, CommentRequest, CreateRequest, CreateResponse, DeleteRequest, DeleteResponse,
    ExportRequest, FlowRequest, HddCreateRequest, HistoryRequest, ListRequest, MoveRequest,
    MoveResponse, NextIdRequest, RankRequest, RatifyRequest, RequeueRequest, RoadmapRequest,
    ShowRequest, UpdateRequest, WorklistRequest,
};
use crate::handlers::relations::{RelationAddRequest, RelationQueryRequest, RelationRemoveRequest};
use crate::health::HealthGate;
use crate::lease::{LocalWriterLease, WriterLease};
use crate::storage::{CommittedEvent, ParquetBackend, Seq, StorageBackend, StoreError};
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

    /// The reply as a JSON value, WITHOUT consuming it — the outbox event
    /// derivation reads reply-only fields (the allocated id, the prior status,
    /// the minted relation id) from here. Structurally identical to
    /// [`into_bytes`](Self::into_bytes)'s output, just not yet serialized.
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            KanbanReply::Create(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            KanbanReply::Move(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            KanbanReply::Delete(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            KanbanReply::Value(v) => v.clone(),
            KanbanReply::Error(e) => serde_json::to_value(e).unwrap_or(serde_json::Value::Null),
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
///
/// `Clone` lets [`KanbanEngine::apply`] keep the decoded command alive across
/// routing so the committed event can be derived from `(command, reply)` after
/// the handler has run (the reply carries the allocated id / prior status).
#[derive(Clone)]
pub enum KanbanCommand {
    // ── Core CRUD + analytics ──
    Create(CreateRequest),
    Move(MoveRequest),
    Claim(crate::handlers::core::ClaimRequest),
    Bounce(crate::handlers::core::BounceRequest),
    /// `requeue` — release a failed executor's claim, increment the item's
    /// `attempt_count`, dead-letter at the caller's cap (issue #40). One engine
    /// turn, so the whole transition is atomic under the single writer.
    Requeue(RequeueRequest),
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
    Flow(FlowRequest),
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
    RelationRemove(RelationRemoveRequest),
    RelationQuery(RelationQueryRequest),
    // ── Command extension (E3 3c-i) ──
    /// A namespaced extension verb (`ext.<namespace>.<verb>`), resolved against
    /// the engine's registered [`EngineExtension`]. Never produced by [`parse`]
    /// (which owns the closed CORE verb set) — only by [`KanbanEngine::dispatch`]
    /// for a verb `parse` did not recognise, so it can never shadow a core verb.
    Extension(ExtCommand),
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
            C::Claim(_) => "claim",
            C::Bounce(_) => "bounce",
            C::Requeue(_) => "requeue",
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
            C::Flow(_) => "flow",
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
            C::RelationRemove(_) => "relation.remove",
            C::RelationQuery(_) => "relation.query",
            // The opaque, frozen &'static for every extension verb — the &'static
            // cannot name the dynamic verb. `verb_display` carries the real name.
            C::Extension(_) => "ext",
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

    /// A human-facing verb label for diagnostics — the frozen [`verb_str`](Self::verb_str)
    /// for a core command, and the full namespaced verb (e.g. `ext.demo.propose`)
    /// for an extension command, whose `verb_str` is the opaque `"ext"`. Returns a
    /// [`Cow`] that never borrows `self`: `Borrowed` wraps the `&'static` core verb,
    /// `Owned` clones the extension's verb — so a caller can hold it across a move
    /// of the command (as [`KanbanEngine::apply`] does).
    pub fn verb_display(&self) -> std::borrow::Cow<'static, str> {
        match self {
            KanbanCommand::Extension(e) => std::borrow::Cow::Owned(e.verb.clone()),
            other => std::borrow::Cow::Borrowed(other.verb_str()),
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
            | C::Claim(_)
            | C::Bounce(_)
            | C::Requeue(_)
            | C::Ratify(_)
            | C::Update(_)
            | C::Comment(_)
            | C::Rank(_)
            | C::Delete(_)
            | C::HddCreate(_, _)
            | C::RelationAdd(_)
            | C::RelationRemove(_) => true,
            C::Flow(_) => false,
            // A property of the REGISTRATION (from `ExtVerbSpec`), not the opaque
            // `verb_str` — so it travels on the command itself.
            C::Extension(e) => e.is_mutation,
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
        matches!(
            self,
            KanbanCommand::RelationAdd(_) | KanbanCommand::RelationRemove(_)
        )
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
    "flow",
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
    "relation.remove",
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

/// The outcome of decoding a wire verb, before it is resolved against a possible
/// extension. Splitting `UnknownVerb` out (rather than collapsing straight to the
/// error reply) is what lets [`KanbanEngine::dispatch`] offer an unknown verb to
/// the extension while keeping [`parse`] byte-identical for every core verb.
enum ParseOutcome {
    /// A recognised core verb, decoded.
    Cmd(KanbanCommand),
    /// A recognised core verb whose payload failed to decode — the existing
    /// `INVALID_PAYLOAD` reply, byte-for-byte.
    BadPayload(KanbanReply),
    /// Not a core verb (may still be an extension verb, or genuinely unknown).
    UnknownVerb,
}

/// Decode a CORE wire verb + payload, without resolving extensions — the single
/// source of the core verb→command mapping. [`parse`] and
/// [`KanbanEngine::dispatch`] both go through it, so the two never drift.
///
/// - A regular verb is `parse_payload::<T>`'d into its variant; a decode error is
///   the existing `INVALID_PAYLOAD` reply, byte-for-byte.
/// - Lenient / hand-parsed verbs carry the raw payload (their handler parses).
/// - An unrecognised verb is [`ParseOutcome::UnknownVerb`] — resolved to
///   `UNKNOWN_COMMAND` by [`parse`], or offered to the extension by `dispatch`.
fn parse_inner(verb: &str, payload: &[u8]) -> ParseOutcome {
    use KanbanCommand as C;
    // A required-payload verb: decode, or short-circuit to the byte-identical
    // INVALID_PAYLOAD reply. The target type is inferred from the variant.
    macro_rules! req {
        () => {
            match parse_req(payload) {
                Ok(v) => v,
                Err(reply) => return ParseOutcome::BadPayload(reply),
            }
        };
    }
    let cmd = match verb {
        "create" => C::Create(req!()),
        "move" => C::Move(req!()),
        "claim" => C::Claim(req!()),
        "bounce" => C::Bounce(req!()),
        "requeue" => C::Requeue(req!()),
        "ratify" => C::Ratify(req!()),
        "update" => C::Update(req!()),
        "comment" => C::Comment(req!()),
        "rank" => C::Rank(req!()),
        "list" => C::List(req!()),
        "show" => C::Show(req!()),
        "query" => C::Query,
        "board" => C::Board(req!()),
        "stats" => C::Stats(payload.to_vec()),
        "delete" => C::Delete(req!()),
        "validate" => C::Validate,
        "export" => C::Export(req!()),
        "roadmap" => C::Roadmap(req!()),
        "flow" => C::Flow(req!()),
        "critical-path" => C::CriticalPath,
        "pipeline-health" => C::PipelineHealth,
        "worklist" => C::Worklist(req!()),
        "next-id" => C::NextId(req!()),
        "history" => C::History(req!()),
        "blocked" => C::Blocked,
        "templates" => C::Templates(payload.to_vec()),
        "hdd.paper" => C::HddCreate(ItemType::Paper, req!()),
        "hdd.hypothesis" => C::HddCreate(ItemType::Hypothesis, req!()),
        "hdd.experiment" => C::HddCreate(ItemType::Experiment, req!()),
        "hdd.measure" => C::HddCreate(ItemType::Measure, req!()),
        "hdd.idea" => C::HddCreate(ItemType::Idea, req!()),
        "hdd.literature" => C::HddCreate(ItemType::Literature, req!()),
        "hdd.validate" => C::HddValidate,
        "hdd.registry" => C::HddRegistry,
        "relation.add" => C::RelationAdd(req!()),
        "relation.remove" => C::RelationRemove(req!()),
        "relation.query" => C::RelationQuery(req!()),
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
        _ => return ParseOutcome::UnknownVerb,
    };
    ParseOutcome::Cmd(cmd)
}

/// Decode a wire verb + payload into a typed CORE command.
///
/// BYTE-IDENTICAL to the former hand-written match (the codec property test calls
/// this directly): a known verb decodes; a known verb with a bad payload is the
/// existing `INVALID_PAYLOAD` reply; an unknown verb is the existing
/// `UNKNOWN_COMMAND` reply. Extension verbs are resolved in
/// [`KanbanEngine::dispatch`], never here, so `parse`'s contract is unchanged.
pub fn parse(verb: &str, payload: &[u8]) -> Result<KanbanCommand, KanbanReply> {
    match parse_inner(verb, payload) {
        ParseOutcome::Cmd(cmd) => Ok(cmd),
        ParseOutcome::BadPayload(reply) => Err(reply),
        ParseOutcome::UnknownVerb => Err(KanbanReply::error(
            &format!("Unknown command: {verb}"),
            "UNKNOWN_COMMAND",
        )),
    }
}

/// `parse_payload` with its error mapped back into a typed reply (byte-identical
/// to the former INVALID_PAYLOAD error).
fn parse_req<T: for<'de> serde::Deserialize<'de>>(payload: &[u8]) -> Result<T, KanbanReply> {
    crate::handlers::parse_payload(payload).map_err(KanbanReply::from_error_bytes)
}

/// The stores + write-durability gate + durable [`StorageBackend`] + writer lease
/// — the whole server semantics.
///
/// Generic over the backend so a consumer swaps durability by construction, and
/// over the [`WriterLease`] so a deployment swaps its lease authority by
/// construction. Both default to the shipped types ([`ParquetBackend`],
/// [`LocalWriterLease`]), so `KanbanEngine` alone is the single-canonical-server
/// engine. Reads and writes both go through the synchronous [`apply`](Self::apply).
pub struct KanbanEngine<B: StorageBackend = ParquetBackend, L: WriterLease = LocalWriterLease> {
    pub store: KanbanStore,
    pub relations: RelationsStore,
    pub data_dir: PathBuf,
    /// Write-durability gate: refuses mutations when the store is not
    /// accepting durable writes, instead of acking writes a restart would lose.
    pub health: HealthGate,
    /// The durable commit boundary + outbox source.
    pub backend: B,
    /// The writer lease. At the commit boundary the engine fences itself if a
    /// newer writer has taken the lease (a higher current epoch); the epoch it
    /// holds is stamped on every commit.
    pub lease: L,
    /// An optional command extension (E3 3c-i): a namespaced verb family with its
    /// own in-memory tables, routed through the SAME commit boundary as core.
    /// `None` is the open engine — zero extension verbs, zero extension events.
    pub extension: Option<Box<dyn EngineExtension + Send>>,
}

impl KanbanEngine<ParquetBackend, LocalWriterLease> {
    /// Construct over the default [`ParquetBackend`] rooted at `data_dir`, taking
    /// the default [`LocalWriterLease`] at the store directory. The backend
    /// discovers its committed high-water seq from any existing commit log (0 for
    /// a fresh store); a store dir that cannot be scanned starts at 0.
    pub fn new(
        store: KanbanStore,
        relations: RelationsStore,
        data_dir: PathBuf,
        health: HealthGate,
    ) -> Self {
        let backend = ParquetBackend::open(&data_dir).unwrap_or_else(|_| {
            // A fresh backend at seq 0 — only reached if the store dir cannot be
            // read at all, in which case the first commit's append will fail loud.
            ParquetBackend::open(&PathBuf::from(".")).expect("default backend")
        });
        Self::with_backend(store, relations, data_dir, health, backend)
    }

    /// Construct the default-backend engine WITH a command extension installed —
    /// the engine that offers the extension's namespaced verbs alongside the core
    /// set. Equivalent to [`new`](Self::new) followed by setting the public
    /// `extension` field; provided so a deployment wires its extension in one call.
    pub fn with_extension(
        store: KanbanStore,
        relations: RelationsStore,
        data_dir: PathBuf,
        health: HealthGate,
        extension: Box<dyn EngineExtension + Send>,
    ) -> Self {
        let mut engine = Self::new(store, relations, data_dir, health);
        engine.extension = Some(extension);
        engine
    }
}

impl<B: StorageBackend> KanbanEngine<B, LocalWriterLease> {
    /// Construct over an explicit backend (a fault-injecting double in tests, a
    /// future durability backend in production) and the default
    /// [`LocalWriterLease`], acquired at the store directory.
    pub fn with_backend(
        store: KanbanStore,
        relations: RelationsStore,
        data_dir: PathBuf,
        health: HealthGate,
        backend: B,
    ) -> Self {
        // Acquire at the data ROOT (which already exists — it is `--data-dir`),
        // NOT the `.arrow-kanban` store dir. The store dir is created lazily by the
        // first commit; pre-creating it here to hold the lease would change the
        // durability-fault semantics (a store dir that exists + is writable while
        // the root is read-only would let a write land that must be refused).
        let lease = LocalWriterLease::acquire(&data_dir);
        Self::with_backend_and_lease(store, relations, data_dir, health, backend, lease)
    }
}

impl<B: StorageBackend, L: WriterLease> KanbanEngine<B, L> {
    /// Construct over an explicit backend AND an explicit writer lease — the fully
    /// generic constructor a deployment (or a test) uses to swap either seam.
    pub fn with_backend_and_lease(
        store: KanbanStore,
        relations: RelationsStore,
        data_dir: PathBuf,
        health: HealthGate,
        backend: B,
        lease: L,
    ) -> Self {
        Self {
            store,
            relations,
            data_dir,
            health,
            backend,
            lease,
            // The open engine: no extension until one is installed (`with_extension`
            // or by setting the public field). Every existing construction site
            // defaults through here, so none is disturbed.
            extension: None,
        }
    }

    /// The high-water committed sequence — the durable log position.
    pub fn committed_seq(&self) -> Seq {
        self.backend.committed_seq()
    }

    /// Committed events with `seq` strictly greater than `after`, in seq order —
    /// the outbox relay's source.
    pub fn events_since(&self, after: Seq) -> Result<Vec<CommittedEvent>, StoreError> {
        self.backend.events_since(after)
    }

    /// Restore the installed extension's tables on load — the 3c-ii completion of
    /// the extension write path. Call once, after the extension is installed and
    /// the core state is loaded (`backend.load`), before serving.
    ///
    /// If an extension is present: load its derived checkpoint
    /// ([`EngineExtension::restore`]) for the seq it reflects, then replay every
    /// committed [`MutationEvent::Extension`] with `seq` strictly greater than that
    /// through [`EngineExtension::replay`]. A torn or absent ext checkpoint restores
    /// from `0`, so the whole ext tail is replayed idempotently — the ext state is
    /// durable across a restart, checkpoint-or-log. A no-op for the open engine.
    ///
    /// This drives ONLY the extension's tables; the core store's load/replay is the
    /// backend's job, so [`StorageBackend`] stays unchanged.
    pub fn restore_extension(&mut self) -> Result<(), StoreError> {
        if self.extension.is_none() {
            return Ok(());
        }
        let dir = crate::health::store_dir(&self.data_dir);
        // (1) Load the ext checkpoint for the seq it reflects. The `&mut` borrow of
        // the extension is released before the backend read below.
        let ext_seq = self
            .extension
            .as_mut()
            .expect("extension present")
            .restore(&dir)
            .map_err(StoreError::Checkpoint)?;
        // (2) Read the committed tail the ext checkpoint does not yet reflect
        // (immutable backend borrow, disjoint from the extension field).
        let tail = self.backend.events_since(ext_seq)?;
        // (3) Replay each EXTENSION event onto the ext tables; core events belong to
        // the core store and are skipped here. `replay` is idempotent, so a torn
        // checkpoint re-presenting the tail does not double a row.
        let ext = self.extension.as_mut().expect("extension present");
        for committed in tail {
            if let crate::events::MutationEvent::Extension {
                namespace,
                kind,
                payload,
            } = committed.event
            {
                ext.replay(&namespace, &kind, &payload)
                    .map_err(StoreError::Checkpoint)?;
            }
        }
        Ok(())
    }

    /// Decode → apply → encode. Strips the `kanban.cmd.` subject prefix first.
    ///
    /// A verb the CORE `parse` does not recognise is offered to the registered
    /// extension (if any) — never before, so an extension can never intercept a
    /// core verb.
    pub fn dispatch(&mut self, subject: &str, payload: &[u8]) -> Vec<u8> {
        let verb = subject.strip_prefix("kanban.cmd.").unwrap_or(subject);
        // Vocabulary hot-reload is PROCESS-level state, not store state: it
        // commits nothing, appends no event, and touches no parquet — so it is
        // handled before the typed parse, outside the health gate and the
        // writer-lease fence (both of which guard DURABLE writes this verb
        // never performs). The reload itself is fail-closed in the vocabulary
        // module: any refusal leaves the running set untouched.
        if verb == "admin_reload_vocab" {
            return crate::handlers::admin_reload_vocab(payload);
        }
        let reply = match parse_inner(verb, payload) {
            ParseOutcome::Cmd(cmd) => self.apply(cmd),
            ParseOutcome::BadPayload(reply) => reply,
            ParseOutcome::UnknownVerb => self.dispatch_unknown(verb, payload),
        };
        reply.into_bytes()
    }

    /// Resolve a verb the core did not recognise against the registered extension.
    /// The extension classifies it (declaring its namespace + mutation flag), and
    /// the engine routes it as a [`KanbanCommand::Extension`] through the SAME
    /// [`apply`](Self::apply) — commit boundary, health gate, and lease fence all
    /// included. An unclassified verb, or no extension at all, is the same
    /// `UNKNOWN_COMMAND` reply the core always produced (so a bare `demo.x` with no
    /// extension, or a registered extension's un-owned `ext.demo.bogus`, both fall
    /// through here unchanged).
    fn dispatch_unknown(&mut self, verb: &str, payload: &[u8]) -> KanbanReply {
        match self.extension.as_ref().and_then(|x| x.classify(verb)) {
            Some(spec) => self.apply(KanbanCommand::Extension(ExtCommand {
                namespace: spec.namespace,
                verb: verb.to_string(),
                payload: payload.to_vec(),
                is_mutation: spec.is_mutation,
            })),
            None => KanbanReply::error(&format!("Unknown command: {verb}"), "UNKNOWN_COMMAND"),
        }
    }

    /// Apply a decoded command, replicating the former `dispatch` flow:
    /// (a) admit the mutation before it touches memory; (b) run the logic;
    /// (c) persist-or-degrade after a successful mutation.
    pub fn apply(&mut self, cmd: KanbanCommand) -> KanbanReply {
        // `verb_display` (not `verb_str`) so an extension command's diagnostics
        // name the real verb (`ext.demo.propose`), not the opaque `"ext"`. The Cow
        // borrows nothing from `cmd` (Borrowed wraps the &'static core verb), so it
        // stays valid across the `route(cmd)` move below; for a core command it is
        // byte-identical to the former &'static verb.
        let verb = cmd.verb_display();
        let is_mut = cmd.is_mutation();

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
                     full disk); the server re-probes and recovers automatically. \
                     [server session start_ms={} — compare via pipeline-health to tell a \
                     restart from a recovery]",
                    self.health.session_start_ms()
                ),
                "STORE_DEGRADED",
            );
        }

        // (a2) FENCE a stale writer BEFORE the mutation touches memory. If a newer
        // writer has taken the lease (a current epoch higher than the one this
        // engine holds), refuse — even though this engine still holds a live
        // handle. Checking here, alongside the health-gate admission, is what makes
        // a fenced mutation truly NOT applied: `route` never runs, nothing is
        // appended to `_events.log`, no event is emitted, and the committed seq
        // does not advance. The epoch this engine DOES hold is stamped on the
        // commit below, so the durable log records the producing writer generation.
        if is_mut {
            match self.lease.is_current() {
                Ok(true) => {}
                Ok(false) => {
                    return KanbanReply::error(
                        &format!(
                            "'{verb}' was REFUSED — this writer is FENCED: a newer writer holds the \
                             lease (the epoch moved past the one this process holds). Your change \
                             was NOT applied and is NOT durable. Re-acquire the lease before writing."
                        ),
                        "WRITER_FENCED",
                    );
                }
                Err(e) => {
                    return KanbanReply::error(
                        &format!(
                            "'{verb}' was REFUSED — the writer lease could not be verified ({e}); \
                             refusing to commit rather than risk a split-brain write. No change was \
                             applied."
                        ),
                        "WRITER_FENCED",
                    );
                }
            }
        }

        // Keep the decoded command alive across routing so the committed event
        // can be derived from `(command, reply)` — the reply carries the
        // allocated id / prior status a checkpoint replay needs.
        let event_cmd = if is_mut { Some(cmd.clone()) } else { None };

        // (b) run the command's logic.
        let result = self.route(cmd);

        // (c) The commit boundary. The mutation is in memory; it is durable only
        // once `backend.commit` returns Ok — the append+fsync of its event is the
        // barrier. On failure the write is NOT durable, so we report the failure
        // to the caller instead of acking a write we could not keep, and degrade
        // the gate so the NEXT mutation is refused before it ever touches memory.
        match result {
            Ok(reply) => {
                if is_mut {
                    // Derive the event that this write commits with. A mutation
                    // MUST produce one (every `is_mutation` command is covered by
                    // `mutation_event`); a None here is an internal inconsistency,
                    // handled fail-closed as a durability failure rather than a
                    // silent unpersisted write (the exact incident shape).
                    // An extension mutation derives its event through the
                    // extension itself; every core mutation through the exhaustive
                    // core derivation. Both feed the SAME commit below.
                    let event = match event_cmd.as_ref() {
                        Some(KanbanCommand::Extension(ext)) => self
                            .extension
                            .as_ref()
                            .and_then(|x| x.derive_event(ext, &reply)),
                        Some(c) => crate::events::mutation_event(c, &reply),
                        None => None,
                    };
                    let commit = match &event {
                        Some(ev) => self
                            .backend
                            .commit(&self.store, &self.relations, self.lease.epoch(), ev)
                            .map_err(|e| e.to_string()),
                        None => Err(format!(
                            "'{verb}' mutated state but produced no event — refusing to ack an \
                             unlogged write"
                        )),
                    };
                    match commit {
                        Ok(seq) => {
                            self.health.record_persist_success();
                            // A derived, POST-commit extension checkpoint — the
                            // analogue of the core `_checkpoint.manifest`, written
                            // alongside it (NOT through `StorageBackend`, which is
                            // unchanged). Best-effort: the ext state is already
                            // durable in the event log, so a torn ext checkpoint is
                            // replayed from the log tail on load. A failure here
                            // NEVER fails the commit — it only leaves the ext
                            // checkpoint trailing, which the idempotent replay tolerates.
                            if let Some(ext) = self.extension.as_ref() {
                                let dir = crate::health::store_dir(&self.data_dir);
                                if let Err(e) = ext.checkpoint(&dir, seq) {
                                    eprintln!(
                                        "kanban: extension checkpoint after commit seq={seq} \
                                         failed ({e}); the uncovered ext tail will be replayed \
                                         idempotently on load."
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            self.health.record_persist_failure(&verb, &err);
                            return KanbanReply::error(
                                &format!(
                                    "'{verb}' was applied in memory but COULD NOT BE COMMITTED ({err}). \
                                     Treat this write as LOST — it will not survive a restart. The server is \
                                     now DEGRADED and further mutations are refused until the store accepts \
                                     writes again. [server session start_ms={} — after recovery, \
                                     compare via pipeline-health; a CHANGED value means the server restarted \
                                     and this write is gone (redo it); UNCHANGED means the in-memory write \
                                     was flushed (redoing DUPLICATES).]",
                                    self.health.session_start_ms()
                                ),
                                "STORE_NOT_DURABLE",
                            );
                        }
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
            C::Claim(req) => core::handle_claim_typed(req, &mut self.store),
            C::Bounce(req) => core::handle_bounce_typed(req, &mut self.store),
            C::Requeue(req) => core::handle_requeue_typed(req, &mut self.store),
            C::Ratify(req) => core::handle_ratify_typed(req, &mut self.store),
            C::Update(req) => core::handle_update_typed(req, &mut self.store, &mut self.relations),
            C::Comment(req) => core::handle_comment_typed(req, &mut self.store),
            C::Rank(req) => core::handle_rank_typed(req, &mut self.store),
            C::List(req) => core::handle_list_typed(req, &self.store),
            C::Show(req) => core::handle_show_typed(req, &self.store, &self.relations),
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
            C::Flow(req) => core::handle_flow_typed(req, &self.store, &self.data_dir),
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
            C::RelationRemove(req) => {
                relations::handle_relation_remove_typed(req, &mut self.relations, &mut self.store)
            }
            C::RelationQuery(req) => relations::handle_relation_query_typed(req, &self.relations),
            C::Extension(ext) => match self.extension.as_mut() {
                // Run the extension's own apply. Its typed error reply is mapped
                // into the engine's byte-carrying error channel; `apply`
                // reconstructs it verbatim via `from_error_bytes`.
                Some(x) => x
                    .apply(&ext.verb, &ext.payload)
                    .map_err(KanbanReply::into_bytes),
                // Unreachable in practice: `Extension` is only ever constructed
                // after `classify` claimed the verb, so the extension is present.
                // Handled fail-safe as UNKNOWN_COMMAND rather than a panic.
                None => Ok(KanbanReply::error(
                    &format!("Unknown command: {}", ext.verb),
                    "UNKNOWN_COMMAND",
                )),
            },
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
