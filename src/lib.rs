// SPDX-License-Identifier: MIT
//! arrow-kanban — Arrow-native, graph-native kanban engine.
//!
//! Provides:
//! - **Config** — parse `.arrow-kanban/config.yaml` (dual boards, WIP limits, state graphs)
//! - **Schemas** — Arrow table definitions for items, relations, runs
//! - **Item Types** — 13 types across development and research boards
//! - **ID Allocation** — sequential, conflict-safe ID assignment
//! - **State Machine** — valid transitions, WIP limit enforcement
//! - **CRUD** — create, read, update, delete items with audit trail
//! - **Relations** — cross-item and cross-board links
//! - **Persistence** — Parquet-backed load/save
//! - **Query** — NL + structured filter extraction
//! - **Display** — terminal table and board rendering
//! - **Export** — markdown + next-id output
//! - **Migration** — parse markdown (with turtle frontmatter) into Arrow tables

pub mod backup;
pub mod base64;
#[cfg(feature = "client")]
pub mod client;
pub mod comments;
pub mod config;
pub mod critical_path;
pub mod crud;
pub mod display;
pub mod embedding;
pub mod experiment_runs;
pub mod export;
pub mod file_index;
pub mod flow;
pub mod hdd;
pub mod hooks;
pub mod id_alloc;
pub mod item_type;
#[cfg(feature = "client")]
pub mod mcp_server;
pub mod migrate;
pub mod parquet_store;
pub mod persist;
pub mod persistence;
pub mod query;
pub mod relation_vocab;
pub mod relations;
pub mod schema;
pub mod semantic;
pub mod state_machine;
pub mod state_model;
pub mod stats;
pub mod templates;
pub mod theme;
pub mod traversal;
pub mod turtle_builder;
pub mod validate;

pub use comments::CommentsStore;
pub use config::{BoardConfig, ConfigFile};
pub use crud::{CreateItemInput, CrudError, KanbanStore};
pub use hdd::{
    build_registry, create_experiment, create_hypothesis, create_idea, create_literature,
    create_measure, create_paper, query_experiment_queue, traverse_relations, validate_hdd,
};
pub use id_alloc::{allocate_id, allocate_id_from_str, max_id_for_type};
pub use item_type::ItemType;
pub use migrate::{MigrateResult, migrate_boards};
pub use persistence::{
    GitBackupMetrics, HealthMetrics, PersistenceConfig, PersistenceEngine, SaveMetrics,
};
pub use relations::RelationsStore;
pub use schema::{comments_schema, items_schema, relations_schema, runs_schema};
pub use state_machine::{check_wip_limit, validate_transition};
