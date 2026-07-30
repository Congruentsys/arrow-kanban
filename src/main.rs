// SPDX-License-Identifier: MIT
//! arrow-kanban CLI — Arrow-native kanban engine.
//!
//! Wraps the arrow-kanban library with a clap-based CLI providing
//! command-line access to boards, items, relationships, and queries.

use arrow::array::Array;
use arrow_kanban::config::ConfigFile;
use arrow_kanban::critical_path;
use arrow_kanban::crud::CreateItemInput;
use arrow_kanban::display;
use arrow_kanban::export;
use arrow_kanban::id_alloc;
use arrow_kanban::item_type::ItemType;
use arrow_kanban::parquet_store::{Commit, persist_commits, restore_commits};
use arrow_kanban::persist;
use arrow_kanban::query;
use arrow_kanban::state_machine;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process;

/// Generate a body template for a given item type using SHACL shapes.
fn generate_template(item_type: &str, title: &str, root: &std::path::Path) -> String {
    let loader = arrow_kanban::templates::ShapeLoader::new(root);
    let generator = arrow_kanban::templates::TemplateGenerator::new(loader);

    if let Some(it) = ItemType::from_str_loose(item_type) {
        generator.generate(&it, title)
    } else {
        format!("# {title}\n")
    }
}

#[derive(Parser)]
#[command(name = "arrow-kanban", about = "Arrow-native kanban engine")]
struct Cli {
    /// Working directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// NATS server URL for remote mode (e.g., nats://mini:4222)
    #[arg(long)]
    server: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
// CLI command enum: the Training subcommand carries a large optional-arg spec. Boxing clap
// `#[arg]` fields breaks the derive; a command enum built once per invocation has no size
// pressure, so allow the variant-size difference.
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Create a new item
    Create {
        /// Item type (expedition, chore, voyage, etc.)
        item_type: String,
        /// Title for the new item
        title: String,
        /// Priority (low, medium, high, critical)
        #[arg(long)]
        priority: Option<String>,
        /// Assign to an agent
        #[arg(long)]
        assign: Option<String>,
        /// Tags (comma-separated)
        #[arg(long)]
        tags: Option<String>,
        /// Body content (inline text)
        #[arg(long)]
        body: Option<String>,
        /// Read body content from a file
        #[arg(long)]
        body_file: Option<String>,
        /// Read body content from stdin
        #[arg(long)]
        body_stdin: bool,
        /// Use a built-in body template (expedition, chore, voyage)
        #[arg(long)]
        template: Option<String>,
        /// Typed relationship, repeatable: --relate <predicate>:<TARGET-ID>
        ///
        /// e.g. --relate implements:VY-1234 --relate validates:H-1234
        /// Predicates: related, dependsOn, blocks, parent, implements, spawns,
        /// tests, validates, measures. Domain/range are enforced.
        #[arg(long = "relate", value_name = "PREDICATE:TARGET")]
        relate: Vec<String>,
        /// Push after creating (atomic create + commit + push)
        #[arg(long)]
        push: bool,
    },

    /// Move an item to a new status
    Move {
        /// Item ID (e.g.)
        id: String,
        /// Target status
        status: String,
        /// Assign to an agent
        #[arg(long)]
        assign: Option<String>,
        /// Force move (bypass WIP limits)
        #[arg(long)]
        force: bool,
        /// Resolution reason (completed, superseded, wont_do, duplicate, obsolete, merged, refuted)
        #[arg(long)]
        resolution: Option<String>,
        /// Provenance URI for closure (e.g., PROP-1234, PR URL)
        #[arg(long)]
        closed_by: Option<String>,
    },

    /// Ratify a phase: strip `pending-ratification` from every item carrying the phase
    /// tag, sprint-start its voyages, and verify none remain — one atomic call (no shell loop).
    Ratify {
        /// The phase tag to ratify (e.g., v19-phase-e)
        #[arg(long)]
        phase: String,
    },

    /// Update fields on an existing item
    Update {
        /// Item ID (e.g.)
        id: String,
        /// New title
        #[arg(long)]
        title: Option<String>,
        /// New priority (low, medium, high, critical)
        #[arg(long)]
        priority: Option<String>,
        /// New assignee
        #[arg(long)]
        assign: Option<String>,
        /// New tags (comma-separated, replaces existing)
        #[arg(long)]
        tags: Option<String>,
        /// New body content (inline)
        #[arg(long)]
        body: Option<String>,
        /// New body content from file
        #[arg(long)]
        body_file: Option<String>,
        /// New related items (comma-separated, replaces existing)
        #[arg(long)]
        related: Option<String>,
        /// New depends-on items (comma-separated, replaces existing)
        #[arg(long)]
        depends_on: Option<String>,
        /// ADD a typed relationship, repeatable: --relate <predicate>:<TARGET-ID>
        ///
        /// Unlike --related/--depends-on (which REPLACE), --relate is ADDITIVE:
        /// adding an edge never silently deletes the others.
        #[arg(long = "relate", value_name = "PREDICATE:TARGET")]
        relate: Vec<String>,
        /// REMOVE a typed relationship, repeatable: --unrelate <predicate>:<TARGET-ID>
        #[arg(long = "unrelate", value_name = "PREDICATE:TARGET")]
        unrelate: Vec<String>,
    },

    /// Add a comment to an item
    Comment {
        /// Item ID
        id: String,
        /// Comment text
        text: String,
    },

    /// List items with optional filters
    List {
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
        /// Filter by board (development, research)
        #[arg(long)]
        board: Option<String>,
        /// Filter by item type
        #[arg(long, name = "type")]
        item_type: Option<String>,
        /// Filter by assignee
        #[arg(long)]
        assignee: Option<String>,
        /// Filter by resolution (completed, superseded, wont_do, duplicate, obsolete, merged, refuted)
        #[arg(long)]
        resolution: Option<String>,
        /// Filter by priority (critical, high, medium, low)
        #[arg(long)]
        priority: Option<String>,
        /// Filter by tag (exact match, multiple --tag flags = AND)
        #[arg(long)]
        tag: Vec<String>,
        /// Show only items with all dependencies met (unblocked)
        #[arg(long)]
        ready: bool,
    },

    /// Show full board view
    Board {
        /// Board to display (development, research)
        #[arg(long, default_value = "development")]
        board: String,
    },

    /// Show full item details
    Show {
        /// Item ID
        id: String,
        /// Output format: default (metadata + body), md (full markdown), json
        #[arg(long, default_value = "default")]
        format: String,
        /// Show all relationships (depends_on, related, RelationsStore edges)
        #[arg(long)]
        relations: bool,
    },

    /// Search items with natural language or structured queries
    Query {
        /// Query string (NL or structured)
        query: Vec<String>,
        /// Semantic search across item titles and tags
        #[arg(long)]
        search: Option<String>,
        /// SPARQL-like filter (subset: SELECT/WHERE/FILTER/ORDER BY/LIMIT)
        #[arg(long)]
        sparql: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show query decomposition
        #[arg(long)]
        verbose: bool,
        /// Disable semantic search (text-only mode)
        #[arg(long)]
        no_semantic: bool,
        /// Limit results (default 20)
        #[arg(long, default_value = "20")]
        top: usize,
        /// Embedding provider: hash (default), ollama, subprocess
        #[arg(long)]
        embedding_provider: Option<String>,
    },

    /// Show board statistics
    Stats {
        /// Board to show stats for
        #[arg(long, default_value = "development")]
        board: String,
        /// Show weekly velocity (items/week for last N weeks, default 4)
        #[arg(long)]
        velocity: bool,
        /// Show burndown chart (items remaining over time)
        #[arg(long)]
        burndown: bool,
        /// Show per-agent throughput
        #[arg(long)]
        by_agent: bool,
        /// Start date for burndown (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Number of weeks for velocity (default 4)
        #[arg(long, default_value = "4")]
        weeks: u32,
    },

    /// Show recently completed items
    History {
        /// Show items completed this week
        #[arg(long)]
        week: bool,
        /// Show items completed this month (last 30 days)
        #[arg(long)]
        month: bool,
        /// Show items completed since date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Filter by assignee
        #[arg(long)]
        by_assignee: Option<String>,
    },

    /// Show roadmap (voyage-grouped, dependency-ordered view)
    Roadmap {
        /// Legacy flat view (backlog items only)
        #[arg(long)]
        flat: bool,
        /// Show only items with all dependencies met
        #[arg(long)]
        ready: bool,
        /// Roll up a campaign (CA-XXXX): its member voyages (partOf edges), each voyage's
        /// %-done, a program aggregate, and a cross-voyage critical path.
        #[arg(long)]
        campaign: Option<String>,
    },

    /// Show critical path (dependency chain with parallel tracks)
    CriticalPath,

    /// Show agent work assignments based on dependency readiness
    Worklist {
        /// Agent names (comma-separated). Default: every assignee on the board.
        #[arg(long, default_value = "")]
        agents: String,
        /// How many items deep per agent (default: 3)
        #[arg(long, default_value = "3")]
        depth: usize,
    },

    /// Show blocked items
    Blocked,

    /// Validate SHACL conformance of items against their shape rules
    Validate {
        /// Item ID to validate (e.g. EX-1234); omit to validate a whole board
        id: Option<String>,
        /// Also print suggested fix commands for each violation
        #[arg(long)]
        fix: bool,
        /// Validate all items on a specific board (development or research)
        #[arg(long)]
        board: Option<String>,
        /// Validate all items on all boards
        #[arg(long)]
        all: bool,
        /// Filter by status (e.g. backlog, in_progress)
        #[arg(long)]
        status: Option<String>,
    },

    /// Export board or item in various formats
    Export {
        /// Item ID (for single-item export) or omit for board export
        #[arg(long)]
        id: Option<String>,
        /// Export format: expedition-index, markdown, json, html, research-index, kanban-materialize, item (default: item)
        #[arg(short, long, default_value = "item")]
        format: String,
        /// Board for board-wide exports (development, research)
        #[arg(long, default_value = "development")]
        board: String,
        /// Filter by item type (expedition, paper, hypothesis, etc.)
        #[arg(long)]
        item_type: Option<String>,
        /// Filter by status (backlog, in_progress, done, etc.)
        #[arg(long)]
        status: Option<String>,
        /// Output file (default: stdout)
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Get the next available ID for a type
    NextId {
        /// Item type
        item_type: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Migrate markdown files to Arrow store
    Migrate {
        /// Dry run — show what would be migrated without saving
        #[arg(long)]
        dry_run: bool,
    },

    /// HDD (research board) commands
    Hdd {
        #[command(subcommand)]
        command: HddCommands,
    },

    /// Set priority rank for an item
    Rank {
        /// Item ID
        id: String,
        /// Rank number (lower = higher priority)
        rank: u32,
    },

    /// Suggest the next item to work on
    Next {
        /// Filter by assignee
        #[arg(long)]
        assignee: Option<String>,
    },

    /// Initialize arrow-kanban in the current directory
    Init {
        /// Theme preset: nautical (default), software, hdd (research)
        #[arg(long, default_value = "nautical")]
        theme: String,
    },

    /// List configured boards
    Boards,

    /// Show or generate body templates for item types
    Templates {
        /// Item type to generate template for (omit to list all types)
        item_type: Option<String>,
    },

    /// Start MCP (Model Context Protocol) server over stdio
    #[cfg(feature = "client")]
    McpServer {
        /// NATS server URL (default: nats://localhost:4222)
        #[arg(long, default_value = "nats://localhost:4222")]
        nats_url: String,
    },

    /// Materialize a single Arrow item and optionally its related items to arrow-kanban/
    MaterializeItem {
        /// Item ID to materialize (e.g. VY-1234)
        id: String,
        /// Scope of related items to materialize: work (dev items), research (HDD items), or all
        #[arg(long, default_value = "all")]
        scope: String,
        /// Maximum depth for recursive related-item traversal (default: 1 for direct only)
        #[arg(long, default_value = "1")]
        depth: u32,
        /// Output directory (default: ./arrow-kanban)
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Snapshot the kanban Arrow store to a timestamped backup directory.
    Backup {
        /// List available snapshots and exit (does not create a backup).
        #[arg(long)]
        list: bool,
        /// Show detailed info about a specific snapshot.
        #[arg(long)]
        inspect: Option<String>,
    },

    /// Restore the kanban Arrow store from a backup snapshot.
    Restore {
        /// Snapshot name to restore (e.g. snapshot-2026-04-07_055839).
        snapshot: String,
        /// Confirm restore — required because this overwrites the live store.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum HddCommands {
    /// Create a paper
    Paper {
        /// Paper title
        title: String,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Create a hypothesis linked to a paper
    Hypothesis {
        /// Hypothesis title
        title: String,
        /// Paper number to link to
        #[arg(long)]
        paper: u32,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Create an experiment linked to a hypothesis
    Experiment {
        /// Experiment title
        title: String,
        /// Hypothesis ID to link to (e.g., H130.1)
        #[arg(long)]
        hypothesis: String,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Create a measure
    Measure {
        /// Measure title
        title: String,
        /// Experiment ID to link to (optional)
        #[arg(long)]
        experiment: Option<String>,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Create an idea
    Idea {
        /// Idea title
        title: String,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Create a literature reference
    Literature {
        /// Literature title
        title: String,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Validate HDD research board integrity
    Validate,
    /// Show paper→hypothesis→experiment→measure registry
    Registry,
    /// Start a new run of an experiment
    Run {
        /// Experiment ID (e.g.)
        experiment_id: String,
        /// Agent running the experiment
        #[arg(long)]
        agent: Option<String>,
    },
    /// Show all runs for an experiment
    Status {
        /// Experiment ID (e.g.)
        experiment_id: String,
    },
    /// Complete an experiment run with results
    Complete {
        /// Experiment ID (e.g.)
        experiment_id: String,
        /// Run number to complete
        #[arg(long)]
        run: u32,
        /// Results as JSON string
        #[arg(long)]
        results: Option<String>,
    },
}

// ── Backup & Restore Commands ───────────────────────────────────────────────
use arrow_kanban::backup::{self, BackupConfig};

/// `arrow-kanban roadmap --campaign CA-XXXX` — the rollup a flat tag can never give. Members are
/// the voyages with a `partOf` edge to the campaign (data, not a hardcoded list); each member
/// voyage's `[done/total]` progress comes from the shared engine, summed into a program %-done,
/// with a cross-voyage critical path over ONLY the member set. Reuses `format_roadmap` — no fork.
/// A campaign with zero members renders empty (not an error), per the read-only-view contract.
fn render_campaign_roadmap(
    root: &std::path::Path,
    camp_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = persist::load_store(root)?;
    let rel_store = persist::load_relations(root)?;
    let members = rel_store.incoming_by_predicate(camp_id, "partOf");

    let mut items = critical_path::extract_items(&store.query_items(None, None, None, None));
    // resolve the typed `implements`/`spawns` expedition→voyage edges into `related`
    // so an expedition linked ONLY via `--relate implements:VY-X` counts toward its voyage.
    critical_path::fold_typed_voyage_memberships(&mut items, &rel_store);
    // The rollup rendering is a shared library fn — identical in the server's handle_roadmap, so
    // `arrow-kanban roadmap --campaign` renders the same in local and --server mode.
    print!(
        "{}",
        critical_path::format_campaign_roadmap(camp_id, &members, &items)
    );
    Ok(())
}

/// Run `arrow-kanban backup` — snapshot or list the kanban Arrow store.
fn run_backup_command(
    root: &std::path::Path,
    list: bool,
    inspect: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = BackupConfig::default();

    if list {
        let snapshots = backup::list_snapshots(&config)?;
        if snapshots.is_empty() {
            println!("No snapshots found in {:?}", config.destination);
        } else {
            println!("Snapshots in {:?}:", config.destination);
            for snap in &snapshots {
                println!(
                    "  {}  version={}  commits={}",
                    snap.name, snap.version, snap.commit_count
                );
            }
        }
        return Ok(());
    }

    if let Some(snapshot_name) = inspect {
        let snapshots = backup::list_snapshots(&config)?;
        let found = snapshots.iter().find(|s| s.name == snapshot_name);
        if let Some(snap) = found {
            println!("Snapshot: {}", snap.name);
            println!("  path: {:?}", snap.path);
            if let Some(created) = &snap.created_at {
                println!("  created: {}", created);
            }
            println!("  version: {}", snap.version);
            println!("  commits: {}", snap.commit_count);
        } else {
            return Err(format!("Snapshot '{}' not found", snapshot_name).into());
        }
        return Ok(());
    }

    // Default: create a new snapshot
    let snapshot_dir = backup::create_snapshot(&config, root)?;
    println!(
        "Backup created: {}",
        snapshot_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    );
    Ok(())
}

/// Run `arrow-kanban restore` — restore the kanban Arrow store from a snapshot.
fn run_restore_command(
    root: &std::path::Path,
    snapshot: &str,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = BackupConfig::default();
    let restored = arrow_kanban::backup::restore_snapshot(snapshot, &config, root, force)?;
    println!(
        "Restored from: {}",
        restored.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(())
}

/// Run the materialize-item command: materialize a single Arrow item and its related web.
///
/// Returns a summary string on success.
fn run_materialize_item_command(
    root: &std::path::Path,
    item_id: &str,
    scope: &str,
    depth: u32,
    out_dir: &std::path::Path,
) -> Result<String, Box<dyn std::error::Error>> {
    use arrow::array::ListArray;
    use std::collections::HashSet;

    let config_path = root.join(".arrow-kanban/config.yaml");
    let _config = if config_path.exists() {
        ConfigFile::from_path(&config_path)?
    } else {
        return Err("No .arrow-kanban/config.yaml found. Run 'arrow-kanban init' first.".into());
    };
    let store = persist::load_store(root)?;

    // Fetch the root item
    let root_batch = store.get_item(item_id)?;

    // Scope filter
    #[derive(Clone, Copy)]
    enum ScopeFilter {
        All,
        Work,
        Research,
    }
    let scope_filter = match scope {
        "work" => ScopeFilter::Work,
        "research" => ScopeFilter::Research,
        _ => ScopeFilter::All,
    };

    fn is_dev_type(t: &str) -> bool {
        matches!(
            t,
            "expedition" | "chore" | "voyage" | "hazard" | "signal" | "feature"
        )
    }
    fn is_research_type(t: &str) -> bool {
        matches!(
            t,
            "paper" | "hypothesis" | "experiment" | "measure" | "literature" | "idea"
        )
    }

    // (id, remaining_depth) — start with root at full depth
    let mut stack: Vec<(String, u32)> = vec![(item_id.to_string(), depth)];
    let mut visited: HashSet<String> = HashSet::new();
    let mut all_ids: Vec<String> = Vec::new();

    while let Some((id, remaining)) = stack.pop() {
        if !visited.insert(id.clone()) {
            continue; // already processed
        }
        all_ids.push(id.clone());

        if remaining == 0 {
            continue; // no more recursion
        }

        // Fetch this item to get its related/depends_on
        let batch = match store.get_item(&id) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let schema = batch.schema();
        let type_idx = schema.index_of("item_type").unwrap_or(0);
        let related_idx = schema.index_of("related").unwrap_or(0);
        let depends_idx = schema.index_of("depends_on").unwrap_or(0);

        let types_arr = arrow::array::as_string_array(batch.column(type_idx));
        let related_col = batch
            .column(related_idx)
            .as_any()
            .downcast_ref::<ListArray>();
        let depends_col = batch
            .column(depends_idx)
            .as_any()
            .downcast_ref::<ListArray>();

        let item_type = types_arr.value(0);

        // Scope filter check
        let type_ok = match scope_filter {
            ScopeFilter::All => true,
            ScopeFilter::Work => is_dev_type(item_type),
            ScopeFilter::Research => is_research_type(item_type),
        };
        if !type_ok {
            continue;
        }

        // Collect related IDs
        if let Some(rel_col) = related_col {
            for rel_id in export::list_values(rel_col, 0) {
                if !visited.contains(&rel_id) {
                    stack.push((rel_id, remaining - 1));
                }
            }
        }
        if let Some(dep_col) = depends_col {
            for dep_id in export::list_values(dep_col, 0) {
                if !visited.contains(&dep_id) {
                    stack.push((dep_id, remaining - 1));
                }
            }
        }
    }

    // Deduplicate
    all_ids.sort();
    all_ids.dedup();

    // Write each item
    let mut written: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for id in &all_ids {
        let batch = match store.get_item(id) {
            Ok(b) => b,
            Err(_) => {
                errors.push(format!("  ERROR: {} not found in Arrow store", id));
                continue;
            }
        };

        match export::kanban_materialize_single(&batch, out_dir) {
            Ok(path) => {
                written.push(format!("  {} → {}", id, path.display()));
            }
            Err(e) => {
                errors.push(format!("  ERROR writing {}: {}", id, e));
            }
        }
    }

    // Build manifest
    let mut manifest = format!(
        "Materialized {} items to {}\n\nRoot: {}\nScope: {}\nDepth: {}\n\nFiles written:\n",
        written.len(),
        out_dir.display(),
        item_id,
        scope,
        depth
    );
    for line in &written {
        manifest.push_str(line);
        manifest.push('\n');
    }
    if !errors.is_empty() {
        manifest.push_str("\nErrors:\n");
        for e in &errors {
            manifest.push_str(e);
            manifest.push('\n');
        }
    }

    // Write manifest file alongside root item
    let root_folder = export::kanban_materialize_single(&root_batch, out_dir)
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    if let Some(folder) = root_folder {
        let manifest_path = folder.join("materialize-manifest.txt");
        std::fs::write(&manifest_path, &manifest)?;
        manifest.push_str(&format!("\nManifest: {}", manifest_path.display()));
    }

    Ok(manifest)
}

// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    let root = cli.root.canonicalize().unwrap_or(cli.root);

    // MCP server runs locally — intercept before NATS client dispatch
    #[cfg(feature = "client")]
    if let Commands::McpServer { ref nats_url } = cli.command {
        let nats = cli.server.as_deref().unwrap_or(nats_url);
        match arrow_kanban::mcp_server::McpServer::new(nats) {
            Ok(server) => {
                eprintln!("arrow-kanban MCP server started (stdio transport)");
                if let Err(e) = server.run() {
                    eprintln!("MCP server error: {e}");
                    process::exit(1);
                }
                return;
            }
            Err(e) => {
                eprintln!("Failed to start MCP server: {e}");
                process::exit(1);
            }
        }
    }

    // MaterializeItem command — materialize a single Arrow item + related web.
    if let Commands::MaterializeItem {
        id,
        scope,
        depth,
        output,
    } = &cli.command
    {
        let root_path = root.clone();
        let out_dir = output
            .clone()
            .unwrap_or_else(|| PathBuf::from("arrow-kanban"));
        match run_materialize_item_command(&root_path, id, scope, *depth, &out_dir) {
            Ok(manifest) => {
                println!("{}", manifest);
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    // Backup command runs locally — snapshot or list Arrow store.
    if let Commands::Backup { list, inspect } = &cli.command {
        if let Err(e) = run_backup_command(&root, *list, inspect.clone()) {
            eprintln!("Error: {e}");
            process::exit(1);
        }
        return;
    }

    // Restore command runs locally — restore from snapshot.
    if let Commands::Restore { snapshot, force } = &cli.command {
        if let Err(e) = run_restore_command(&root, snapshot, *force) {
            eprintln!("Error: {e}");
            process::exit(1);
        }
        return;
    }

    // Client mode: send commands to NATS server
    #[cfg(feature = "client")]
    if let Some(server_url) = &cli.server {
        match run_client(server_url, &cli.command) {
            Ok(()) => return,
            Err(e) => {
                // When --server is specified, NEVER fall back to local mode.
                // The Arrow store on NATS is the single source of truth.
                // Local fallback would serve stale data, so we refuse instead.
                eprintln!("Error: {e}");
                process::exit(1);
            }
        }
    }

    if let Err(e) = run(root, cli.command) {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn run(root: PathBuf, command: Commands) -> Result<(), Box<dyn std::error::Error>> {
    // Handle init before config loading — init creates the config
    if let Commands::Init { ref theme } = command {
        return run_init(&root, theme);
    }

    // Load config
    let config_path = root.join(".arrow-kanban/config.yaml");
    let config = if config_path.exists() {
        ConfigFile::from_path(&config_path)?
    } else {
        return Err("No .arrow-kanban/config.yaml found. Run 'arrow-kanban init' first.".into());
    };

    // Load store
    let mut store = persist::load_store(&root)?;

    match command {
        Commands::Create {
            item_type,
            title,
            priority,
            assign,
            tags,
            body,
            body_file,
            body_stdin,
            template,
            relate,
            push,
        } => {
            let it = ItemType::from_str_loose(&item_type)
                .ok_or_else(|| format!("Unknown item type: {item_type}"))?;

            let tag_list = tags
                .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default();

            // Resolve body content from --body, --body-file, --body-stdin, or --template
            let body_content = if let Some(b) = body {
                Some(b)
            } else if let Some(path) = body_file {
                Some(
                    std::fs::read_to_string(&path)
                        .map_err(|e| format!("Failed to read body file '{}': {}", path, e))?,
                )
            } else if body_stdin {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                if buf.is_empty() { None } else { Some(buf) }
            } else if template.is_some() {
                Some(generate_template(&item_type, &title, &root))
            } else {
                None
            };

            // typed relationships at create time. Validate EVERY edge before
            // creating anything — a half-related item is worse than a refused one.
            let edges = parse_relate_specs(&relate, it.as_str(), None)?;

            // related/dependsOn also project into the flat columns, so roadmap /
            // critical-path / worklist / show keep reading exactly what they read today.
            let mut related_flat: Vec<String> = Vec::new();
            let mut depends_flat: Vec<String> = Vec::new();
            for (pred, target) in &edges {
                match arrow_kanban::relation_vocab::flat_column_for(pred) {
                    Some("related") => related_flat.push(target.clone()),
                    Some("depends_on") => depends_flat.push(target.clone()),
                    _ => {}
                }
            }

            let id = store.create_item(&CreateItemInput {
                title: title.clone(),
                item_type: it,
                priority,
                assignee: assign,
                tags: tag_list,
                related: related_flat,
                depends_on: depends_flat,
                body: body_content,
            })?;

            if !edges.is_empty() {
                let mut rel_store = persist::load_relations(&root)?;
                for (pred, target) in &edges {
                    rel_store.add_relation(&id, target, pred)?;
                }
                persist::save_relations(&root, &rel_store)?;
            }

            persist::save_store(&root, &store)?;
            println!("Created {id}: {title}");
            for (pred, target) in &edges {
                println!("  {id} -{pred}-> {target}");
            }

            if push {
                // Graph-native commit (queryable audit trail via parquet_store)
                if let Ok(data_dir) = persist::data_dir(&root) {
                    let mut commits_table = restore_commits(&data_dir)
                        .ok()
                        .flatten()
                        .unwrap_or_default();

                    let last_id = commits_table.all().last().map(|c| c.commit_id.clone());
                    let commit = Commit {
                        commit_id: uuid::Uuid::new_v4().to_string(),
                        parent_ids: last_id.into_iter().collect(),
                        timestamp_ms: chrono::Utc::now().timestamp_millis(),
                        message: format!("Create {id}: {title}"),
                        author: "arrow-kanban".to_string(),
                    };
                    commits_table.append(commit);
                    if let Err(e) = persist_commits(&commits_table, &data_dir) {
                        eprintln!("  Warning: failed to persist graph-native commit: {e}");
                    }
                }

                // Remote sync via shell git (remote operations are out of scope
                // for the in-crate parquet commit log)
                let data_dir = root.join(".arrow-kanban");
                let status = std::process::Command::new("git")
                    .current_dir(&root)
                    .args(["add", &data_dir.to_string_lossy()])
                    .status();
                if status.is_ok() {
                    let msg = format!("Create {id}: {title}");
                    let commit_status = std::process::Command::new("git")
                        .current_dir(&root)
                        .args(["commit", "-m", &msg])
                        .status();
                    if commit_status.is_ok() {
                        // Try push with 3 retries (rebase on conflict)
                        for attempt in 1..=3 {
                            let push_result = std::process::Command::new("git")
                                .current_dir(&root)
                                .args(["push", "origin", "HEAD"])
                                .status();
                            match push_result {
                                Ok(s) if s.success() => {
                                    println!("  (committed and pushed to remote)");
                                    break;
                                }
                                _ if attempt < 3 => {
                                    let _ = std::process::Command::new("git")
                                        .current_dir(&root)
                                        .args(["pull", "--rebase", "origin", "main"])
                                        .status();
                                }
                                _ => {
                                    eprintln!("  Warning: push failed after 3 attempts");
                                }
                            }
                        }
                    }
                }
            }
        }

        Commands::Move {
            id,
            status,
            assign,
            force,
            resolution,
            closed_by,
        } => {
            // Get current item to check board
            let item = store.get_item(&id)?;
            let current_status = item
                .column(arrow_kanban::schema::items_col::STATUS)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("status")
                .value(0);
            let item_type_str = item
                .column(arrow_kanban::schema::items_col::ITEM_TYPE)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("type")
                .value(0);
            let board_name = item
                .column(arrow_kanban::schema::items_col::BOARD)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("board")
                .value(0);

            let board = config.board(board_name)?;

            // Validate resolution before the move
            state_machine::validate_resolution(resolution.as_deref(), &status)?;

            // Validate transition (skip if forced)
            if !force {
                state_machine::validate_transition_for_type(
                    board,
                    current_status,
                    &status,
                    Some(item_type_str),
                )?;

                // Check WIP limits
                let exempt: Vec<&str> = board.wip_exempt_types.iter().map(|s| s.as_str()).collect();
                let count = store.count_at_status(&status, &exempt);
                state_machine::check_wip_limit(board, &status, count, item_type_str)?;
            }

            // Atomic exclusive claim: reject a cross-agent claim on an already
            // in_progress item (without --force) — same guard as the server's single-writer path.
            if status == "in_progress"
                && let Some(assignee) = &assign
            {
                store.check_exclusive_claim(&id, assignee, force)?;
            }

            let old = store.update_status(&id, &status, None, force, None)?;

            // Handle assignee update
            if let Some(assignee) = &assign {
                store.update_assignee(&id, Some(assignee))?;
            }

            // Set resolution if provided
            if let Some(ref res) = resolution {
                store.update_resolution(&id, Some(res))?;
            }

            // Set closed_by if provided
            if let Some(ref cb) = closed_by {
                store.update_closed_by(&id, Some(cb))?;
            }

            persist::save_store(&root, &store)?;

            let mut msg = if force {
                format!("Moved {id} from {old} to {status} (forced)")
            } else {
                format!("Moved {id} from {old} to {status}")
            };
            if let Some(ref res) = resolution {
                msg.push_str(&format!(" [resolution: {res}]"));
            }
            println!("{msg}");
        }

        Commands::Ratify { phase } => {
            let report = store.ratify_phase(&phase)?;
            if report.remaining_pending != 0 {
                return Err(format!(
                    "ratify {phase}: {} item(s) still pending-ratification after strip (partial \
                     ratification — NOT sprint-started)",
                    report.remaining_pending
                )
                .into());
            }
            persist::save_store(&root, &store)?;
            println!(
                "Ratified {phase}: stripped pending-ratification from {} item(s); started {} voyage(s){}",
                report.stripped.len(),
                report.voyages_started.len(),
                if report.voyages_started.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", report.voyages_started.join(", "))
                }
            );
        }

        Commands::Update {
            id,
            title,
            priority,
            assign,
            tags,
            body,
            body_file,
            related,
            depends_on,
            relate,
            unrelate,
        } => {
            // Verify item exists
            let _ = store.get_item(&id)?;

            let mut updated = Vec::new();

            if let Some(t) = &title {
                store.update_title(&id, t)?;
                updated.push("title");
            }
            if let Some(p) = &priority {
                store.update_priority(&id, Some(p))?;
                updated.push("priority");
            }
            if let Some(a) = &assign {
                store.update_assignee(&id, Some(a))?;
                updated.push("assignee");
            }
            if let Some(t) = &tags {
                let tag_list: Vec<String> = t.split(',').map(|s| s.trim().to_string()).collect();
                store.update_tags(&id, &tag_list)?;
                updated.push("tags");
            }
            if let Some(b) = &body {
                store.update_body(&id, Some(b))?;
                updated.push("body");
            }
            if let Some(path) = &body_file {
                let content = std::fs::read_to_string(path)?;
                store.update_body(&id, Some(content.trim()))?;
                updated.push("body");
            }
            if let Some(r) = &related {
                let list: Vec<String> = r.split(',').map(|s| s.trim().to_string()).collect();
                store.update_related(&id, &list)?;
                updated.push("related");
            }
            if let Some(d) = &depends_on {
                let list: Vec<String> = d.split(',').map(|s| s.trim().to_string()).collect();
                store.update_depends_on(&id, &list)?;
                updated.push("depends_on");
            }

            // typed relationships on EDIT — additive/subtractive, unlike
            // --related/--depends-on which replace. Validated before any mutation.
            if !relate.is_empty() || !unrelate.is_empty() {
                let source_type = arrow_kanban::relation_vocab::type_from_id(&id).unwrap_or("");
                let add = parse_relate_specs(&relate, source_type, Some(&id))?;
                let del = parse_relate_specs(&unrelate, source_type, Some(&id))?;

                let mut rel_store = persist::load_relations(&root)?;
                for (pred, target) in &add {
                    rel_store.add_relation(&id, target, pred)?;
                    // Keep the flat projection in step, or a related/dependsOn edge added
                    // on EDIT is invisible to roadmap / critical-path / worklist / show.
                    if let Some(col) = flat_col_for(pred) {
                        store.add_to_list_field(&id, col, target)?;
                    }
                    println!("  + {id} -{pred}-> {target}");
                }
                for (pred, target) in &del {
                    // Remove BOTH projections INDEPENDENTLY. A flat-only
                    // edge (created by --depends-on/--related, no typed edge) makes
                    // remove_relation error — propagating that with `?` used to abort the
                    // whole update, and gating the flat removal behind it left the edge stuck.
                    let typed_removed = rel_store.remove_relation(&id, target, pred).is_ok();
                    let flat_removed = match flat_col_for(pred) {
                        Some(col) => store.remove_from_list_field(&id, col, target)?,
                        None => false,
                    };
                    if typed_removed || flat_removed {
                        println!("  - {id} -{pred}-> {target}");
                    } else {
                        println!("  (no {id} -{pred}-> {target} edge to remove)");
                    }
                }
                persist::save_relations(&root, &rel_store)?;
                if !add.is_empty() {
                    updated.push("relationships_added");
                }
                if !del.is_empty() {
                    updated.push("relationships_removed");
                }
            }

            if updated.is_empty() {
                println!(
                    "No fields specified to update. Use --title, --priority, --assign, --tags, --body, --body-file, --related, --depends-on, --relate, or --unrelate."
                );
            } else {
                persist::save_store(&root, &store)?;
                println!("Updated {id}: {}", updated.join(", "));
            }
        }

        Commands::Comment { id, text } => {
            // Verify item exists
            let _ = store.get_item(&id)?;

            // Record comment as a run entry
            store.add_comment(&id, &text, None)?;
            persist::save_store(&root, &store)?;
            println!("Comment added to {id}");
        }

        Commands::List {
            status,
            board,
            item_type,
            assignee,
            resolution,
            priority,
            tag,
            ready,
        } => {
            let mut results = store.query_items(
                status.as_deref(),
                item_type.as_deref(),
                board.as_deref(),
                assignee.as_deref(),
            );

            // Post-filter by resolution if specified
            if let Some(ref res_filter) = resolution {
                results.retain(|batch| {
                    let res_col = batch
                        .column(arrow_kanban::schema::items_col::RESOLUTION)
                        .as_any()
                        .downcast_ref::<arrow::array::StringArray>()
                        .expect("resolution column");
                    !res_col.is_null(0) && res_col.value(0) == res_filter.as_str()
                });
            }

            // Post-filter by priority if specified
            if let Some(ref pri_filter) = priority {
                results.retain(|batch| {
                    let pri_col = batch
                        .column(arrow_kanban::schema::items_col::PRIORITY)
                        .as_any()
                        .downcast_ref::<arrow::array::StringArray>()
                        .expect("priority column");
                    !pri_col.is_null(0) && pri_col.value(0) == pri_filter.as_str()
                });
            }

            // Post-filter by tags if specified (AND logic: all tags must match)
            if !tag.is_empty() {
                results.retain(|batch| {
                    let tags_col = batch
                        .column(arrow_kanban::schema::items_col::TAGS)
                        .as_any()
                        .downcast_ref::<arrow::array::ListArray>()
                        .expect("tags column");
                    (0..batch.num_rows()).any(|i| {
                        if tags_col.is_null(i) {
                            return false;
                        }
                        let item_tags = tags_col.value(i);
                        let tag_arr = item_tags
                            .as_any()
                            .downcast_ref::<arrow::array::StringArray>()
                            .expect("tag values");
                        let item_tag_set: std::collections::HashSet<&str> =
                            (0..tag_arr.len()).map(|j| tag_arr.value(j)).collect();
                        tag.iter().all(|t| item_tag_set.contains(t.as_str()))
                    })
                });
            }

            // Post-filter: only items with all dependencies met
            if ready {
                let all_batches = store.query_items(None, None, None, None);
                let items = critical_path::extract_items(&all_batches);
                let cp = critical_path::compute_critical_path(&items)
                    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                let ready_set: std::collections::HashSet<&str> =
                    cp.ready.iter().map(|s| s.as_str()).collect();
                results.retain(|batch| {
                    let ids = batch
                        .column(arrow_kanban::schema::items_col::ID)
                        .as_any()
                        .downcast_ref::<arrow::array::StringArray>()
                        .expect("id");
                    (0..batch.num_rows()).any(|i| ready_set.contains(ids.value(i)))
                });
            }

            print!("{}", display::format_item_table(&results));
        }

        Commands::Board { board } => {
            let board_config = config.board(&board)?;
            let results = store.query_items(None, None, Some(&board), None);
            print!(
                "{}",
                display::format_board_view(&results, &board_config.states)
            );
        }

        Commands::Show {
            id,
            format,
            relations,
        } => {
            let item = store.get_item(&id)?;
            match format.as_str() {
                "md" => {
                    // Full markdown output (frontmatter + body)
                    print!("{}", export::item_to_markdown(&item, 0));
                }
                "json" => {
                    // JSON output including body
                    let batches = vec![item];
                    print!("{}", export::export_json(&batches));
                }
                _ => {
                    // Default: metadata + body
                    print!("{}", display::format_item_detail(&item));
                }
            }

            if relations {
                print!("{}", format_item_relations(&id, &store));
            }
        }

        Commands::Query {
            query: words,
            search,
            sparql,
            json,
            verbose,
            no_semantic,
            top,
            embedding_provider,
        } => {
            // Handle --search flag (semantic search with substring fallback)
            if let Some(ref search_text) = search {
                let all = store.query_items(None, None, None, None);

                // Substring-only search (the open core has no semantic backend).
                let search_lower = search_text.to_lowercase();
                let matched: Vec<arrow::array::RecordBatch> = all
                    .into_iter()
                    .filter(|batch| {
                        let titles = batch
                            .column(arrow_kanban::schema::items_col::TITLE)
                            .as_any()
                            .downcast_ref::<arrow::array::StringArray>()
                            .expect("title");
                        (0..batch.num_rows())
                            .any(|i| titles.value(i).to_lowercase().contains(&search_lower))
                    })
                    .collect();

                // Also scan external files in arrow-kanban/ folder
                let arrow_ids: std::collections::HashSet<&str> = matched
                    .iter()
                    .flat_map(|batch| {
                        let ids = batch
                            .column(arrow_kanban::schema::items_col::ID)
                            .as_any()
                            .downcast_ref::<arrow::array::StringArray>()
                            .expect("id");
                        (0..batch.num_rows())
                            .map(|i| ids.value(i))
                            .collect::<Vec<_>>()
                    })
                    .collect();
                let file_results = arrow_kanban::file_index::search_files(search_text, &arrow_ids);

                print!("{}", display::format_item_table(&matched));
                if !file_results.is_empty() {
                    println!("\n[External files in arrow-kanban/]");
                    for fr in &file_results {
                        println!("  {} | {} | {}", fr.id, fr.title, fr.item_type);
                    }
                }
                return Ok(());
            }

            // Handle --sparql flag (expanded parser)
            if let Some(ref sparql_query) = sparql {
                let parsed = query::parse_sparql(sparql_query);
                let all = store.query_items(None, None, None, None);
                let rows = query::execute_sparql(&all, &parsed);

                if json {
                    // JSON output for SPARQL results
                    let items: Vec<String> = rows
                        .iter()
                        .map(|row| {
                            let pairs: Vec<String> = row
                                .iter()
                                .map(|(k, v)| {
                                    format!(r#""{}": "{}""#, k.trim_start_matches('?'), v)
                                })
                                .collect();
                            format!("  {{{}}}", pairs.join(", "))
                        })
                        .collect();
                    println!("[\n{}\n]", items.join(",\n"));
                } else {
                    print!(
                        "{}",
                        query::format_sparql_results(&rows, &parsed.select_vars)
                    );
                }
                return Ok(());
            }

            // Default: hybrid query (NL + structured + semantic)
            let query_str = words.join(" ");

            let filters = query::parse_nl_query(&query_str);

            if verbose {
                print!("{}", query::format_query_decomposition(&filters));
            }

            // If we have an ID pattern, just show that item
            if let Some(id) = &filters.id_pattern {
                match store.get_item(id) {
                    Ok(item) => {
                        print!("{}", display::format_item_detail(&item));
                        return Ok(());
                    }
                    Err(_) => {
                        println!("Item not found: {id}");
                        return Ok(());
                    }
                }
            }

            // Handle relation queries (blockers, dependencies)
            if let Some(ref rq) = filters.relation_query {
                let all_items = store.query_items(None, None, None, None);
                let adj = build_dependency_adjacency(&all_items);

                match rq {
                    query::RelationQuery::BlockersOf(target) => {
                        // Find what blocks this item (reverse: who does target depend on?)
                        let chain = arrow_kanban::traversal::bfs_with_adjacency(target, &adj, 10);
                        if chain.is_empty() {
                            println!("{target} has no blockers.");
                        } else {
                            let status_map = build_status_map(&store);
                            println!("Blockers of {target}:");
                            for node in &chain {
                                let status =
                                    status_map.get(&node.id).map(|s| s.as_str()).unwrap_or("?");
                                let indent = "  ".repeat(node.depth);
                                println!("  {indent}→ {} [{}]", node.id, status);
                            }
                        }
                    }
                    query::RelationQuery::DependenciesOf(target) => {
                        let chain = arrow_kanban::traversal::bfs_with_adjacency(target, &adj, 10);
                        if chain.is_empty() {
                            println!("{target} has no dependencies.");
                        } else {
                            println!("Dependencies of {target}:");
                            for node in &chain {
                                let indent = "  ".repeat(node.depth);
                                println!("  {indent}→ {} (depth {})", node.id, node.depth);
                            }
                        }
                    }
                }
                return Ok(());
            }

            // Handle ID range filter (e.g., "expeditions above 3100")
            if let Some(above) = filters.id_above {
                let all_items = store.query_items(
                    filters.status.as_deref(),
                    filters.item_type.as_deref(),
                    filters.board.as_deref(),
                    filters.assignee.as_deref(),
                );
                let filtered: Vec<arrow::array::RecordBatch> = all_items
                    .into_iter()
                    .filter(|batch| {
                        let Some(ids) = batch
                            .column(arrow_kanban::schema::items_col::ID)
                            .as_any()
                            .downcast_ref::<arrow::array::StringArray>()
                        else {
                            return false;
                        };
                        let below = filters.id_below;
                        (0..batch.num_rows()).any(|i| {
                            // Extract numeric suffix from ID (e.g., "EX-1234" → 1234)
                            let id = ids.value(i);
                            id.rsplit('-')
                                .next()
                                .and_then(|n| n.parse::<u32>().ok())
                                .is_some_and(|n| n >= above && below.is_none_or(|b| n <= b))
                        })
                    })
                    .collect();
                print!("{}", display::format_item_table(&filtered));
                return Ok(());
            }

            // Rank results. Semantic embedding ranking is an optional backend
            // (feature `semantic-search`); the open FOSS core ranks by text/substring.
            let all = store.query_items(None, None, None, None);
            let results = {
                let _ = (&no_semantic, &embedding_provider);
                query::hybrid_query(&all, &query_str, top)
            };

            if json {
                print!("{}", query::format_ranked_results_json(&results));
            } else if filters.text_query.is_some() {
                // Show ranked output when a text query drove semantic scoring
                print!("{}", query::format_ranked_results(&results));
            } else {
                // Fall back to standard table for pure structural queries
                let filtered = store.query_items(
                    filters.status.as_deref(),
                    filters.item_type.as_deref(),
                    filters.board.as_deref(),
                    filters.assignee.as_deref(),
                );
                let text_filtered: Vec<arrow::array::RecordBatch> =
                    if let Some(text) = &filters.text_query {
                        filtered
                            .into_iter()
                            .filter(|batch| {
                                let titles = batch
                                    .column(arrow_kanban::schema::items_col::TITLE)
                                    .as_any()
                                    .downcast_ref::<arrow::array::StringArray>()
                                    .expect("title");
                                (0..batch.num_rows())
                                    .any(|i| query::text_matches(titles.value(i), text))
                            })
                            .collect()
                    } else {
                        filtered
                    };
                print!("{}", display::format_item_table(&text_filtered));
            }
        }

        Commands::Stats {
            board,
            velocity,
            burndown,
            by_agent,
            since,
            weeks,
        } => {
            if velocity {
                let vel = arrow_kanban::stats::compute_velocity(store.runs_batches(), weeks);
                print!("{}", arrow_kanban::stats::format_velocity(&vel));
            } else if burndown {
                let since_ms = if let Some(ref date) = since {
                    arrow_kanban::stats::parse_date_to_ms(date)
                        .ok_or_else(|| format!("Invalid date: {date} (use YYYY-MM-DD)"))?
                } else {
                    // Default: last 4 weeks
                    chrono::Utc::now().timestamp_millis() - (4 * 7 * 24 * 60 * 60 * 1000)
                };
                let points = arrow_kanban::stats::compute_burndown(
                    store.items_batches(),
                    store.runs_batches(),
                    since_ms,
                );
                print!("{}", arrow_kanban::stats::format_burndown(&points));
            } else if by_agent {
                let stats = arrow_kanban::stats::compute_agent_stats(store.runs_batches());
                print!("{}", arrow_kanban::stats::format_agent_stats(&stats));
            } else {
                // Default: existing status/type summary
                let board_config = config.board(&board)?;
                let results = store.query_items(None, None, Some(&board), None);
                print!("{}", display::format_stats(&results, &board_config.states));
            }
        }

        Commands::History {
            week,
            month,
            since,
            by_assignee,
        } => {
            let since_ms = if let Some(ref date) = since {
                arrow_kanban::stats::parse_date_to_ms(date)
                    .ok_or_else(|| format!("Invalid date: {date} (use YYYY-MM-DD)"))?
            } else if month {
                chrono::Utc::now().timestamp_millis() - (30 * 24 * 60 * 60 * 1000)
            } else if week {
                chrono::Utc::now().timestamp_millis() - (7 * 24 * 60 * 60 * 1000)
            } else {
                // Default: show all done items (use old behavior if no flags)
                0
            };

            if week || month || since.is_some() || by_assignee.is_some() {
                let entries = arrow_kanban::stats::filter_history(
                    store.items_batches(),
                    store.runs_batches(),
                    since_ms,
                    by_assignee.as_deref(),
                );
                print!("{}", arrow_kanban::stats::format_history_entries(&entries));
            } else {
                // Original behavior: show all done items
                let default_board = config.default_board()?;
                let done_status = default_board
                    .states
                    .last()
                    .map(|s| s.as_str())
                    .unwrap_or("done");
                print!(
                    "{}",
                    display::format_history(store.items_batches(), done_status)
                );
            }
        }

        Commands::Roadmap {
            flat,
            ready,
            campaign,
        } => {
            if let Some(camp_id) = campaign {
                render_campaign_roadmap(&root, &camp_id)?;
            } else {
                let all_batches = store.query_items(None, None, None, None);
                if all_batches.is_empty() {
                    println!("No items found.");
                } else if flat {
                    // Legacy flat view — backlog only, sorted by manual rank then by priority-string fallback.
                    let results = store.query_items(Some("backlog"), None, None, None);
                    if results.is_empty() {
                        println!("No backlog items.");
                    } else {
                        let mut sorted = results;
                        sorted.sort_by(|a, b| {
                            let get_key = |batch: &arrow::array::RecordBatch| -> (u8, i32, i32) {
                                let prios = batch
                                    .column(arrow_kanban::schema::items_col::PRIORITY)
                                    .as_any()
                                    .downcast_ref::<arrow::array::StringArray>()
                                    .expect("priority");
                                let priority = if prios.is_null(0) {
                                    99
                                } else {
                                    match prios.value(0) {
                                        "critical" => 0,
                                        "high" => 1,
                                        "medium" => 2,
                                        "low" => 3,
                                        _ => 99,
                                    }
                                };
                                let ranks = batch
                                    .column(arrow_kanban::schema::items_col::PRIORITY_RANK)
                                    .as_any()
                                    .downcast_ref::<arrow::array::Int32Array>()
                                    .expect("priority_rank");
                                if ranks.is_null(0) {
                                    (1, 0, priority)
                                } else {
                                    (0, ranks.value(0), priority)
                                }
                            };
                            get_key(a).cmp(&get_key(b))
                        });
                        println!("Roadmap (flat, ranked by manual rank then priority):");
                        print!("{}", display::format_item_table(&sorted));
                    }
                } else {
                    // Voyage-grouped, dependency-ordered view
                    let mut items = critical_path::extract_items(&all_batches);
                    // fold the typed `implements`/`spawns` expedition→voyage edges into
                    // `related` so an `implements`-only expedition groups under its voyage.
                    let rel_store = persist::load_relations(&root)?;
                    critical_path::fold_typed_voyage_memberships(&mut items, &rel_store);
                    let cp = critical_path::compute_critical_path(&items)
                        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                    let (groups, orphans) = critical_path::group_by_voyage(&items);

                    if ready {
                        // Show only ready items
                        println!("Ready Items ({}):", cp.ready.len());
                        let ready_batches: Vec<_> = cp
                            .ready
                            .iter()
                            .filter_map(|id| store.get_item(id).ok())
                            .collect();
                        if ready_batches.is_empty() {
                            println!("  (none)");
                        } else {
                            print!("{}", display::format_item_table(&ready_batches));
                        }
                    } else {
                        print!(
                            "{}",
                            critical_path::format_roadmap(&items, &groups, &orphans, &cp)
                        );
                    }
                }
            }
        }

        Commands::CriticalPath => {
            let all_batches = store.query_items(None, None, None, None);
            if all_batches.is_empty() {
                println!("No items found.");
            } else {
                let items = critical_path::extract_items(&all_batches);
                let cp = critical_path::compute_critical_path(&items)
                    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                print!("{}", critical_path::format_critical_path(&items, &cp));
            }
        }

        Commands::Worklist { agents, depth } => {
            let all_batches = store.query_items(None, None, None, None);
            if all_batches.is_empty() {
                println!("No items found.");
            } else {
                let items = critical_path::extract_items(&all_batches);
                let cp = critical_path::compute_critical_path(&items)
                    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                let agent_list: Vec<String> = if agents.trim().is_empty() {
                    critical_path::agents_from_items(&items)
                } else {
                    agents.split(',').map(|s| s.trim().to_string()).collect()
                };
                let worklist = critical_path::generate_worklist(&items, &cp, &agent_list, depth);
                print!("{}", critical_path::format_worklist(&worklist));
            }
        }

        Commands::Blocked => {
            let blocked = find_blocked_items(&store);
            if blocked.is_empty() {
                println!("No blocked items.");
            } else {
                println!("Blocked Items ({}):", blocked.len());
                print!("{}", display::format_item_table(&blocked));

                // Show blocker chains for each blocked item
                println!("\nBlocker Chains:");
                println!("{}", "─".repeat(60));
                let all_items = store.query_items(None, None, None, None);
                let adj = build_dependency_adjacency(&all_items);
                let status_map = build_status_map(&store);

                for batch in &blocked {
                    if let Some(ids) = batch
                        .column(arrow_kanban::schema::items_col::ID)
                        .as_any()
                        .downcast_ref::<arrow::array::StringArray>()
                    {
                        for i in 0..batch.num_rows() {
                            let id = ids.value(i);
                            let chain = arrow_kanban::traversal::bfs_with_adjacency(id, &adj, 10);
                            if !chain.is_empty() {
                                print!("  {id}");
                                for node in &chain {
                                    let status =
                                        status_map.get(&node.id).map(|s| s.as_str()).unwrap_or("?");
                                    print!(" → {} [{}]", node.id, status);
                                }
                                println!();
                            }
                        }
                    }
                }
            }
        }

        Commands::Validate {
            id,
            fix,
            board,
            all,
            status,
        } => {
            use arrow_kanban::validate;

            if let Some(item_id) = id {
                // Single-item validation
                let batch = store.get_item(&item_id)?;
                let report = validate::validate_item(&batch);
                println!("{}", validate::format_report(&report, fix));
                if !report.is_conformant() {
                    process::exit(1);
                }
            } else {
                // Board or all-boards validation
                let board_filter = if all {
                    None
                } else {
                    Some(board.as_deref().unwrap_or("development"))
                };

                let batches = store.query_items(status.as_deref(), None, board_filter, None);
                if batches.is_empty() {
                    println!("No items found.");
                } else {
                    let reports = validate::validate_all(&batches);

                    // Print per-item reports for items with violations
                    let mut any_violations = false;
                    for report in &reports {
                        if !report.violations.is_empty() {
                            println!("{}", validate::format_report(report, fix));
                            println!();
                            any_violations = true;
                        }
                    }

                    println!("{}", validate::format_board_summary(&reports));

                    if any_violations {
                        process::exit(1);
                    }
                }
            }
        }

        Commands::Export {
            id,
            format,
            board,
            item_type,
            status,
            output,
        } => {
            // Build query filters from optional args
            let type_filter = item_type
                .as_ref()
                .and_then(|t| arrow_kanban::item_type::ItemType::from_str_loose(t))
                .map(|t| t.as_str().to_string());

            let status_filter = status.clone();

            let content = match format.as_str() {
                "expedition-index" => {
                    let batches = store.query_items(
                        status_filter.as_deref(),
                        type_filter.as_deref(),
                        Some(&board),
                        None,
                    );
                    export::export_board_index(&batches, &board, None)
                }
                "json" => {
                    let batches = store.query_items(
                        status_filter.as_deref(),
                        type_filter.as_deref(),
                        Some(&board),
                        None,
                    );
                    export::export_json(&batches)
                }
                "markdown" => {
                    let batches = store.query_items(
                        status_filter.as_deref(),
                        type_filter.as_deref(),
                        Some(&board),
                        None,
                    );
                    export::export_markdown_table(&batches)
                }
                "html" => {
                    let batches = store.query_items(
                        status_filter.as_deref(),
                        type_filter.as_deref(),
                        Some(&board),
                        None,
                    );
                    // Compute burndown for the last 8 weeks
                    let since_ms =
                        chrono::Utc::now().timestamp_millis() - (8 * 7 * 24 * 60 * 60 * 1000i64);
                    let burndown = arrow_kanban::stats::compute_burndown(
                        store.items_batches(),
                        store.runs_batches(),
                        since_ms,
                    );
                    let points = if burndown.is_empty() {
                        None
                    } else {
                        Some(burndown.as_slice())
                    };
                    export::export_board_html(&batches, &board, None, points)
                }
                "research-index" => {
                    let rel_store = persist::load_relations(&root)?;
                    let chains = arrow_kanban::build_registry(&store, &rel_store);
                    export::export_research_index_html(&chains)
                }
                "kanban-materialize" => {
                    let out_dir = output
                        .as_ref()
                        .map(PathBuf::from)
                        .ok_or("--output required for kanban-materialize format")?;
                    let batches = store.query_items(
                        status_filter.as_deref(),
                        type_filter.as_deref(),
                        Some(&board),
                        None,
                    );
                    let count = export::kanban_materialize(&batches, &board, &out_dir)?;
                    // kanban-materialize always prints to stdout; --output is the base directory
                    println!("Materialized {} items to {}", count, out_dir.display());
                    return Ok(());
                }
                _ => {
                    if let Some(item_id) = &id {
                        let item = store.get_item(item_id)?;
                        export::item_to_markdown(&item, 0)
                    } else {
                        return Err("--id required for item export format".into());
                    }
                }
            };

            if let Some(out_path) = &output {
                std::fs::write(out_path, &content)?;
                println!("Exported to {out_path}");
            } else {
                print!("{content}");
            }
        }

        Commands::Migrate { dry_run } => {
            let result = arrow_kanban::migrate::migrate_boards(&root, &config)?;
            println!("{}", result.summary());

            if dry_run {
                println!("Dry run — no changes saved.");
            } else {
                let (migrated_store, rel_store) = result.into_stores()?;
                persist::save_store(&root, &migrated_store)?;
                persist::save_relations(&root, &rel_store)?;
                println!("Migration saved to .arrow-kanban/");
            }
        }

        Commands::Hdd { command: hdd_cmd } => {
            let mut rel_store = persist::load_relations(&root)?;
            let parse_tags = |t: Option<String>| -> Vec<String> {
                t.map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                    .unwrap_or_default()
            };

            match hdd_cmd {
                HddCommands::Paper { title, tags } => {
                    let result =
                        arrow_kanban::create_paper(&mut store, &title, parse_tags(tags), vec![])?;
                    persist::save_store(&root, &store)?;
                    println!("Created {}: {}", result.id, title);
                }
                HddCommands::Hypothesis { title, paper, tags } => {
                    let result = arrow_kanban::create_hypothesis(
                        &mut store,
                        &mut rel_store,
                        &title,
                        paper,
                        parse_tags(tags),
                        vec![],
                    )?;
                    persist::save_store(&root, &store)?;
                    persist::save_relations(&root, &rel_store)?;
                    println!(
                        "Created {}: {} (linked to PAPER-{})",
                        result.id, title, paper
                    );
                }
                HddCommands::Experiment {
                    title,
                    hypothesis,
                    tags,
                } => {
                    let result = arrow_kanban::create_experiment(
                        &mut store,
                        &mut rel_store,
                        &title,
                        &hypothesis,
                        parse_tags(tags),
                        vec![],
                    )?;
                    persist::save_store(&root, &store)?;
                    persist::save_relations(&root, &rel_store)?;
                    println!(
                        "Created {}: {} (linked to {})",
                        result.id, title, hypothesis
                    );
                }
                HddCommands::Measure {
                    title,
                    experiment,
                    tags,
                } => {
                    let result = arrow_kanban::create_measure(
                        &mut store,
                        &mut rel_store,
                        &title,
                        experiment.as_deref(),
                        parse_tags(tags),
                        vec![],
                    )?;
                    persist::save_store(&root, &store)?;
                    persist::save_relations(&root, &rel_store)?;
                    println!("Created {}: {}", result.id, title);
                }
                HddCommands::Idea { title, tags } => {
                    let result =
                        arrow_kanban::create_idea(&mut store, &title, parse_tags(tags), vec![])?;
                    persist::save_store(&root, &store)?;
                    println!("Created {}: {}", result.id, title);
                }
                HddCommands::Literature { title, tags } => {
                    let result = arrow_kanban::create_literature(
                        &mut store,
                        &title,
                        parse_tags(tags),
                        vec![],
                    )?;
                    persist::save_store(&root, &store)?;
                    println!("Created {}: {}", result.id, title);
                }
                HddCommands::Validate => {
                    let errors = arrow_kanban::validate_hdd(&store, &rel_store);
                    if errors.is_empty() {
                        println!("HDD validation passed — no issues found.");
                    } else {
                        println!("HDD validation found {} issues:", errors.len());
                        for err in &errors {
                            println!("  - {err}");
                        }
                    }
                }
                HddCommands::Registry => {
                    let chains = arrow_kanban::build_registry(&store, &rel_store);
                    if chains.is_empty() {
                        println!("No papers found.");
                    } else {
                        for chain in &chains {
                            println!("📄 {} — {}", chain.paper_id, chain.paper_title);
                            for hyp in &chain.hypotheses {
                                println!("  🔬 {} — {}", hyp.id, hyp.title);
                                for expr in &hyp.experiments {
                                    println!("    🧪 {} — {}", expr.id, expr.title);
                                    for m in &expr.measures {
                                        println!("      📏 {} — {}", m.id, m.title);
                                    }
                                }
                            }
                        }
                    }
                }
                HddCommands::Run {
                    experiment_id,
                    agent,
                } => {
                    let mut run_store = persist::load_experiment_runs(&root);
                    let run_id = run_store.start_run(&experiment_id, agent.as_deref())?;
                    persist::save_experiment_runs(&root, &run_store)?;
                    println!("Started {run_id}");
                }
                HddCommands::Status { experiment_id } => {
                    let run_store = persist::load_experiment_runs(&root);
                    let runs = run_store.list_runs(&experiment_id);
                    print!("{}", arrow_kanban::experiment_runs::format_runs(&runs));
                }
                HddCommands::Complete {
                    experiment_id,
                    run,
                    results,
                } => {
                    let mut run_store = persist::load_experiment_runs(&root);
                    run_store.complete_run(&experiment_id, run, results.as_deref())?;
                    persist::save_experiment_runs(&root, &run_store)?;
                    println!("Completed {experiment_id} run #{run}");
                }
            }
        }

        Commands::Rank { id, rank } => {
            // Verify item exists
            let _item = store.get_item(&id)?;
            store.update_rank(&id, Some(rank as i32))?;
            persist::save_store(&root, &store)?;
            println!("Ranked {id} → {rank}");
        }

        Commands::Next { assignee } => {
            // Find highest-priority unblocked backlog item
            let results = store.query_items(Some("backlog"), None, None, assignee.as_deref());
            if results.is_empty() {
                println!("No backlog items found.");
            } else {
                // Return the first item (already sorted by query)
                let item = &results[0];
                let id = item
                    .column(arrow_kanban::schema::items_col::ID)
                    .as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .expect("id")
                    .value(0);
                let title = item
                    .column(arrow_kanban::schema::items_col::TITLE)
                    .as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .expect("title")
                    .value(0);
                println!("Next: {id} — {title}");
            }
        }

        Commands::Init { .. } => {
            // Handled before config loading — should not reach here
            unreachable!("Init is handled before config loading");
        }

        Commands::Boards => {
            for board in &config.boards {
                let items = store.query_items(None, None, Some(&board.name), None);
                println!("{}: {} items", board.name, items.len());
            }
        }

        Commands::Templates { item_type } => {
            let loader = arrow_kanban::templates::ShapeLoader::new(&root);
            let generator = arrow_kanban::templates::TemplateGenerator::new(loader);

            if let Some(type_str) = item_type {
                if let Some(it) = ItemType::from_str_loose(&type_str) {
                    print!("{}", generator.generate(&it, "<Title>"));
                } else {
                    eprintln!("Unknown item type: {type_str}");
                    process::exit(1);
                }
            } else {
                let summaries = generator.list_all();
                print!(
                    "{}",
                    arrow_kanban::templates::format_type_listing(&summaries)
                );
            }
        }

        #[cfg(feature = "client")]
        Commands::McpServer { nats_url } => {
            let server = arrow_kanban::mcp_server::McpServer::new(&nats_url)?;
            eprintln!("arrow-kanban MCP server started (stdio transport)");
            server.run()?;
        }

        Commands::NextId { item_type, json } => {
            let it = ItemType::from_str_loose(&item_type)
                .ok_or_else(|| format!("Unknown item type: {item_type}"))?;
            let prefix = it.prefix();
            let next = id_alloc::max_id_for_type(store.items_batches(), prefix) + 1;

            if json {
                println!("{}", export::next_id_json(prefix, next));
            } else {
                println!("{}-{}", prefix, next);
            }
        }

        // MaterializeItem/Config/Backup/Restore are intercepted before
        // run() regardless of features (they run locally). These arms are unreachable
        // but required for exhaustive pattern matching in every feature config.
        Commands::MaterializeItem { .. } | Commands::Backup { .. } | Commands::Restore { .. } => {
            unreachable!("MaterializeItem/Config/Backup/Restore intercepted before run()")
        }
    }

    Ok(())
}

/// Handle `init` command — runs before config loading since init creates the config.
/// Uses std::fs for infrastructure bootstrapping (not data persistence).
///
/// Themes:
/// - `nautical` — default (dev + research boards)
/// - `software` — software dev only (dev board, no research)
/// - `hdd` — research board preset (research board primary)
fn run_init(root: &std::path::Path, theme: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config_dir = root.join(".arrow-kanban");
    if config_dir.exists() {
        println!("Already initialized — .arrow-kanban/ exists");
        return Ok(());
    }

    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(
        config_dir.join("config.yaml"),
        arrow_kanban::config::default_config_yaml(),
    )?;
    std::fs::create_dir_all(config_dir.join("data"))?;

    // Copy ontology files (kanban.ttl + SHACL shapes)
    let ontology_src = root.join("crates/arrow-kanban/ontology");
    let ontology_dest = config_dir.join("ontology");
    if ontology_src.exists() {
        copy_dir_recursive(&ontology_src, &ontology_dest)?;
    }

    // Generate theme-specific template files
    let loader = arrow_kanban::templates::ShapeLoader::new(root);
    let generator = arrow_kanban::templates::TemplateGenerator::new(loader);
    let templates_dir = config_dir.join("templates");
    std::fs::create_dir_all(&templates_dir)?;

    let types: Vec<ItemType> = match theme {
        "software" => vec![
            ItemType::Expedition,
            ItemType::Voyage,
            ItemType::Campaign,
            ItemType::Chore,
            ItemType::Hazard,
            ItemType::Signal,
            ItemType::Feature,
        ],
        "hdd" => vec![
            ItemType::Paper,
            ItemType::Hypothesis,
            ItemType::Experiment,
            ItemType::Measure,
            ItemType::Idea,
            ItemType::Literature,
        ],
        _ => {
            // nautical = all 13 types
            vec![
                ItemType::Expedition,
                ItemType::Voyage,
                ItemType::Campaign,
                ItemType::Chore,
                ItemType::Hazard,
                ItemType::Signal,
                ItemType::Feature,
                ItemType::Paper,
                ItemType::Hypothesis,
                ItemType::Experiment,
                ItemType::Measure,
                ItemType::Idea,
                ItemType::Literature,
            ]
        }
    };

    for it in &types {
        let template = generator.generate(it, "<Title>");
        let filename = format!("_TEMPLATE_{}.md", it.as_str().to_uppercase());
        std::fs::write(templates_dir.join(&filename), &template)?;
    }

    // Install Claude Code skills (idempotent — updates existing)
    install_claude_skills(root)?;

    println!(
        "Initialized arrow-kanban in .arrow-kanban/ (theme: {theme}, {} templates)",
        types.len()
    );
    Ok(())
}

/// Recursively copy a directory.
fn copy_dir_recursive(
    src: &std::path::Path,
    dest: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }

    Ok(())
}

/// Install Claude Code skills to .claude/skills/ for kanban workflow integration.
/// Idempotent — safe to run multiple times.
fn install_claude_skills(root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let skills_src = root.join("crates/arrow-kanban/skills");
    let skills_dest = root.join(".claude/skills");

    if !skills_src.exists() {
        // Skills not bundled (standalone install) — skip silently
        return Ok(());
    }

    let mut installed = 0;
    for entry in std::fs::read_dir(&skills_src)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }

        let skill_name = entry.file_name();
        let dest_dir = skills_dest.join(&skill_name);
        let src_skill = entry.path().join("SKILL.md");

        if !src_skill.exists() {
            continue;
        }

        // Skip if destination already has a SKILL.md (don't overwrite project-specific skills)
        let dest_skill = dest_dir.join("SKILL.md");
        if dest_skill.exists() {
            continue;
        }

        std::fs::create_dir_all(&dest_dir)?;
        std::fs::copy(&src_skill, &dest_skill)?;
        installed += 1;
    }

    if installed > 0 {
        println!("Installed {installed} Claude Code skills to .claude/skills/");
    }

    Ok(())
}

/// Run a command in client mode via NATS request-reply.
#[cfg(feature = "client")]
fn run_client(server_url: &str, command: &Commands) -> Result<(), Box<dyn std::error::Error>> {
    let client = arrow_kanban::client::NatsClient::connect(server_url)?;

    // a board-wide export (no --id) must PAGINATE — the full ~4577-item reply exceeds the
    // NATS max_payload, so a single request never returns (the client deadline elapses). Loop
    // fixed-size pages and reassemble. Single-item export (--id) stays a one-shot request below.
    if let Commands::Export {
        id: None,
        format,
        board,
        item_type,
        status,
        output,
    } = command
        && (format == "json" || format == "markdown")
    {
        return run_export_paginated(
            &client,
            format,
            board,
            item_type.as_deref(),
            status.as_deref(),
            output.as_deref(),
        );
    }

    let (cmd, payload) = command_to_nats(command);
    let response = client.request(&cmd, &payload)?;

    // Print the response in a user-friendly way
    print_client_response(&cmd, &response);

    Ok(())
}

#[cfg(feature = "client")]
/// reassemble paginated export pages into one document. JSON pages are each a
/// `[...]` item array — parse and concatenate them into one array (a malformed/empty page
/// contributes nothing). Markdown pages each carry the two header rows (`| ID | … |` +
/// `|---|…|`); keep them only from the first page and append the row lines from the rest.
fn reassemble_export_pages(format: &str, pages: &[String]) -> String {
    if format == "json" {
        let mut items: Vec<serde_json::Value> = Vec::new();
        for p in pages {
            if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(p)
            {
                items.extend(arr);
            }
        }
        serde_json::to_string_pretty(&serde_json::Value::Array(items))
            .unwrap_or_else(|_| "[]".to_string())
    } else {
        let mut md = String::new();
        for (i, p) in pages.iter().enumerate() {
            if i == 0 {
                md.push_str(p);
            } else {
                for line in p.lines().skip(2) {
                    md.push('\n');
                    md.push_str(line);
                }
            }
        }
        md
    }
}

/// board-wide export over `--server`, paginated. Requests fixed-size pages
/// (`offset`/`limit`) — each reply staying under the NATS max_payload — until
/// `offset + count >= total`, then reassembles: JSON pages are parsed and their item arrays
/// concatenated; markdown pages keep the two header lines only from the first page. Emits to
/// `--output` or stdout, matching the local (non-server) export path.
#[cfg(feature = "client")]
fn run_export_paginated(
    client: &arrow_kanban::client::NatsClient,
    format: &str,
    board: &str,
    item_type: Option<&str>,
    status: Option<&str>,
    output: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    const PAGE: usize = 400;
    let mut offset: usize = 0;
    let mut pages: Vec<String> = Vec::new();
    loop {
        let payload = serde_json::json!({
            "id": serde_json::Value::Null,
            "format": format,
            "board": board,
            "item_type": item_type,
            "status": status,
            "offset": offset,
            "limit": PAGE,
        });
        let response = client.request("export", &payload)?;
        let content = response
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let total = response.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let count = response.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        pages.push(content.to_string());
        offset += count;
        // `count == 0` guards against an empty/over-range page so we never loop forever.
        if count == 0 || offset >= total {
            break;
        }
    }

    let out = reassemble_export_pages(format, &pages);
    if let Some(path) = output {
        std::fs::write(path, &out)?;
        println!("Exported to {path}");
    } else {
        print!("{out}");
    }
    Ok(())
}

#[cfg(feature = "client")]
fn parse_nats_tags(tags: &Option<String>) -> Vec<String> {
    tags.as_ref()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_default()
}

/// The flat column a predicate projects onto, if any.
fn flat_col_for(predicate: &str) -> Option<usize> {
    use arrow_kanban::schema::items_col;
    match arrow_kanban::relation_vocab::flat_column_for(predicate) {
        Some("related") => Some(items_col::RELATED),
        Some("depends_on") => Some(items_col::DEPENDS_ON),
        _ => None,
    }
}

/// Parse and validate `predicate:TARGET` specs for local mode.
///
/// Mirrors the server-side check so `--relate` behaves IDENTICALLY on both paths — a flag
/// that works over NATS but is silently ignored locally is worse than no flag.
fn parse_relate_specs(
    specs: &[String],
    source_type: &str,
    source_id: Option<&str>,
) -> std::result::Result<Vec<(String, String)>, String> {
    use arrow_kanban::relation_vocab as vocab;
    let mut out = Vec::new();
    for spec in specs {
        let (pred, target) = vocab::parse_spec(spec).ok_or_else(|| {
            format!("malformed --relate '{spec}' — expected <predicate>:<TARGET-ID>, e.g. implements:VY-1234")
        })?;
        let p = vocab::validate_edge(
            pred,
            source_id.unwrap_or("<new>"),
            source_type,
            target,
            vocab::type_from_id(target),
        )
        .map_err(|e| e.to_string())?;
        out.push((p.name.to_string(), target.to_string()));
    }
    Ok(out)
}

/// Convert a CLI command to a NATS subject + JSON payload.
#[cfg(feature = "client")]
fn command_to_nats(command: &Commands) -> (String, serde_json::Value) {
    match command {
        Commands::Create {
            item_type,
            title,
            priority,
            assign,
            tags,
            body,
            body_file,
            body_stdin: _,
            template,
            relate,
            push: _,
        } => {
            let tag_list: Vec<String> = tags
                .as_deref()
                .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default();

            // For NATS mode, resolve body from --body, --body-file, or --template
            // (stdin not supported in NATS client mode)
            let body_content = if body.is_some() {
                body.clone()
            } else if let Some(path) = body_file {
                match std::fs::read_to_string(path) {
                    Ok(content) => Some(content),
                    Err(e) => {
                        eprintln!("Error: Failed to read body file '{}': {}", path, e);
                        std::process::exit(1);
                    }
                }
            } else if template.is_some() {
                Some(generate_template(
                    item_type,
                    title,
                    &std::env::current_dir().unwrap_or_default(),
                ))
            } else {
                None
            };

            (
                "create".to_string(),
                serde_json::json!({
                    "title": title,
                    "item_type": item_type,
                    "priority": priority,
                    "assignee": assign,
                    "tags": tag_list,
                    "body": body_content,
                    // typed relationships at CREATE time.
                    "relate": relate,
                }),
            )
        }
        Commands::Move {
            id,
            status,
            assign,
            force,
            resolution,
            closed_by,
        } => (
            "move".to_string(),
            serde_json::json!({
                "id": id,
                "status": status,
                "assignee": assign,
                "force": force,
                "resolution": resolution,
                "closed_by": closed_by,
            }),
        ),
        Commands::Ratify { phase } => (
            "ratify".to_string(),
            serde_json::json!({ "phase_tag": phase }),
        ),
        Commands::List {
            status,
            board,
            item_type,
            assignee,
            resolution,
            priority,
            tag,
            ready,
        } => (
            "list".to_string(),
            serde_json::json!({
                "status": status,
                "board": board,
                "item_type": item_type,
                "assignee": assignee,
                "resolution": resolution,
                "priority": priority,
                "tags": tag,
                "ready": ready,
            }),
        ),
        Commands::Board { board } => ("board".to_string(), serde_json::json!({ "board": board })),
        Commands::Show {
            id,
            format,
            relations,
        } => (
            "show".to_string(),
            serde_json::json!({ "id": id, "format": format, "relations": relations }),
        ),
        Commands::Query {
            query: words,
            search,
            sparql,
            json: _,
            verbose: _,
            no_semantic: _,
            top,
            embedding_provider,
        } => {
            let mut payload = serde_json::json!({ "query": words.join(" ") });
            if let Some(s) = search {
                payload["search"] = serde_json::json!(s);
                payload["top"] = serde_json::json!(top);
            }
            if *top != 10 {
                payload["top"] = serde_json::json!(top);
            }
            if let Some(s) = sparql {
                payload["sparql"] = serde_json::json!(s);
            }
            if let Some(ep) = embedding_provider {
                payload["embedding_provider"] = serde_json::json!(ep);
            }
            ("query".to_string(), payload)
        }
        Commands::Stats {
            board,
            velocity,
            burndown,
            by_agent,
            since,
            weeks,
        } => (
            "stats".to_string(),
            serde_json::json!({
                "board": board,
                "velocity": velocity,
                "burndown": burndown,
                "by_agent": by_agent,
                "since": since,
                "weeks": weeks,
            }),
        ),
        Commands::History {
            week,
            month,
            since,
            by_assignee,
        } => (
            "history".to_string(),
            serde_json::json!({
                "week": week,
                "month": month,
                "since": since,
                "by_assignee": by_assignee,
            }),
        ),
        Commands::Roadmap {
            flat,
            ready,
            campaign,
        } => (
            "roadmap".to_string(),
            serde_json::json!({ "flat": flat, "ready": ready, "campaign": campaign }),
        ),
        Commands::CriticalPath => ("critical-path".to_string(), serde_json::json!({})),
        Commands::Worklist { agents, depth } => (
            "worklist".to_string(),
            serde_json::json!({ "agents": agents, "depth": depth }),
        ),
        Commands::Blocked => ("blocked".to_string(), serde_json::json!({})),
        Commands::Validate {
            id,
            fix,
            board,
            all,
            status,
        } => (
            "validate".to_string(),
            serde_json::json!({ "id": id, "fix": fix, "board": board, "all": all, "status": status }),
        ),
        Commands::Export {
            id,
            format,
            board,
            item_type,
            status,
            output: _,
        } => (
            "export".to_string(),
            serde_json::json!({ "id": id, "format": format, "board": board, "item_type": item_type, "status": status }),
        ),
        Commands::NextId { item_type, json: _ } => (
            "next-id".to_string(),
            serde_json::json!({ "item_type": item_type }),
        ),
        Commands::Migrate { dry_run: _ } => {
            // Migration doesn't make sense in client mode
            ("stats".to_string(), serde_json::json!({}))
        }
        Commands::Hdd { command } => match &command {
            HddCommands::Run {
                experiment_id,
                agent,
            } => (
                "hdd.run".to_string(),
                serde_json::json!({ "experiment_id": experiment_id, "agent": agent }),
            ),
            HddCommands::Status { experiment_id } => (
                "hdd.run.status".to_string(),
                serde_json::json!({ "experiment_id": experiment_id }),
            ),
            HddCommands::Complete {
                experiment_id,
                run,
                results,
            } => (
                "hdd.run.complete".to_string(),
                serde_json::json!({ "experiment_id": experiment_id, "run": run, "results": results }),
            ),
            HddCommands::Paper { title, tags } => (
                "hdd.paper".to_string(),
                serde_json::json!({ "title": title, "tags": parse_nats_tags(tags) }),
            ),
            HddCommands::Hypothesis { title, paper, tags } => (
                "hdd.hypothesis".to_string(),
                serde_json::json!({ "title": title, "paper": paper, "tags": parse_nats_tags(tags) }),
            ),
            HddCommands::Experiment {
                title,
                hypothesis,
                tags,
            } => (
                "hdd.experiment".to_string(),
                serde_json::json!({ "title": title, "hypothesis": hypothesis, "tags": parse_nats_tags(tags) }),
            ),
            HddCommands::Measure {
                title,
                experiment,
                tags,
            } => (
                "hdd.measure".to_string(),
                serde_json::json!({ "title": title, "experiment": experiment, "tags": parse_nats_tags(tags) }),
            ),
            HddCommands::Idea { title, tags } => (
                "hdd.idea".to_string(),
                serde_json::json!({ "title": title, "tags": parse_nats_tags(tags) }),
            ),
            HddCommands::Literature { title, tags } => (
                "hdd.literature".to_string(),
                serde_json::json!({ "title": title, "tags": parse_nats_tags(tags) }),
            ),
            HddCommands::Validate => ("hdd.validate".to_string(), serde_json::json!({})),
            HddCommands::Registry => ("hdd.registry".to_string(), serde_json::json!({})),
        },
        Commands::Rank { id, rank } => (
            "rank".to_string(),
            serde_json::json!({ "id": id, "rank": rank }),
        ),
        Commands::Next { assignee } => (
            "list".to_string(),
            serde_json::json!({ "status": "backlog", "assignee": assignee }),
        ),
        Commands::Init { .. } => ("stats".to_string(), serde_json::json!({})),
        Commands::Boards => ("stats".to_string(), serde_json::json!({})),
        Commands::Templates { item_type } => (
            "templates".to_string(),
            serde_json::json!({ "item_type": item_type }),
        ),
        Commands::McpServer { .. } => ("stats".to_string(), serde_json::json!({})),
        Commands::Update {
            id,
            title,
            priority,
            assign,
            tags,
            body,
            body_file,
            related,
            depends_on,
            relate,
            unrelate,
        } => {
            let mut payload = serde_json::json!({ "id": id });
            // typed relationships on EDIT. Additive/subtractive, unlike the
            // replace-semantics of --related/--depends-on.
            if !relate.is_empty() {
                payload["relate"] = serde_json::json!(relate);
            }
            if !unrelate.is_empty() {
                payload["unrelate"] = serde_json::json!(unrelate);
            }
            if let Some(t) = title {
                payload["title"] = serde_json::json!(t);
            }
            if let Some(p) = priority {
                payload["priority"] = serde_json::json!(p);
            }
            if let Some(a) = assign {
                payload["assignee"] = serde_json::json!(a);
            }
            if let Some(t) = tags {
                let list: Vec<&str> = t.split(',').map(|s| s.trim()).collect();
                payload["tags"] = serde_json::json!(list);
            }
            if let Some(path) = body_file {
                if let Ok(content) = std::fs::read_to_string(path) {
                    payload["body"] = serde_json::json!(content.trim());
                }
            } else if let Some(b) = body {
                payload["body"] = serde_json::json!(b);
            }
            if let Some(r) = related {
                let list: Vec<&str> = r.split(',').map(|s| s.trim()).collect();
                payload["related"] = serde_json::json!(list);
            }
            if let Some(d) = depends_on {
                let list: Vec<&str> = d.split(',').map(|s| s.trim()).collect();
                payload["depends_on"] = serde_json::json!(list);
            }
            ("update".to_string(), payload)
        }
        Commands::Comment { id, text } => (
            "comment".to_string(),
            serde_json::json!({ "id": id, "text": text }),
        ),
        Commands::MaterializeItem { .. } | Commands::Backup { .. } | Commands::Restore { .. } => {
            unreachable!("MaterializeItem/Config/Backup/Restore intercepted before NATS dispatch")
        }
    }
}

/// Print a NATS response in user-friendly format.
#[cfg(feature = "client")]
fn print_client_response(command: &str, response: &serde_json::Value) {
    match command {
        "create" => {
            if let (Some(id), Some(title)) = (
                response.get("id").and_then(|v| v.as_str()),
                response.get("title").and_then(|v| v.as_str()),
            ) {
                println!("Created {id}: {title}");
            }
        }
        "move" => {
            if let (Some(id), Some(from), Some(to)) = (
                response.get("id").and_then(|v| v.as_str()),
                response.get("from").and_then(|v| v.as_str()),
                response.get("to").and_then(|v| v.as_str()),
            ) {
                println!("Moved {id} from {from} to {to}");
            }
        }
        "rank" => {
            if let (Some(id), Some(rank)) = (
                response.get("id").and_then(|v| v.as_str()),
                response.get("rank").and_then(|v| v.as_i64()),
            ) {
                println!("Ranked {id} → {rank}");
            } else if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                println!("Cleared rank on {id}");
            }
        }
        "list" | "query" | "blocked" => {
            if let Some(table) = response.get("table").and_then(|v| v.as_str()) {
                print!("{table}");
            }
        }
        "show" => {
            // The server renders per --format and returns exactly ONE of: `detail`
            // (default), `markdown` (--format md), or `json` (--format json). Print
            // whichever is present — previously only `detail` was handled, so under
            // --server both `--format md` and `--format json` silently printed
            // NOTHING (the time-to-orient measure could not read the
            // item as JSON at all).
            if let Some(detail) = response.get("detail").and_then(|v| v.as_str()) {
                print!("{detail}");
            } else if let Some(markdown) = response.get("markdown").and_then(|v| v.as_str()) {
                print!("{markdown}");
            } else if let Some(json) = response.get("json").and_then(|v| v.as_str()) {
                println!("{json}");
            }
        }
        "board" | "roadmap" | "critical-path" | "worklist" => {
            if let Some(view) = response.get("view").and_then(|v| v.as_str()) {
                print!("{view}");
            }
        }
        "pipeline-health" => {
            // Emit the full reading as JSON (redirect to research/shared/eval-data/runway/).
            println!(
                "{}",
                serde_json::to_string_pretty(response).unwrap_or_else(|_| response.to_string())
            );
        }
        "stats" => {
            if let Some(stats) = response.get("stats").and_then(|v| v.as_str()) {
                print!("{stats}");
            }
        }
        "history" => {
            if let Some(history) = response.get("history").and_then(|v| v.as_str()) {
                print!("{history}");
            }
        }
        "validate" => {
            if let Some(valid) = response.get("valid").and_then(|v| v.as_bool()) {
                if valid {
                    println!("Board is valid.");
                } else if let Some(issues) = response.get("issues") {
                    println!("Validation issues:\n{issues}");
                }
            }
        }
        "export" => {
            if let Some(content) = response.get("content").and_then(|v| v.as_str()) {
                print!("{content}");
            }
        }
        "next-id" => {
            if let Some(next) = response.get("next_id").and_then(|v| v.as_str()) {
                println!("{next}");
            }
        }
        "delete" => {
            if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                println!("Deleted {id}");
            }
        }
        // PR commands
        "pr.create" => {
            if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                let title = response.get("title").and_then(|v| v.as_str()).unwrap_or("");
                println!("Created proposal {id}: {title}");
                if let Some(source) = response.get("source_branch").and_then(|v| v.as_str()) {
                    let target = response
                        .get("target_branch")
                        .and_then(|v| v.as_str())
                        .unwrap_or("main");
                    println!("  {source} → {target}");
                }
            }
        }
        "pr.list" => {
            if let Some(table) = response.get("table").and_then(|v| v.as_str()) {
                print!("{table}");
            } else {
                println!("No proposals.");
            }
        }
        "pr.view" | "pr.diff" | "pr.checks" => {
            if let Some(detail) = response.get("detail").and_then(|v| v.as_str()) {
                print!("{detail}");
            }
        }
        "pr.review" => {
            if let Some(msg) = response.get("message").and_then(|v| v.as_str()) {
                println!("{msg}");
            }
        }
        "pr.merge" => {
            if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                println!("Merged {id}");
            }
        }
        "pr.close" => {
            if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                println!("Closed {id}");
            }
        }
        "pr.comment" => {
            if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
                println!("Comment added to {id}");
            }
        }
        "pr.revise" => {
            if let Some(msg) = response.get("message").and_then(|v| v.as_str()) {
                println!("{msg}");
            }
        }
        "pr.resolve" => {
            if let Some(msg) = response.get("message").and_then(|v| v.as_str()) {
                println!("{msg}");
            }
        }
        // Git commands
        "git.push" | "git.pull" | "git.clone" | "git.rebase" => {
            if let Some(msg) = response.get("message").and_then(|v| v.as_str()) {
                println!("{msg}");
            }
        }
        "git.log" | "git.blame" => {
            if let Some(detail) = response.get("detail").and_then(|v| v.as_str()) {
                print!("{detail}");
            }
        }
        // Source transport
        "source.push" => {
            if let Some(msg) = response.get("message").and_then(|v| v.as_str()) {
                println!("{msg}");
            }
        }
        "source.branches" => {
            if let Some(branches) = response.get("branches").and_then(|v| v.as_array()) {
                if branches.is_empty() {
                    println!("No branches on server.");
                } else {
                    println!("Branches on server:\n");
                    for b in branches {
                        if let Some(info) = b.as_object() {
                            let name = info.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let size = info.get("size_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                            let agent = info.get("agent").and_then(|v| v.as_str()).unwrap_or("?");
                            println!("  {name}  ({size} bytes, pushed by {agent})");
                        }
                    }
                }
            }
        }
        "source.delete" => {
            if let Some(msg) = response.get("message").and_then(|v| v.as_str()) {
                println!("{msg}");
            }
        }
        "templates" => {
            if let Some(template) = response.get("template").and_then(|v| v.as_str()) {
                print!("{template}");
            } else if let Some(types) = response.get("types").and_then(|v| v.as_array()) {
                println!("Available item types:\n");
                for t in types {
                    let name = t.get("type").and_then(|v| v.as_str()).unwrap_or("-");
                    let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    println!("  {name:12} {desc}");
                }
                println!("\nUsage: nk templates <type>");
            }
        }
        "hdd.paper" | "hdd.hypothesis" | "hdd.experiment" | "hdd.measure" | "hdd.idea"
        | "hdd.literature" => {
            if let (Some(id), Some(title)) = (
                response.get("id").and_then(|v| v.as_str()),
                response.get("title").and_then(|v| v.as_str()),
            ) {
                println!("Created {id}: {title}");
            }
        }
        "hdd.validate" | "hdd.registry" => {
            if let Some(output) = response.get("output").and_then(|v| v.as_str()) {
                print!("{output}");
            }
        }
        "hdd.run" => {
            if let Some(run_id) = response.get("run_id").and_then(|v| v.as_str()) {
                println!("Started {run_id}");
            }
        }
        "hdd.run.status" => {
            if let Some(output) = response.get("output").and_then(|v| v.as_str()) {
                print!("{output}");
            }
        }
        "hdd.run.complete" => {
            if let (Some(id), Some(run)) = (
                response.get("experiment_id").and_then(|v| v.as_str()),
                response.get("run").and_then(|v| v.as_u64()),
            ) {
                println!("Completed {id} run #{run}");
            }
        }
        _ => {
            // Generic JSON output for unknown commands
            println!(
                "{}",
                serde_json::to_string_pretty(response).unwrap_or_default()
            );
        }
    }
}

/// Find items that are blocked by unmet dependencies.
///
/// Uses in-crate BFS for transitive dependency traversal with cycle detection.
/// An item is "blocked" if it has a depends_on chain where at least one
/// dependency is not in a terminal state.
fn find_blocked_items(store: &arrow_kanban::crud::KanbanStore) -> Vec<arrow::array::RecordBatch> {
    use arrow::array::{Array as _, StringArray};
    use arrow_kanban::schema::items_col;

    // Build a set of done/terminal item IDs
    let mut done_ids = std::collections::HashSet::new();
    for batch in store.items_batches() {
        let Some(ids) = batch
            .column(items_col::ID)
            .as_any()
            .downcast_ref::<StringArray>()
        else {
            continue;
        };
        let Some(statuses) = batch
            .column(items_col::STATUS)
            .as_any()
            .downcast_ref::<StringArray>()
        else {
            continue;
        };
        let Some(deleted) = batch
            .column(items_col::DELETED)
            .as_any()
            .downcast_ref::<arrow::array::BooleanArray>()
        else {
            continue;
        };
        for i in 0..batch.num_rows() {
            if !deleted.value(i) && statuses.value(i) == "done" {
                done_ids.insert(ids.value(i).to_string());
            }
        }
    }

    // Build transitive dependency graph using the in-crate traversal
    let all = store.query_items(None, None, None, None);
    let adj = build_dependency_adjacency(&all);

    // Filter: items where any transitive dependency is not done
    all.into_iter()
        .filter(|batch| {
            let Some(ids) = batch
                .column(items_col::ID)
                .as_any()
                .downcast_ref::<StringArray>()
            else {
                return false;
            };
            let Some(statuses) = batch
                .column(items_col::STATUS)
                .as_any()
                .downcast_ref::<StringArray>()
            else {
                return false;
            };
            (0..batch.num_rows()).any(|i| {
                if statuses.value(i) == "done" {
                    return false;
                }
                // Use BFS to find all transitive dependencies
                let deps = arrow_kanban::traversal::bfs_with_adjacency(ids.value(i), &adj, 100);
                // Blocked if any transitive dep is not done. Skip an
                // empty-string dep — it is not a real dependency, and counting it here while
                // `--ready` (critical_path) treats `""` as satisfied made the two disagree.
                deps.iter()
                    .any(|node| !node.id.trim().is_empty() && !done_ids.contains(&node.id))
            })
        })
        .collect()
}

/// Format all relationships for an item (depends_on, related, RelationsStore).
fn format_item_relations(id: &str, store: &arrow_kanban::crud::KanbanStore) -> String {
    use arrow::array::{Array as _, ListArray, StringArray};
    use arrow_kanban::schema::items_col;

    let mut lines = Vec::new();
    lines.push(String::new());
    lines.push("  Relations".to_string());
    lines.push(format!("  {}", "─".repeat(60)));

    // Get the item batch
    let Ok(item) = store.get_item(id) else {
        lines.push("  (item not found)".to_string());
        return lines.join("\n") + "\n";
    };

    // depends_on
    if let Some(deps) = item
        .column(items_col::DEPENDS_ON)
        .as_any()
        .downcast_ref::<ListArray>()
        && !deps.is_null(0)
        && !deps.value(0).is_empty()
    {
        let dep_vals = deps.value(0);
        if let Some(dep_strs) = dep_vals.as_any().downcast_ref::<StringArray>() {
            lines.push("  Depends on:".to_string());
            for j in 0..dep_strs.len() {
                if !dep_strs.is_null(j) {
                    lines.push(format!("    → {}", dep_strs.value(j)));
                }
            }
        }
    }

    // related
    if let Some(related) = item
        .column(items_col::RELATED)
        .as_any()
        .downcast_ref::<ListArray>()
        && !related.is_null(0)
        && !related.value(0).is_empty()
    {
        let rel_vals = related.value(0);
        if let Some(rel_strs) = rel_vals.as_any().downcast_ref::<StringArray>() {
            lines.push("  Related:".to_string());
            for j in 0..rel_strs.len() {
                if !rel_strs.is_null(j) {
                    lines.push(format!("    ↔ {}", rel_strs.value(j)));
                }
            }
        }
    }

    // Transitive dependency chain (via in-crate BFS)
    let all = store.query_items(None, None, None, None);
    let adj = build_dependency_adjacency(&all);

    let chain = arrow_kanban::traversal::bfs_with_adjacency(id, &adj, 10);
    if !chain.is_empty() {
        lines.push("  Dependency chain (transitive):".to_string());
        for node in &chain {
            let indent = "  ".repeat(node.depth);
            lines.push(format!("    {indent}↳ {} (depth {})", node.id, node.depth));
        }
    }

    if lines.len() <= 3 {
        lines.push("  (no relationships)".to_string());
    }

    lines.join("\n") + "\n"
}

/// Build dependency adjacency map from all items.
fn build_dependency_adjacency(
    all_items: &[arrow::array::RecordBatch],
) -> std::collections::HashMap<String, Vec<String>> {
    use arrow_kanban::schema::items_col;

    let mut adj = std::collections::HashMap::new();
    for batch in all_items {
        let partial = arrow_kanban::traversal::build_adjacency_from_list(
            batch,
            items_col::ID,
            items_col::DEPENDS_ON,
            arrow_kanban::traversal::Direction::Forward,
        );
        for (k, v) in partial {
            adj.entry(k).or_insert_with(Vec::new).extend(v);
        }
    }
    adj
}

/// Build a map of item ID → status for chain display.
fn build_status_map(
    store: &arrow_kanban::crud::KanbanStore,
) -> std::collections::HashMap<String, String> {
    use arrow::array::StringArray;
    use arrow_kanban::schema::items_col;

    let mut map = std::collections::HashMap::new();
    for batch in store.items_batches() {
        let Some(ids) = batch
            .column(items_col::ID)
            .as_any()
            .downcast_ref::<StringArray>()
        else {
            continue;
        };
        let Some(statuses) = batch
            .column(items_col::STATUS)
            .as_any()
            .downcast_ref::<StringArray>()
        else {
            continue;
        };
        for i in 0..batch.num_rows() {
            map.insert(ids.value(i).to_string(), statuses.value(i).to_string());
        }
    }
    map
}

#[cfg(test)]
mod cli_parse_tests {
    //! `arrow-kanban pr comment` accepts positional <TEXT> (mirroring
    //! `arrow-kanban comment <ID> <TEXT>`) AND the legacy `--body` flag — exactly one.
    use super::*;

    /// Parse an argv (sans the leading binary name is NOT included — clap wants it).
    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("arrow-kanban").chain(argv.iter().copied()))
    }

    #[test]
    fn item_comment_positional_unchanged() {
        // The sibling `comment <ID> <TEXT>` surface is untouched.
        let cli = parse(&["comment", "EX-1", "note"]).expect("item comment parses");
        match &cli.command {
            Commands::Comment { id, text } => {
                assert_eq!(id, "EX-1");
                assert_eq!(text, "note");
            }
            _ => panic!("expected Commands::Comment"),
        }
    }
}

// `reassemble_export_pages` is compiled only with `client` (the paginated
// server-export path), so its tests are gated the same way.
#[cfg(all(test, feature = "client"))]
mod ch6555_reassembly_tests {
    //! paginated board-export pages must reassemble into one document —
    //! JSON arrays concatenated, markdown headers de-duplicated to the first page.
    use super::reassemble_export_pages;

    #[test]
    fn json_pages_concatenate_into_one_array() {
        let pages = vec![
            r#"[{"id":"EX-1"},{"id":"EX-2"}]"#.to_string(),
            r#"[{"id":"EX-3"}]"#.to_string(),
            "[]".to_string(), // an empty trailing page contributes nothing
        ];
        let out = reassemble_export_pages("json", &pages);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 3, "all items across pages present: {out}");
        assert_eq!(arr[0]["id"], "EX-1");
        assert_eq!(arr[2]["id"], "EX-3");
    }

    #[test]
    fn json_ignores_a_malformed_page() {
        // A page that timed out / returned junk must not poison the whole export.
        let pages = vec![r#"[{"id":"EX-1"}]"#.to_string(), "not json".to_string()];
        let out = reassemble_export_pages("json", &pages);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn markdown_keeps_one_header_appends_rows() {
        let hdr = "| ID | Title | Type | Status | Priority | Assignee |\n|---|---|---|---|---|---|";
        let page1 = format!("{hdr}\n| EX-1 | A | expedition | done | - | M5 |");
        let page2 = format!("{hdr}\n| EX-2 | B | chore | backlog | - | - |");
        let out = reassemble_export_pages("markdown", &[page1, page2]);
        // Exactly ONE header row + ONE separator row across the whole doc (count full lines,
        // not substrings — "|---|" repeats WITHIN a single separator line).
        let header_lines = out
            .lines()
            .filter(|l| l.starts_with("| ID | Title"))
            .count();
        let sep_lines = out
            .lines()
            .filter(|l| *l == "|---|---|---|---|---|---|")
            .count();
        assert_eq!(header_lines, 1, "one header row only:\n{out}");
        assert_eq!(sep_lines, 1, "one separator row only:\n{out}");
        assert!(
            out.contains("| EX-1 |") && out.contains("| EX-2 |"),
            "both rows present"
        );
    }
}
