// SPDX-License-Identifier: MIT
//! Critical path analysis — topological sort, dependency depth, and worklist generation.
//!
//! Builds a DAG from `depends_on` edges across kanban items, then computes:
//! - Topological ordering (Kahn's algorithm)
//! - Depth for each node (longest path from any root)
//! - Parallel groups (items at the same depth)
//! - The critical path (longest dependency chain)
//! - Blocked vs ready status
//! - Voyage grouping (via `related` field)

use arrow::array::{Array, BooleanArray, Int32Array, ListArray, RecordBatch, StringArray};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::relations::RelationsStore;
use crate::schema::items_col;

/// Lightweight item info extracted from Arrow batches.
#[derive(Debug, Clone)]
pub struct ItemInfo {
    pub id: String,
    pub title: String,
    pub item_type: String,
    pub status: String,
    pub priority: String,
    pub assignee: String,
    pub related: Vec<String>,
    pub depends_on: Vec<String>,
    /// Manual rank. `Some(n)` = pinned at position n (lower = higher
    /// priority); `None` = unranked, falls back to `priority`-string ordering.
    pub rank: Option<i32>,
}

/// Result of critical path analysis.
#[derive(Debug)]
pub struct CriticalPathResult {
    /// Items in topological order (roots first).
    pub ordered: Vec<String>,
    /// Depth of each item (longest path from any root). Depth 0 = no unmet dependencies.
    pub depth: HashMap<String, u32>,
    /// Items grouped by depth level.
    pub depth_groups: BTreeMap<u32, Vec<String>>,
    /// The longest dependency chain (the actual critical path).
    pub longest_path: Vec<String>,
    /// Items that have all dependencies met (or no dependencies).
    pub ready: Vec<String>,
    /// Items with at least one unmet dependency.
    pub blocked: Vec<String>,
    /// Transitive dependent count per item (how many items depend on this, transitively).
    pub downstream_count: HashMap<String, usize>,
}

/// Items grouped under their parent voyage.
#[derive(Debug)]
pub struct VoyageGroup {
    pub voyage_id: String,
    pub voyage_title: String,
    pub voyage_status: String,
    pub items: Vec<String>,
    pub done_count: usize,
    pub total_count: usize,
}

/// Agent assignment recommendation.
#[derive(Debug)]
pub struct WorklistEntry {
    pub agent: String,
    pub items: Vec<WorklistItem>,
}

#[derive(Debug)]
pub struct WorklistItem {
    pub id: String,
    pub title: String,
    pub blocked: bool,
}

// ─── Extract ────────────────────────────────────────────────────────────────

/// Extract lightweight item info from Arrow batches.
pub fn extract_items(batches: &[RecordBatch]) -> Vec<ItemInfo> {
    let mut items = Vec::new();

    for batch in batches {
        let ids = col_str(batch, items_col::ID);
        let titles = col_str(batch, items_col::TITLE);
        let types = col_str(batch, items_col::ITEM_TYPE);
        let statuses = col_str(batch, items_col::STATUS);
        let priorities = col_str(batch, items_col::PRIORITY);
        let assignees = col_str(batch, items_col::ASSIGNEE);
        let deleted = batch
            .column(items_col::DELETED)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("deleted column");
        let related_col = batch
            .column(items_col::RELATED)
            .as_any()
            .downcast_ref::<ListArray>()
            .expect("related column");
        let depends_col = batch
            .column(items_col::DEPENDS_ON)
            .as_any()
            .downcast_ref::<ListArray>()
            .expect("depends_on column");
        let rank_col = batch
            .column(items_col::PRIORITY_RANK)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("priority_rank column");

        for i in 0..batch.num_rows() {
            if deleted.value(i) {
                continue;
            }

            let related = extract_list_values(related_col, i);
            let depends_on = extract_list_values(depends_col, i);
            let rank = if rank_col.is_null(i) {
                None
            } else {
                Some(rank_col.value(i))
            };

            items.push(ItemInfo {
                id: ids.value(i).to_string(),
                title: titles.value(i).to_string(),
                item_type: types.value(i).to_string(),
                status: statuses.value(i).to_string(),
                priority: if priorities.is_null(i) {
                    "-".to_string()
                } else {
                    priorities.value(i).to_string()
                },
                assignee: if assignees.is_null(i) {
                    "-".to_string()
                } else {
                    assignees.value(i).to_string()
                },
                related,
                depends_on,
                rank,
            });
        }
    }

    items
}

// ─── Critical Path ──────────────────────────────────────────────────────────

/// Compute critical path analysis for non-done items.
///
/// Uses Kahn's algorithm for topological sort, then computes depth
/// (longest path from any root) for dependency ordering.
pub fn compute_critical_path(items: &[ItemInfo]) -> Result<CriticalPathResult, String> {
    // Build the item lookup (all items, including done — needed for dep resolution)
    let item_map: HashMap<&str, &ItemInfo> = items.iter().map(|i| (i.id.as_str(), i)).collect();

    // Done items set
    let done_ids: HashSet<&str> = items
        .iter()
        .filter(|i| i.status == "done" || i.status == "complete" || i.status == "retired")
        .map(|i| i.id.as_str())
        .collect();

    // Active items (not done, not deleted)
    let active: Vec<&ItemInfo> = items
        .iter()
        .filter(|i| !done_ids.contains(i.id.as_str()))
        .collect();

    let active_ids: HashSet<&str> = active.iter().map(|i| i.id.as_str()).collect();

    // Build adjacency list and in-degree count (only among active items)
    // Edge: dependency → dependent (if A depends on B, edge is B → A)
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();

    for item in &active {
        adj.entry(item.id.as_str()).or_default();
        in_degree.entry(item.id.as_str()).or_insert(0);

        for dep_id in &item.depends_on {
            // Only count active (non-done) dependencies
            if active_ids.contains(dep_id.as_str()) {
                adj.entry(dep_id.as_str())
                    .or_default()
                    .push(item.id.as_str());
                *in_degree.entry(item.id.as_str()).or_insert(0) += 1;
            }
        }
    }

    // Kahn's algorithm — topological sort
    let mut queue: VecDeque<&str> = VecDeque::new();
    for (&id, &deg) in &in_degree {
        if deg == 0 {
            queue.push_back(id);
        }
    }

    let mut ordered: Vec<String> = Vec::new();
    let mut depth: HashMap<String, u32> = HashMap::new();

    // Initialize roots at depth 0
    for &id in queue.iter() {
        depth.insert(id.to_string(), 0);
    }

    while let Some(node) = queue.pop_front() {
        ordered.push(node.to_string());
        let node_depth = depth[node];

        if let Some(dependents) = adj.get(node) {
            for &dep in dependents {
                // Propagate depth: dependent is at least 1 deeper than this node
                let new_depth = node_depth + 1;
                let current = depth.entry(dep.to_string()).or_insert(0);
                if new_depth > *current {
                    *current = new_depth;
                }

                let deg = in_degree.get_mut(dep).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(dep);
                }
            }
        }
    }

    // Cycle detection
    if ordered.len() < active.len() {
        let in_cycle: Vec<String> = active
            .iter()
            .filter(|i| !ordered.contains(&i.id))
            .map(|i| format!("{} ({})", i.id, i.title))
            .collect();
        return Err(format!(
            "Dependency cycle detected among: {}",
            in_cycle.join(", ")
        ));
    }

    // Group by depth
    let mut depth_groups: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for (id, &d) in &depth {
        depth_groups.entry(d).or_default().push(id.clone());
    }
    // Sort within each group by manual rank, then by
    // priority-string fallback. See `sort_key`.
    for group in depth_groups.values_mut() {
        group.sort_by(|a, b| {
            let ka = item_map
                .get(a.as_str())
                .map(|i| sort_key(i))
                .unwrap_or((2, 0, 99));
            let kb = item_map
                .get(b.as_str())
                .map(|i| sort_key(i))
                .unwrap_or((2, 0, 99));
            ka.cmp(&kb)
        });
    }

    // Compute downstream (transitive dependent) count
    let mut downstream_count: HashMap<String, usize> = HashMap::new();
    // Process in reverse topological order
    for id in ordered.iter().rev() {
        let count = if let Some(dependents) = adj.get(id.as_str()) {
            dependents
                .iter()
                .map(|&d| 1 + downstream_count.get(d).copied().unwrap_or(0))
                .sum()
        } else {
            0
        };
        downstream_count.insert(id.clone(), count);
    }

    // Find longest path (critical path) by backtracking from deepest node
    let max_depth = depth.values().copied().max().unwrap_or(0);
    let longest_path = trace_longest_path(&ordered, &depth, &active, max_depth);

    // Ready vs blocked
    let mut ready = Vec::new();
    let mut blocked = Vec::new();
    for item in &active {
        let has_unmet = item
            .depends_on
            .iter()
            .any(|dep| active_ids.contains(dep.as_str()));
        if has_unmet {
            blocked.push(item.id.clone());
        } else {
            ready.push(item.id.clone());
        }
    }

    // Sort ready by manual rank, then by priority-string fallback. See `sort_key`.
    ready.sort_by(|a, b| {
        let ka = item_map
            .get(a.as_str())
            .map(|i| sort_key(i))
            .unwrap_or((2, 0, 99));
        let kb = item_map
            .get(b.as_str())
            .map(|i| sort_key(i))
            .unwrap_or((2, 0, 99));
        ka.cmp(&kb)
    });

    Ok(CriticalPathResult {
        ordered,
        depth,
        depth_groups,
        longest_path,
        ready,
        blocked,
        downstream_count,
    })
}

/// Trace the longest path through the DAG by picking the deepest node at each level.
fn trace_longest_path(
    ordered: &[String],
    depth: &HashMap<String, u32>,
    active: &[&ItemInfo],
    max_depth: u32,
) -> Vec<String> {
    if max_depth == 0 {
        return Vec::new();
    }

    let item_map: HashMap<&str, &ItemInfo> = active.iter().map(|&i| (i.id.as_str(), i)).collect();

    // Build reverse adjacency: for each item, what are its active dependencies?
    let mut deps_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for &item in active {
        let active_deps: Vec<&str> = item
            .depends_on
            .iter()
            .filter(|d| item_map.contains_key(d.as_str()))
            .map(|d| d.as_str())
            .collect();
        deps_of.insert(item.id.as_str(), active_deps);
    }

    // Start from the deepest node
    let mut path = Vec::new();
    let deepest = ordered
        .iter()
        .find(|id| depth.get(id.as_str()).copied() == Some(max_depth));

    if let Some(start) = deepest {
        let mut current = start.as_str();
        path.push(current.to_string());

        // Walk backward through dependencies
        loop {
            let deps = deps_of.get(current).cloned().unwrap_or_default();
            if deps.is_empty() {
                break;
            }
            // Pick the dependency with the highest depth (stay on critical path)
            let next = deps
                .iter()
                .max_by_key(|d| depth.get(**d).copied().unwrap_or(0));
            if let Some(&n) = next {
                path.push(n.to_string());
                current = n;
            } else {
                break;
            }
        }

        path.reverse();
    }

    path
}

// ─── Voyage Grouping ────────────────────────────────────────────────────────

/// Group items under their parent voyages using the `related` field.
///
/// An item belongs to a voyage if its `related` list contains a voyage ID,
/// or if it IS a voyage.
/// fold the TYPED expedition→voyage membership edges (`implements` and its
/// inverse `spawns`) into each item's in-memory [`ItemInfo::related`], so the voyage
/// rollup ([`group_by_voyage`] / [`campaign_aggregate`]) SEES an expedition linked ONLY
/// via `arrow-kanban update EX-X --relate implements:VY-Y`.
///
/// `implements`/`spawns` are **typed-only** — they never project to the flat `related`
/// column (`relation_vocab::flat_column_for` returns `None` for them), unlike
/// `related`/`dependsOn`. The rollup resolves membership from `related` alone, so before
/// this fold an `implements`-only expedition was invisible to the group and it rendered
/// `[0/0]` (the typed-membership repro: expeditions under voyages linked only by typed edges).
///
/// **Query-side only, no data change.** This mutates the in-memory `ItemInfo` set built
/// for a roadmap view; the stored edges are untouched. The projection semantics of
/// `implements` stay deliberately typed-only in the store — this resolves
/// them at read time exactly where the rollup needs them.
pub fn fold_typed_voyage_memberships(items: &mut [ItemInfo], rel_store: &RelationsStore) {
    let voyage_ids: HashSet<String> = items
        .iter()
        .filter(|i| i.item_type == "voyage")
        .map(|i| i.id.clone())
        .collect();
    if voyage_ids.is_empty() {
        return;
    }

    // child id → the voyage ids to fold into its `related`.
    let mut add: HashMap<String, Vec<String>> = HashMap::new();

    // `implements` is stored E→V (source = expedition). The members of voyage V are the
    // SOURCES of `implements` edges pointing at V.
    for v in &voyage_ids {
        for e in rel_store.incoming_by_predicate(v, "implements") {
            add.entry(e).or_default().push(v.clone());
        }
    }

    // `spawns` is stored V→E (source = voyage; it is `implements`' inverse, and the store
    // keeps only ONE direction). The voyages that spawn expedition E are the SOURCES of
    // `spawns` edges pointing at E — keep only those that are voyages in this item set.
    for it in items.iter() {
        if it.item_type == "voyage" {
            continue;
        }
        for v in rel_store.incoming_by_predicate(&it.id, "spawns") {
            if voyage_ids.contains(&v) {
                add.entry(it.id.clone()).or_default().push(v);
            }
        }
    }

    // Apply: fold each resolved voyage id into `related`, skipping duplicates so an
    // expedition linked via BOTH the flat column and a typed edge is counted once.
    for it in items.iter_mut() {
        if let Some(voyages) = add.get(&it.id) {
            for v in voyages {
                if !it.related.contains(v) {
                    it.related.push(v.clone());
                }
            }
        }
    }
}

pub fn group_by_voyage(items: &[ItemInfo]) -> (Vec<VoyageGroup>, Vec<String>) {
    let item_map: HashMap<&str, &ItemInfo> = items.iter().map(|i| (i.id.as_str(), i)).collect();

    // Find all voyages
    let voyages: Vec<&ItemInfo> = items.iter().filter(|i| i.item_type == "voyage").collect();

    let voyage_ids: HashSet<&str> = voyages.iter().map(|v| v.id.as_str()).collect();

    // Map items to their voyage(s)
    let mut voyage_items: HashMap<&str, Vec<String>> = HashMap::new();
    let mut assigned_to_voyage: HashSet<&str> = HashSet::new();

    for item in items {
        if item.item_type == "voyage" {
            continue; // Voyages themselves aren't children
        }
        for rel in &item.related {
            if voyage_ids.contains(rel.as_str()) {
                voyage_items
                    .entry(rel.as_str())
                    .or_default()
                    .push(item.id.clone());
                assigned_to_voyage.insert(item.id.as_str());
            }
        }
    }

    // Build voyage groups
    let mut groups: Vec<VoyageGroup> = Vec::new();
    for voyage in &voyages {
        // Skip done voyages with no active children
        let children = voyage_items
            .get(voyage.id.as_str())
            .cloned()
            .unwrap_or_default();
        let done_count = children
            .iter()
            .filter(|id| {
                item_map
                    .get(id.as_str())
                    .map(|i| i.status == "done" || i.status == "complete" || i.status == "retired")
                    .unwrap_or(false)
            })
            .count();
        let total = children.len();

        // Skip voyages that are done AND have no active children
        if (voyage.status == "done" || voyage.status == "complete") && done_count == total {
            continue;
        }

        groups.push(VoyageGroup {
            voyage_id: voyage.id.clone(),
            voyage_title: voyage.title.clone(),
            voyage_status: voyage.status.clone(),
            items: children,
            done_count,
            total_count: total,
        });
    }

    // Sort voyages: in_progress first, then backlog, then by ID
    groups.sort_by(|a, b| {
        let sa = status_sort_order(&a.voyage_status);
        let sb = status_sort_order(&b.voyage_status);
        sa.cmp(&sb).then_with(|| a.voyage_id.cmp(&b.voyage_id))
    });

    // Orphan items (not assigned to any voyage, not a voyage, not done)
    let orphans: Vec<String> = items
        .iter()
        .filter(|i| {
            i.item_type != "voyage"
                && !assigned_to_voyage.contains(i.id.as_str())
                && i.status != "done"
                && i.status != "complete"
                && i.status != "retired"
        })
        .map(|i| i.id.clone())
        .collect();

    (groups, orphans)
}

// ─── Worklist ───────────────────────────────────────────────────────────────

/// Distinct assignees present on the board (excluding the `unassigned` sentinel and
/// blanks), sorted. This is the generic worklist agent set when the caller passes no
/// `--agents` filter — the open engine derives the roster from the DATA, never a
/// hardcoded fleet list.
pub fn agents_from_items(items: &[ItemInfo]) -> Vec<String> {
    let mut seen: Vec<String> = items
        .iter()
        .map(|i| i.assignee.trim().to_string())
        .filter(|a| !a.is_empty() && a != "unassigned")
        .collect();
    seen.sort();
    seen.dedup();
    seen
}

// ─── Capability-aware routing ────────────────────────────────────────────────

/// Tag-grammar suffix that declares a required capability: a tag `"<cap>-required"`
/// means the item requires the capability `"<cap>"` (e.g. `"gpu-required"` → `"gpu"`).
/// The engine fixes only this *grammar* — never the capability NAMES, and never any
/// agent/node name. Which capabilities exist, and which agents provide them, is
/// consumer policy (e.g. a fleet routes `gpu-required` work to its GPU nodes).
pub const CAPABILITY_TAG_SUFFIX: &str = "-required";

/// Generic capability-routing configuration consumed by [`generate_worklist`].
///
/// * `item_requirements` — item id → capabilities that item needs to be worked.
/// * `agent_capabilities` — agent → capabilities that agent provides.
///
/// Both halves are supplied by the CONSUMER. The engine assigns no meaning to any
/// specific capability or agent; it only enforces the generic rule *"withhold an
/// item from an agent that does not provide every capability the item requires."*
/// An empty config imposes no constraints (every item routes to every agent), so
/// the [`Default`] is a routing no-op. This replaced a hardcoded
/// `assignee == <node-name>` worklist rule: the specific `gpu-required`→GPU-node
/// mapping is now consumer policy, never baked into the open engine.
#[derive(Debug, Clone, Default)]
pub struct CapabilityRouting {
    pub item_requirements: HashMap<String, Vec<String>>,
    pub agent_capabilities: HashMap<String, HashSet<String>>,
}

impl CapabilityRouting {
    /// `true` if `agent` may be assigned `item_id` — i.e. it provides every
    /// capability the item requires. An item with no requirement routes to anyone.
    ///
    /// **No agent-capability policy → no constraint.** `item_requirements` is
    /// auto-derived from the board's `-required` tags (see
    /// [`item_requirements_from_batches`]), so the routing config is populated even
    /// when the consumer declares NO agent capabilities. In that state there is no
    /// basis to route BY capability, so every item routes to every agent — the same
    /// "no-op default" the type documents, now holding for the WIRED default (derived
    /// requirements + empty policy), not only the pure `::default()`. Withholding a
    /// `-required` item from everyone here would silently starve the whole board (the
    /// old rule keyed on `assignee`, not tags, so it routed such work to everyone).
    ///
    /// **Once ANY agent capability is declared, the constraint applies:** an item that
    /// requires a capability the given agent does not provide — including an agent
    /// absent from a non-empty policy, which provides nothing — is withheld from it.
    /// The consumer owns the agent-capability map; that is the intended semantics.
    pub fn agent_can_take(&self, item_id: &str, agent: &str) -> bool {
        // No provider policy at all → capability routing is inert (route to everyone).
        // Guards the WIRED default: derived requirements + no declared capabilities.
        // Without this, an always-populated `item_requirements` drives every
        // `-required` item to the fail-closed `None` arm below → total starvation.
        if self.agent_capabilities.is_empty() {
            return true;
        }
        match self.item_requirements.get(item_id) {
            None => true,
            Some(reqs) if reqs.is_empty() => true,
            Some(reqs) => match self.agent_capabilities.get(agent) {
                Some(provided) => reqs.iter().all(|c| provided.contains(c)),
                None => false,
            },
        }
    }
}

/// Parse the capabilities an item requires from its tags via the `<cap>-required`
/// convention (see [`CAPABILITY_TAG_SUFFIX`]). `["gpu-required", "v20"]` → `["gpu"]`.
/// An empty result means the item imposes no capability requirement.
pub fn required_capabilities_from_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .filter_map(|t| t.trim().strip_suffix(CAPABILITY_TAG_SUFFIX))
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string())
        .collect()
}

/// Parse a flat `Agent=cap1,cap2;Agent2=cap3` spec into the agent→capabilities map.
/// A convenience for consumers that carry the config as a string (a CLI flag, a NATS
/// request); it bakes in no roster or capability names — only the `;`/`=`/`,` grammar.
pub fn parse_agent_capabilities(spec: &str) -> HashMap<String, HashSet<String>> {
    let mut map: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in spec.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some((agent, caps)) = entry.split_once('=') {
            let set: HashSet<String> = caps
                .split(',')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(String::from)
                .collect();
            // Union on a repeated agent — last-wins would silently DROP an earlier
            // declaration's capabilities (`node-a=gpu;node-a=cuda` must yield both).
            map.entry(agent.trim().to_string()).or_default().extend(set);
        }
    }
    map
}

/// Build the `item_requirements` half of a [`CapabilityRouting`] from the board's
/// items, reading each item's tags. The consumer combines this with its own
/// agent-capability policy to complete the routing config.
pub fn item_requirements_from_batches(batches: &[RecordBatch]) -> HashMap<String, Vec<String>> {
    let mut reqs = HashMap::new();
    for batch in batches {
        let ids = col_str(batch, items_col::ID);
        let tags_col = batch
            .column(items_col::TAGS)
            .as_any()
            .downcast_ref::<ListArray>()
            .expect("tags column");
        for i in 0..batch.num_rows() {
            let tags = extract_list_values(tags_col, i);
            let caps = required_capabilities_from_tags(&tags);
            if !caps.is_empty() {
                reqs.insert(ids.value(i).to_string(), caps);
            }
        }
    }
    reqs
}

/// Generate agent work assignments based on ready items and current assignments.
///
/// `routing` supplies capability-aware routing (generic — no hardcoded roster); pass
/// [`CapabilityRouting::default`] for no capability constraints.
pub fn generate_worklist(
    items: &[ItemInfo],
    cp: &CriticalPathResult,
    agents: &[String],
    routing: &CapabilityRouting,
    depth_limit: usize,
) -> Vec<WorklistEntry> {
    let item_map: HashMap<&str, &ItemInfo> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    let blocked_set: HashSet<&str> = cp.blocked.iter().map(|s| s.as_str()).collect();

    // Items currently in progress (already assigned)
    let in_progress: HashMap<&str, &str> = items
        .iter()
        .filter(|i| i.status == "in_progress" && i.assignee != "-")
        .map(|i| (i.assignee.as_str(), i.id.as_str()))
        .collect();

    let mut entries: Vec<WorklistEntry> = Vec::new();

    for agent in agents {
        let mut agent_items: Vec<WorklistItem> = Vec::new();

        // First: current in-progress work
        if let Some(&current_id) = in_progress.get(agent.as_str())
            && let Some(info) = item_map.get(current_id)
        {
            agent_items.push(WorklistItem {
                id: current_id.to_string(),
                title: truncate(&info.title, 40),
                blocked: false,
            });
        }

        // Then: ready items suitable for this agent
        let mut available: Vec<&str> = cp
            .ready
            .iter()
            .filter(|id| {
                let info = item_map.get(id.as_str());
                if let Some(info) = info {
                    // Skip items already in progress
                    if info.status != "backlog" {
                        return false;
                    }
                    // If assigned to a specific agent, only show for that agent
                    if info.assignee != "-" && info.assignee != *agent {
                        return false;
                    }
                    // Capability-aware routing: withhold work whose required
                    // capabilities this agent does not provide. Generic — no
                    // hardcoded node names; the consumer supplies the map.
                    if !routing.agent_can_take(&info.id, agent) {
                        return false;
                    }
                    true
                } else {
                    false
                }
            })
            .map(|s| s.as_str())
            .collect();

        // Sort by: assigned-to-this-agent first, then by manual rank, then by downstream count (bottlenecks first), then by
        // priority-string fallback.
        available.sort_by(|&a, &b| {
            let a_assigned = item_map
                .get(a)
                .map(|i| i.assignee == *agent)
                .unwrap_or(false);
            let b_assigned = item_map
                .get(b)
                .map(|i| i.assignee == *agent)
                .unwrap_or(false);
            b_assigned
                .cmp(&a_assigned)
                .then_with(|| {
                    let ka = item_map.get(a).map(|i| sort_key(i)).unwrap_or((2, 0, 99));
                    let kb = item_map.get(b).map(|i| sort_key(i)).unwrap_or((2, 0, 99));
                    ka.cmp(&kb)
                })
                .then_with(|| {
                    let da = cp.downstream_count.get(a).copied().unwrap_or(0);
                    let db = cp.downstream_count.get(b).copied().unwrap_or(0);
                    db.cmp(&da)
                })
        });

        // Fill up to depth_limit
        for &id in &available {
            if agent_items.len() >= depth_limit {
                break;
            }
            // Skip if already assigned to another agent in this worklist
            if let Some(info) = item_map.get(id) {
                agent_items.push(WorklistItem {
                    id: id.to_string(),
                    title: truncate(&info.title, 40),
                    blocked: blocked_set.contains(id),
                });
            }
        }

        // Add next blocked items that will become ready
        if agent_items.len() < depth_limit {
            for id in &cp.blocked {
                if agent_items.len() >= depth_limit {
                    break;
                }
                if let Some(info) = item_map.get(id.as_str()) {
                    if info.status != "backlog" {
                        continue;
                    }
                    if info.assignee != "-" && info.assignee != *agent {
                        continue;
                    }
                    // Capability-aware routing (see the ready-items filter above).
                    if !routing.agent_can_take(&info.id, agent) {
                        continue;
                    }
                    // Check if this will unblock after current agent work completes
                    agent_items.push(WorklistItem {
                        id: id.clone(),
                        title: truncate(&info.title, 40),
                        blocked: true,
                    });
                }
            }
        }

        entries.push(WorklistEntry {
            agent: agent.clone(),
            items: agent_items,
        });
    }

    entries
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn col_str(batch: &RecordBatch, col: usize) -> &StringArray {
    batch
        .column(col)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("string column")
}

fn extract_list_values(list_col: &ListArray, row: usize) -> Vec<String> {
    if list_col.is_null(row) {
        return Vec::new();
    }
    let values = list_col.value(row);
    if values.is_empty() {
        return Vec::new();
    }
    let strings = values
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("list values");
    (0..strings.len())
        .map(|j| strings.value(j).to_string())
        .collect()
}

pub fn priority_rank(p: &str) -> i32 {
    match p.to_lowercase().as_str() {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 99,
    }
}

/// "Highest priority first" sort key for ordering items in roadmap / worklist /
/// next / ready views. Items with a manual `rank` sort
/// before unranked items; among ranked items, lower rank value wins; ties are
/// broken by the priority-string ordering (`critical` < `high` < ...).
///
/// Returned tuple `(rank_bucket, rank_value, priority_rank)` is ascending-sortable:
/// - bucket 0: ranked items (Some(n))
/// - bucket 1: unranked items (None)
///
/// The `99` fallback for missing items pushes them to a bucket beyond both.
pub fn sort_key(item: &ItemInfo) -> (u8, i32, i32) {
    let priority = priority_rank(&item.priority);
    match item.rank {
        Some(r) => (0, r, priority),
        None => (1, 0, priority),
    }
}

fn status_sort_order(s: &str) -> i32 {
    match s {
        "in_progress" => 0,
        "backlog" => 1,
        "review" => 2,
        "done" | "complete" | "retired" => 3,
        _ => 4,
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max - 3).collect();
        format!("{t}...")
    }
}

// ─── Display Formatting ─────────────────────────────────────────────────────

/// roll a campaign's member voyages up into `(member_count, done_expeditions,
/// total_expeditions)` — the program aggregate a flat tag can never give. `members` is the set of
/// member voyage ids (the caller resolves it from `partOf` edges — data, never a hardcoded list).
///
/// Computed from the UNFILTERED item set on purpose: a member voyage that has already COMPLETED
/// must still count toward the program %-done. [`group_by_voyage`] deliberately DROPS fully-done
/// voyages (right for the active-work roadmap section below — they need no attention), but summing
/// the aggregate from those filtered groups would make program %-done *fall* the moment a member
/// voyage finishes — inverting the exact thing the rollup exists to show (DGX2). Only
/// child items of type `expedition` count — chores/signals are excluded so the "expeditions done"
/// label is honest. `member_count` counts the member voyages that actually exist as items (a stale
/// `partOf` id resolves to nothing). Zero members → `(0, 0, 0)` (an empty view, not an error).
pub fn campaign_aggregate(items: &[ItemInfo], members: &[String]) -> (usize, usize, usize) {
    let member_set: HashSet<&str> = members.iter().map(String::as_str).collect();
    // Member voyages present as items — the header/count is over these, including completed ones.
    let existing_members: HashSet<&str> = items
        .iter()
        .filter(|i| i.item_type == "voyage" && member_set.contains(i.id.as_str()))
        .map(|i| i.id.as_str())
        .collect();
    let mut done = 0;
    let mut total = 0;
    for item in items {
        if item.item_type != "expedition" {
            continue; // "expeditions done" counts expeditions only — not chores/signals.
        }
        // A child of a member voyage: its `related` names one (the same edge group_by_voyage uses).
        if item
            .related
            .iter()
            .any(|r| existing_members.contains(r.as_str()))
        {
            total += 1;
            if item.status == "done" || item.status == "complete" || item.status == "retired" {
                done += 1;
            }
        }
    }
    (existing_members.len(), done, total)
}

/// render a campaign's rollup as a String — the header, member count, program %-done
/// aggregate, and the shared roadmap over ONLY the member voyages (with a cross-voyage critical
/// path). `members` are the campaign's member voyage ids (the caller resolves them from `partOf`
/// edges — data, never a hardcoded list). Reuses [`format_roadmap`] — no parallel renderer. A
/// campaign with no members returns just the empty-view header (not an error). Shared by the
/// local CLI handler AND the server's `handle_roadmap`, so `--campaign` works in both modes.
pub fn format_campaign_roadmap(camp_id: &str, members: &[String], items: &[ItemInfo]) -> String {
    // Aggregate + member count come from the UNFILTERED items so completed members still count.
    let (member_count, done, total) = campaign_aggregate(items, members);

    let mut out = format!("Campaign {camp_id} — {member_count} member voyage(s) [partOf]:\n");
    if member_count == 0 {
        out.push_str(&format!(
            "  (no members yet — a voyage joins with: arrow-kanban update VY-XXXX --relate partOf:{camp_id})\n"
        ));
        return out;
    }
    let pct = (done * 100).checked_div(total).unwrap_or(0);
    out.push_str(&format!(
        "  Program: {done}/{total} expeditions done ({pct}%)\n\n"
    ));

    // The active-work roadmap section over ONLY the member voyages, with a cross-voyage critical
    // path over that subset. group_by_voyage DROPS fully-done member voyages here (they need no
    // active work) — the aggregate line above already counts them, so a completed member shows in
    // the %-done but not in this section.
    let (all_groups, _orphans) = group_by_voyage(items);
    let member_groups: Vec<VoyageGroup> = all_groups
        .into_iter()
        .filter(|g| members.contains(&g.voyage_id))
        .collect();
    let member_ids: HashSet<&str> = member_groups
        .iter()
        .flat_map(|g| {
            std::iter::once(g.voyage_id.as_str()).chain(g.items.iter().map(String::as_str))
        })
        .collect();
    let member_items: Vec<ItemInfo> = items
        .iter()
        .filter(|i| member_ids.contains(i.id.as_str()))
        .cloned()
        .collect();
    match compute_critical_path(&member_items) {
        Ok(cp) => out.push_str(&format_roadmap(&member_items, &member_groups, &[], &cp)),
        Err(e) => out.push_str(&format!("  (cross-voyage critical path error: {e})\n")),
    }
    out
}

pub fn format_roadmap(
    items: &[ItemInfo],
    groups: &[VoyageGroup],
    orphans: &[String],
    cp: &CriticalPathResult,
) -> String {
    let item_map: HashMap<&str, &ItemInfo> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    let blocked_set: HashSet<&str> = cp.blocked.iter().map(|s| s.as_str()).collect();
    let mut lines = Vec::new();

    for group in groups {
        let progress = format!(
            "[{}/{}{}]",
            group.done_count,
            group.total_count,
            if group.total_count > 0 { " done" } else { "" }
        );
        lines.push(format!(
            "{}: {} {}",
            group.voyage_id, group.voyage_title, progress
        ));

        // Sort items by depth (roots first), then priority
        let mut sorted_items = group.items.clone();
        sorted_items.sort_by(|a, b| {
            let da = cp.depth.get(a.as_str()).copied().unwrap_or(0);
            let db = cp.depth.get(b.as_str()).copied().unwrap_or(0);
            da.cmp(&db).then_with(|| {
                let ka = item_map
                    .get(a.as_str())
                    .map(|i| sort_key(i))
                    .unwrap_or((2, 0, 99));
                let kb = item_map
                    .get(b.as_str())
                    .map(|i| sort_key(i))
                    .unwrap_or((2, 0, 99));
                ka.cmp(&kb)
            })
        });

        for id in &sorted_items {
            if let Some(info) = item_map.get(id.as_str()) {
                let (icon, status_tag) = item_status_display(info, &blocked_set);
                let assignee_tag = if info.assignee != "-" {
                    format!("  {}", info.assignee)
                } else {
                    String::new()
                };
                let dep_tag = if blocked_set.contains(id.as_str()) {
                    let unmet: Vec<&str> = info
                        .depends_on
                        .iter()
                        .filter(|d| {
                            item_map
                                .get(d.as_str())
                                .map(|i| {
                                    i.status != "done"
                                        && i.status != "complete"
                                        && i.status != "retired"
                                })
                                .unwrap_or(false)
                        })
                        .map(|d| d.as_str())
                        .collect();
                    format!(" -> {}", unmet.join(", "))
                } else {
                    String::new()
                };
                let title = truncate(&info.title, 44);
                lines.push(format!(
                    "  {icon} {:<14}{:<46}{}{assignee_tag}{dep_tag}",
                    id, title, status_tag
                ));
            }
        }
        lines.push(String::new());
    }

    // Orphan section
    if !orphans.is_empty() {
        lines.push("Ungrouped:".to_string());
        for id in orphans {
            if let Some(info) = item_map.get(id.as_str()) {
                let (icon, status_tag) = item_status_display(info, &blocked_set);
                let assignee_tag = if info.assignee != "-" {
                    format!("  {}", info.assignee)
                } else {
                    String::new()
                };
                let title = truncate(&info.title, 44);
                lines.push(format!(
                    "  {icon} {:<14}{:<46}{}{assignee_tag}",
                    id, title, status_tag
                ));
            }
        }
        lines.push(String::new());
    }

    lines.join("\n")
}

/// Format critical path display.
pub fn format_critical_path(items: &[ItemInfo], cp: &CriticalPathResult) -> String {
    let item_map: HashMap<&str, &ItemInfo> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    let blocked_set: HashSet<&str> = cp.blocked.iter().map(|s| s.as_str()).collect();
    let max_depth = cp.depth_groups.keys().last().copied().unwrap_or(0);
    let total_items: usize = cp.depth_groups.values().map(|v| v.len()).sum();

    let mut lines = Vec::new();
    lines.push(format!(
        "Critical Path ({} items, {} depth levels):",
        total_items,
        max_depth + 1
    ));
    lines.push(String::new());

    for (&depth_level, group) in &cp.depth_groups {
        let label = if depth_level == 0 {
            "start now".to_string()
        } else {
            format!("after depth {}", depth_level - 1)
        };
        let parallel = if group.len() > 1 {
            format!(" — {} parallel", group.len())
        } else {
            String::new()
        };
        lines.push(format!("Depth {} ({label}{parallel}):", depth_level));

        for id in group {
            if let Some(info) = item_map.get(id.as_str()) {
                let assignee = if info.assignee != "-" {
                    format!("{:<8}", info.assignee)
                } else {
                    "Any     ".to_string()
                };
                let title = truncate(&info.title, 44);
                let dep_note = if blocked_set.contains(id.as_str()) {
                    let unmet: Vec<&str> = info
                        .depends_on
                        .iter()
                        .filter(|d| {
                            item_map
                                .get(d.as_str())
                                .map(|i| {
                                    i.status != "done"
                                        && i.status != "complete"
                                        && i.status != "retired"
                                })
                                .unwrap_or(false)
                        })
                        .map(|d| d.as_str())
                        .collect();
                    format!("  <- {}", unmet.join(", "))
                } else {
                    String::new()
                };
                lines.push(format!("  {:<14}{:<46}{assignee}{dep_note}", id, title));
            }
        }
        lines.push(String::new());
    }

    // Bottlenecks
    let mut bottlenecks: Vec<(&String, &usize)> = cp.downstream_count.iter().collect();
    bottlenecks.sort_by(|a, b| b.1.cmp(a.1));
    let top_bottlenecks: Vec<_> = bottlenecks
        .into_iter()
        .take(5)
        .filter(|(_, c)| **c > 0)
        .collect();

    if !top_bottlenecks.is_empty() {
        lines.push("Bottlenecks (most downstream dependents):".to_string());
        for (id, count) in top_bottlenecks {
            if let Some(info) = item_map.get(id.as_str()) {
                let title = truncate(&info.title, 40);
                lines.push(format!(
                    "  {:<14}{:<42} -> {} items depend",
                    id, title, count
                ));
            }
        }
        lines.push(String::new());
    }

    lines.join("\n")
}

/// Format worklist display.
pub fn format_worklist(entries: &[WorklistEntry]) -> String {
    let mut lines = Vec::new();

    // Header
    let headers: Vec<String> = entries.iter().map(|e| format!("{:<24}", e.agent)).collect();
    lines.push(format!("| {} |", headers.join(" | ")));
    let dividers: Vec<String> = entries.iter().map(|_| "-".repeat(24)).collect();
    lines.push(format!("| {} |", dividers.join(" | ")));

    // Find max rows
    let max_rows = entries.iter().map(|e| e.items.len()).max().unwrap_or(0);

    for row in 0..max_rows {
        let cells: Vec<String> = entries
            .iter()
            .map(|e| {
                if row < e.items.len() {
                    let item = &e.items[row];
                    let blocked_mark = if item.blocked { " ~" } else { "" };
                    let cell = format!("{} {}{}", item.id, truncate(&item.title, 10), blocked_mark);
                    format!("{:<24}", truncate(&cell, 24))
                } else {
                    " ".repeat(24)
                }
            })
            .collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }

    lines.push(String::new());
    lines.push("~ = blocked, will become ready after current work completes".to_string());

    lines.join("\n")
}

fn item_status_display(info: &ItemInfo, blocked: &HashSet<&str>) -> (&'static str, &'static str) {
    match info.status.as_str() {
        "done" | "complete" | "retired" => ("✓", "done"),
        "in_progress" => ("▶", "IN PROGRESS"),
        "review" => ("⊙", "REVIEW"),
        _ if blocked.contains(info.id.as_str()) => ("◇", "BLOCKED"),
        _ => ("→", "READY"),
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_items() -> Vec<ItemInfo> {
        vec![
            ItemInfo {
                id: "EX-1".into(),
                title: "Root task A".into(),
                item_type: "expedition".into(),
                status: "backlog".into(),
                priority: "critical".into(),
                assignee: "-".into(),
                related: vec!["VY-1".into()],
                depends_on: vec![],
                rank: None,
            },
            ItemInfo {
                id: "EX-2".into(),
                title: "Root task B".into(),
                item_type: "expedition".into(),
                status: "backlog".into(),
                priority: "high".into(),
                assignee: "DGX".into(),
                related: vec!["VY-1".into()],
                depends_on: vec![],
                rank: None,
            },
            ItemInfo {
                id: "EX-3".into(),
                title: "Depends on A".into(),
                item_type: "expedition".into(),
                status: "backlog".into(),
                priority: "high".into(),
                assignee: "-".into(),
                related: vec!["VY-1".into()],
                depends_on: vec!["EX-1".into()],
                rank: None,
            },
            ItemInfo {
                id: "EX-4".into(),
                title: "Depends on A and B".into(),
                item_type: "expedition".into(),
                status: "backlog".into(),
                priority: "critical".into(),
                assignee: "-".into(),
                related: vec!["VY-1".into()],
                depends_on: vec!["EX-1".into(), "EX-2".into()],
                rank: None,
            },
            ItemInfo {
                id: "EX-5".into(),
                title: "Depends on 3 and 4".into(),
                item_type: "expedition".into(),
                status: "backlog".into(),
                priority: "medium".into(),
                assignee: "-".into(),
                related: vec!["VY-1".into()],
                depends_on: vec!["EX-3".into(), "EX-4".into()],
                rank: None,
            },
            ItemInfo {
                id: "VY-1".into(),
                title: "Test Voyage".into(),
                item_type: "voyage".into(),
                status: "backlog".into(),
                priority: "critical".into(),
                assignee: "-".into(),
                related: vec![],
                depends_on: vec![],
                rank: None,
            },
            ItemInfo {
                id: "EX-6".into(),
                title: "Orphan task".into(),
                item_type: "expedition".into(),
                status: "backlog".into(),
                priority: "low".into(),
                assignee: "-".into(),
                related: vec![],
                depends_on: vec![],
                rank: None,
            },
            ItemInfo {
                id: "EX-99".into(),
                title: "Done task".into(),
                item_type: "expedition".into(),
                status: "done".into(),
                priority: "high".into(),
                assignee: "M5".into(),
                related: vec!["VY-1".into()],
                depends_on: vec![],
                rank: None,
            },
        ]
    }

    #[test]
    fn test_topological_sort_ordering() {
        let items = make_items();
        let cp = compute_critical_path(&items).unwrap();

        // EX-1 and EX-2 should come before EX-3 and EX-4
        let pos = |id: &str| cp.ordered.iter().position(|x| x == id).unwrap();
        assert!(pos("EX-1") < pos("EX-3"));
        assert!(pos("EX-1") < pos("EX-4"));
        assert!(pos("EX-2") < pos("EX-4"));
        assert!(pos("EX-3") < pos("EX-5"));
        assert!(pos("EX-4") < pos("EX-5"));
    }

    #[test]
    fn test_depth_computation() {
        let items = make_items();
        let cp = compute_critical_path(&items).unwrap();

        assert_eq!(cp.depth["EX-1"], 0);
        assert_eq!(cp.depth["EX-2"], 0);
        assert_eq!(cp.depth["EX-3"], 1);
        assert_eq!(cp.depth["EX-4"], 1);
        assert_eq!(cp.depth["EX-5"], 2);
    }

    #[test]
    fn test_ready_vs_blocked() {
        let items = make_items();
        let cp = compute_critical_path(&items).unwrap();

        // EX-1, EX-2, VY-1, EX-6 are ready (no active deps)
        assert!(cp.ready.contains(&"EX-1".to_string()));
        assert!(cp.ready.contains(&"EX-2".to_string()));
        assert!(cp.ready.contains(&"EX-6".to_string()));

        // EX-3, EX-4, EX-5 are blocked
        assert!(cp.blocked.contains(&"EX-3".to_string()));
        assert!(cp.blocked.contains(&"EX-4".to_string()));
        assert!(cp.blocked.contains(&"EX-5".to_string()));
    }

    #[test]
    fn test_done_items_excluded() {
        let items = make_items();
        let cp = compute_critical_path(&items).unwrap();

        // EX-99 is done, should not appear in any output
        assert!(!cp.ordered.contains(&"EX-99".to_string()));
        assert!(!cp.ready.contains(&"EX-99".to_string()));
        assert!(!cp.blocked.contains(&"EX-99".to_string()));
    }

    #[test]
    fn test_downstream_count() {
        let items = make_items();
        let cp = compute_critical_path(&items).unwrap();

        // EX-1 -> EX-3, EX-4 -> EX-5 (3 transitive)
        assert!(cp.downstream_count["EX-1"] >= 3);
        // EX-5 has no dependents
        assert_eq!(cp.downstream_count["EX-5"], 0);
    }

    #[test]
    fn test_cycle_detection() {
        let items = vec![
            ItemInfo {
                id: "A".into(),
                title: "A".into(),
                item_type: "expedition".into(),
                status: "backlog".into(),
                priority: "-".into(),
                assignee: "-".into(),
                related: vec![],
                depends_on: vec!["B".into()],
                rank: None,
            },
            ItemInfo {
                id: "B".into(),
                title: "B".into(),
                item_type: "expedition".into(),
                status: "backlog".into(),
                priority: "-".into(),
                assignee: "-".into(),
                related: vec![],
                depends_on: vec!["A".into()],
                rank: None,
            },
        ];
        let result = compute_critical_path(&items);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cycle"));
    }

    #[test]
    fn test_voyage_grouping() {
        let items = make_items();
        let (groups, orphans) = group_by_voyage(&items);

        // VY-1 should have EX-1 through EX-5 + EX-99
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].voyage_id, "VY-1");
        assert_eq!(groups[0].total_count, 6); // EX-1..5 + EX-99
        assert_eq!(groups[0].done_count, 1); // EX-99

        // EX-6 is an orphan
        assert!(orphans.contains(&"EX-6".to_string()));
    }

    // ── Typed implements/spawns voyage-membership folding ──────────────────────

    /// A bare expedition + voyage with NO flat `related` link between them — the
    /// state the repro hit (`--relate implements:VY-X` writes no flat column).
    fn ex(id: &str) -> ItemInfo {
        ItemInfo {
            id: id.into(),
            title: id.into(),
            item_type: "expedition".into(),
            status: "backlog".into(),
            priority: "high".into(),
            assignee: "-".into(),
            related: vec![],
            depends_on: vec![],
            rank: None,
        }
    }
    fn vy(id: &str) -> ItemInfo {
        ItemInfo {
            item_type: "voyage".into(),
            ..ex(id)
        }
    }

    #[test]
    fn fold_resolves_implements_edge_into_related() {
        // EX-9 implements VY-9 (typed-only; no flat related) — before the fold it is
        // invisible to grouping; after, it groups under VY-9.
        let mut items = vec![ex("EX-9"), vy("VY-9")];
        let mut rels = RelationsStore::new();
        rels.add_relation("EX-9", "VY-9", "implements").unwrap();

        // Control: without the fold the voyage renders but with ZERO children — the exact
        // [0/0] the chore filed — and EX-9 is an orphan.
        let (groups_before, orphans_before) = group_by_voyage(&items);
        let vy9_before = groups_before.iter().find(|g| g.voyage_id == "VY-9");
        assert_eq!(
            vy9_before.map(|g| g.total_count),
            Some(0),
            "the [0/0] bug: voyage present but no children before the fold"
        );
        assert!(orphans_before.contains(&"EX-9".to_string()));

        fold_typed_voyage_memberships(&mut items, &rels);
        assert!(
            items[0].related.contains(&"VY-9".to_string()),
            "implements target folded into related"
        );
        let (groups, orphans) = group_by_voyage(&items);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].voyage_id, "VY-9");
        assert_eq!(groups[0].total_count, 1, "EX-9 now counts toward VY-9");
        assert!(!orphans.contains(&"EX-9".to_string()));
    }

    #[test]
    fn fold_resolves_spawns_edge_the_inverse_direction() {
        // The membership stored the OTHER way: VY-8 spawns EX-8 (voyage as source).
        let mut items = vec![ex("EX-8"), vy("VY-8")];
        let mut rels = RelationsStore::new();
        rels.add_relation("VY-8", "EX-8", "spawns").unwrap();

        fold_typed_voyage_memberships(&mut items, &rels);
        assert!(items[0].related.contains(&"VY-8".to_string()));
        let (groups, _) = group_by_voyage(&items);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].total_count, 1);
    }

    #[test]
    fn fold_does_not_double_count_when_flat_edge_also_present() {
        // Belt-and-suspenders: an expedition linked via BOTH the flat related column
        // AND a typed implements edge must appear once, not twice.
        let mut ex7 = ex("EX-7");
        ex7.related = vec!["VY-7".into()];
        let mut items = vec![ex7, vy("VY-7")];
        let mut rels = RelationsStore::new();
        rels.add_relation("EX-7", "VY-7", "implements").unwrap();

        fold_typed_voyage_memberships(&mut items, &rels);
        assert_eq!(
            items[0].related.iter().filter(|r| *r == "VY-7").count(),
            1,
            "VY-7 appears once, not duplicated"
        );
        let (groups, _) = group_by_voyage(&items);
        assert_eq!(groups[0].total_count, 1, "counted once");
    }

    #[test]
    fn fold_is_a_noop_without_voyages_or_edges() {
        // No voyages → early return, no panic, related untouched.
        let mut items = vec![ex("EX-1"), ex("EX-2")];
        fold_typed_voyage_memberships(&mut items, &RelationsStore::new());
        assert!(items.iter().all(|i| i.related.is_empty()));

        // Voyage present but no edges → nothing folded.
        let mut items2 = vec![ex("EX-1"), vy("VY-1")];
        fold_typed_voyage_memberships(&mut items2, &RelationsStore::new());
        assert!(items2[0].related.is_empty());
    }

    #[test]
    fn fold_campaign_aggregate_counts_implements_only_expedition() {
        // The headline repro: campaign_aggregate (program %-done) must count an
        // implements-only expedition once the fold has run.
        let mut items = vec![ex("EX-9"), vy("VY-9")];
        let mut rels = RelationsStore::new();
        rels.add_relation("EX-9", "VY-9", "implements").unwrap();
        let members = vec!["VY-9".to_string()];

        // Before: 0 total (the [0/0] the chore filed).
        let (_, _, total_before) = campaign_aggregate(&items, &members);
        assert_eq!(total_before, 0);

        fold_typed_voyage_memberships(&mut items, &rels);
        let (member_count, done, total) = campaign_aggregate(&items, &members);
        assert_eq!(member_count, 1);
        assert_eq!(total, 1, "the implements-only expedition now counts");
        assert_eq!(done, 0);
    }

    #[test]
    fn test_depth_groups() {
        let items = make_items();
        let cp = compute_critical_path(&items).unwrap();

        // Depth 0: EX-1, EX-2, VY-1, EX-6
        assert!(cp.depth_groups[&0].contains(&"EX-1".to_string()));
        assert!(cp.depth_groups[&0].contains(&"EX-2".to_string()));

        // Depth 2: EX-5
        assert!(cp.depth_groups[&2].contains(&"EX-5".to_string()));
    }

    #[test]
    fn test_worklist_generation() {
        let items = make_items();
        let cp = compute_critical_path(&items).unwrap();
        let agents = vec!["DGX".to_string(), "M5".to_string(), "Mini".to_string()];
        let worklist = generate_worklist(&items, &cp, &agents, &CapabilityRouting::default(), 3);

        assert_eq!(worklist.len(), 3);
        // DGX should get EX-2 (assigned to DGX)
        let dgx = &worklist[0];
        assert_eq!(dgx.agent, "DGX");
        assert!(dgx.items.iter().any(|i| i.id == "EX-2"));
    }

    // ─── CH-6824: generic capability-aware worklist routing ─────────────────────

    /// Capability-tagged work is routed ONLY to agents that provide the required
    /// capability — the generic replacement for the removed hardcoded `assignee ==
    /// "DGX"` rule. Node names here are deliberately synthetic (`node-a`/`node-b`) to
    /// prove the engine keys off a capability predicate, not any fleet roster.
    #[test]
    fn worklist_routes_capability_tagged_work_only_to_capable_agents() {
        let items = vec![ItemInfo {
            id: "EX-CAP".into(),
            title: "needs the accel capability".into(),
            item_type: "expedition".into(),
            status: "backlog".into(),
            priority: "high".into(),
            assignee: "-".into(),
            related: vec![],
            depends_on: vec![],
            rank: None,
        }];
        let cp = compute_critical_path(&items).unwrap();
        let agents = vec!["node-a".to_string(), "node-b".to_string()];

        // node-a provides "accel"; node-b provides nothing.
        let mut agent_capabilities: HashMap<String, HashSet<String>> = HashMap::new();
        agent_capabilities.insert("node-a".to_string(), HashSet::from(["accel".to_string()]));
        let routing = CapabilityRouting {
            item_requirements: HashMap::from([("EX-CAP".to_string(), vec!["accel".to_string()])]),
            agent_capabilities,
        };

        let worklist = generate_worklist(&items, &cp, &agents, &routing, 3);
        let node_a = worklist.iter().find(|e| e.agent == "node-a").unwrap();
        let node_b = worklist.iter().find(|e| e.agent == "node-b").unwrap();

        assert!(
            node_a.items.iter().any(|i| i.id == "EX-CAP"),
            "a capable agent MUST be routed capability-tagged work"
        );
        assert!(
            !node_b.items.iter().any(|i| i.id == "EX-CAP"),
            "a non-capable agent must NOT be routed capability-tagged work"
        );

        // Control: with no routing config the same item routes to everyone
        // (capability routing is opt-in; the default is a no-op).
        let open = generate_worklist(&items, &cp, &agents, &CapabilityRouting::default(), 3);
        assert!(
            open.iter()
                .all(|e| e.items.iter().any(|i| i.id == "EX-CAP")),
            "with no capability config the item routes to every agent"
        );
    }

    /// REGRESSION (PR #6 review, Mini): the dangerous MIDDLE state the pure-`::default()`
    /// control above cannot reach — `item_requirements` DERIVED (populated) but no agent
    /// capability policy declared. Before the empty-policy guard, this fell to the
    /// fail-closed `None => false` arm and withheld the `-required` item from EVERY
    /// agent (silent total starvation). The predicate must treat "no policy" as a no-op.
    #[test]
    fn agent_can_take_wired_default_no_policy_routes_required_items_to_everyone() {
        let routing = CapabilityRouting {
            item_requirements: HashMap::from([("EX-CAP".to_string(), vec!["accel".to_string()])]),
            agent_capabilities: HashMap::new(), // no policy supplied — the WIRED default
        };
        assert!(
            routing.agent_can_take("EX-CAP", "node-a"),
            "with requirements derived but NO provider policy, a required item must route to anyone"
        );
        assert!(
            routing.agent_can_take("EX-CAP", "node-z"),
            "...to EVERY agent — no policy means no capability constraint, not fail-closed"
        );
    }

    /// End-to-end proof of the same regression through the real `generate_worklist`
    /// path (my review's "test the WIRED path, not just `::default()`"): a board with a
    /// `-required` item but no agent-capability policy must route it to every agent, not
    /// starve it. This is the case the existing control (both halves empty) skips.
    #[test]
    fn worklist_wired_default_no_policy_does_not_starve_required_items() {
        let items = vec![ItemInfo {
            id: "EX-CAP".into(),
            title: "needs the accel capability".into(),
            item_type: "expedition".into(),
            status: "backlog".into(),
            priority: "high".into(),
            assignee: "-".into(),
            related: vec![],
            depends_on: vec![],
            rank: None,
        }];
        let cp = compute_critical_path(&items).unwrap();
        let agents = vec!["node-a".to_string(), "node-b".to_string()];

        // WIRED default: requirements auto-derived, but the consumer supplied NO policy.
        let routing = CapabilityRouting {
            item_requirements: HashMap::from([("EX-CAP".to_string(), vec!["accel".to_string()])]),
            agent_capabilities: HashMap::new(),
        };
        let worklist = generate_worklist(&items, &cp, &agents, &routing, 3);
        assert!(
            worklist
                .iter()
                .all(|e| e.items.iter().any(|i| i.id == "EX-CAP")),
            "a -required item with no provider policy must route to EVERY agent (no silent starvation)"
        );
    }

    /// AMEND (PR #6 review): a repeated agent in the flat spec must UNION its
    /// capabilities, not overwrite — last-wins silently dropped an earlier decl's caps.
    #[test]
    fn parse_agent_capabilities_unions_a_repeated_agent() {
        let map = parse_agent_capabilities("node-a=gpu;node-a=cuda");
        assert_eq!(
            map.get("node-a"),
            Some(&HashSet::from(["gpu".to_string(), "cuda".to_string()])),
            "repeated agent decls must union (gpu ∪ cuda), not last-wins to {{cuda}}"
        );
    }

    /// The `<cap>-required` tag grammar yields the capability names (and nothing else).
    #[test]
    fn required_capabilities_parses_the_required_suffix_grammar() {
        let tags = vec![
            "gpu-required".to_string(),
            "v20".to_string(),
            "cuda-required".to_string(),
            "-required".to_string(), // empty prefix → not a capability
        ];
        let caps = required_capabilities_from_tags(&tags);
        assert_eq!(caps, vec!["gpu".to_string(), "cuda".to_string()]);
    }

    /// The flat `Agent=cap,cap;...` spec (CLI flag / NATS request) parses to the
    /// agent→capabilities map, tolerating whitespace and empty/blank entries.
    #[test]
    fn parse_agent_capabilities_reads_the_flat_spec() {
        let map = parse_agent_capabilities(" node-a = gpu, cuda ; node-b = ; ");
        assert_eq!(
            map.get("node-a"),
            Some(&HashSet::from(["gpu".to_string(), "cuda".to_string()]))
        );
        // `node-b=` → present with an empty capability set.
        assert_eq!(map.get("node-b"), Some(&HashSet::new()));
        assert_eq!(map.len(), 2);
        assert!(parse_agent_capabilities("").is_empty());
    }

    /// Closed-vocabulary grep-guard (mirrors the FOSS export-gate discipline): the
    /// generic engine's PRODUCTION routing logic must hardcode NO fleet/node name.
    /// Roster→capability policy lives in the consumer, never here. Test fixtures
    /// below the `#[cfg(test)]` marker legitimately use real names, so only the
    /// production region (before that marker) is scanned. Reintroducing the old
    /// `assignee == "DGX"` rule turns this RED — exactly as the export gate blocks
    /// brand/vocab leaks into the open tree.
    #[test]
    fn engine_routing_hardcodes_no_roster_names() {
        let src = include_str!("critical_path.rs");
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("source has a production region before the test module");
        const FORBIDDEN: &[&str] = &[
            "DGX", "DGX1", "DGX2", "M5", "Mini", "Air", "negaDGX", "negaM5", "negaMini",
        ];
        for name in FORBIDDEN {
            let needle = format!("\"{name}\"");
            assert!(
                !production.contains(needle.as_str()),
                "hardcoded roster name {needle} found in the generic engine's production \
                 code — route on a capability predicate, not a node name (CH-6824)"
            );
        }
    }

    #[test]
    fn test_empty_items() {
        let items: Vec<ItemInfo> = vec![];
        let cp = compute_critical_path(&items).unwrap();
        assert!(cp.ordered.is_empty());
        assert!(cp.ready.is_empty());
        assert!(cp.blocked.is_empty());
    }

    // ─── Rank-based sort ───────────────────────────────────────────────────

    /// `sort_key` puts ranked items in front of unranked ones, regardless of
    /// the priority-string. Among ranked items, lower rank wins. Among
    /// unranked items, priority-string is the tiebreaker.
    #[test]
    fn test_sort_key_orders_rank_before_priority() {
        let critical_unranked = ItemInfo {
            id: "A".into(),
            title: "critical, no rank".into(),
            item_type: "expedition".into(),
            status: "backlog".into(),
            priority: "critical".into(),
            assignee: "-".into(),
            related: vec![],
            depends_on: vec![],
            rank: None,
        };
        let low_rank1 = ItemInfo {
            id: "B".into(),
            title: "low, rank 1".into(),
            item_type: "expedition".into(),
            status: "backlog".into(),
            priority: "low".into(),
            assignee: "-".into(),
            related: vec![],
            depends_on: vec![],
            rank: Some(1),
        };
        let medium_rank3 = ItemInfo {
            id: "C".into(),
            title: "medium, rank 3".into(),
            item_type: "expedition".into(),
            status: "backlog".into(),
            priority: "medium".into(),
            assignee: "-".into(),
            related: vec![],
            depends_on: vec![],
            rank: Some(3),
        };

        let mut keys = [&critical_unranked, &low_rank1, &medium_rank3];
        keys.sort_by_key(|i| sort_key(i));

        assert_eq!(keys[0].id, "B", "rank=1 wins over rank=3 and unranked");
        assert_eq!(keys[1].id, "C", "rank=3 wins over unranked");
        assert_eq!(keys[2].id, "A", "unranked goes last even when critical");
    }

    /// When two items are both unranked, `sort_key` falls back to the
    /// priority-string ordering (critical < high < medium < low).
    #[test]
    fn test_sort_key_falls_back_to_priority_when_unranked() {
        let critical = ItemInfo {
            id: "C".into(),
            title: "critical".into(),
            item_type: "expedition".into(),
            status: "backlog".into(),
            priority: "critical".into(),
            assignee: "-".into(),
            related: vec![],
            depends_on: vec![],
            rank: None,
        };
        let high = ItemInfo {
            id: "H".into(),
            title: "high".into(),
            item_type: "expedition".into(),
            status: "backlog".into(),
            priority: "high".into(),
            assignee: "-".into(),
            related: vec![],
            depends_on: vec![],
            rank: None,
        };

        let mut items = [&high, &critical];
        items.sort_by_key(|i| sort_key(i));
        assert_eq!(items[0].id, "C", "critical < high among unranked");
        assert_eq!(items[1].id, "H");
    }

    /// `compute_critical_path` populates `ready` in rank-ascending order
    /// (ranked items first, then by priority-string fallback).
    #[test]
    fn test_ready_list_sorts_ranked_items_first() {
        let make = |id: &str, priority: &str, rank: Option<i32>| ItemInfo {
            id: id.into(),
            title: id.into(),
            item_type: "expedition".into(),
            status: "backlog".into(),
            priority: priority.into(),
            assignee: "-".into(),
            related: vec![],
            depends_on: vec![],
            rank,
        };
        let items = vec![
            make("EX-A", "critical", None),
            make("EX-B", "low", Some(1)),
            make("EX-C", "medium", Some(2)),
            make("EX-D", "high", None),
        ];
        let cp = compute_critical_path(&items).unwrap();
        // All items have no dependencies → all ready.
        assert_eq!(cp.ready.len(), 4);
        assert_eq!(cp.ready[0], "EX-B", "rank=1 first");
        assert_eq!(cp.ready[1], "EX-C", "rank=2 second");
        // Among unranked: critical < high
        assert_eq!(cp.ready[2], "EX-A");
        assert_eq!(cp.ready[3], "EX-D");
    }

    #[test]
    fn test_longest_path() {
        let items = make_items();
        let cp = compute_critical_path(&items).unwrap();

        // Longest path should go through depth 0 -> 1 -> 2
        assert!(!cp.longest_path.is_empty());
        // First item should be at depth 0, last at max depth
        if let Some(first) = cp.longest_path.first() {
            assert_eq!(cp.depth[first], 0);
        }
        if let Some(last) = cp.longest_path.last() {
            assert_eq!(cp.depth[last], 2);
        }
    }

    /// The campaign rollup over member voyages — the aggregate a flat tag
    /// cannot give. A COMPLETED member voyage MUST still count toward the program %-done (the
    /// regression DGX2 caught: group_by_voyage drops fully-done voyages, which would make %-done
    /// fall as members finish). Only expeditions count (chores excluded); non-members are excluded;
    /// zero/stale members are an empty (0,0,0) view, not an error.
    #[test]
    fn campaign_aggregate_counts_expeditions_including_completed_members() {
        let item = |id: &str, ty: &str, status: &str, related: Vec<&str>| ItemInfo {
            id: id.into(),
            title: format!("{id} title"),
            item_type: ty.into(),
            status: status.into(),
            priority: "-".into(),
            assignee: "-".into(),
            related: related.into_iter().map(String::from).collect(),
            depends_on: vec![],
            rank: None,
        };
        // VY-a is a COMPLETED member (3/3) — it must STILL count. VY-b is an active member (1/2).
        // VY-c (5/5) is NOT a member. A chore child of VY-a must NOT inflate the expedition count.
        let items = vec![
            item("VY-a", "voyage", "complete", vec![]),
            item("EX-a1", "expedition", "done", vec!["VY-a"]),
            item("EX-a2", "expedition", "complete", vec!["VY-a"]),
            item("EX-a3", "expedition", "retired", vec!["VY-a"]),
            item("CH-a1", "chore", "done", vec!["VY-a"]), // chore — excluded from expedition count
            item("VY-b", "voyage", "ready", vec![]),
            item("EX-b1", "expedition", "done", vec!["VY-b"]),
            item("EX-b2", "expedition", "backlog", vec!["VY-b"]),
            item("VY-c", "voyage", "ready", vec![]),
            item("EX-c1", "expedition", "done", vec!["VY-c"]),
        ];
        let members = vec!["VY-a".to_string(), "VY-b".to_string()];

        // 2 members; completed VY-a (3/3) + active VY-b (1/2) = 4 done / 5 total. The chore is
        // excluded; VY-c is not a member.
        assert_eq!(campaign_aggregate(&items, &members), (2, 4, 5));
        // Zero members → (0,0,0): renders empty, never an error.
        assert_eq!(campaign_aggregate(&items, &[]), (0, 0, 0));
        // A member id with no matching voyage item contributes nothing (robust to stale membership).
        assert_eq!(campaign_aggregate(&items, &["VY-z".to_string()]), (0, 0, 0));
    }

    /// the shared campaign renderer (used by BOTH the local CLI and the server) over a
    /// 2-voyage-member fixture — header, member count, program aggregate, member listing; and the
    /// zero-members empty view.
    #[test]
    fn format_campaign_roadmap_renders_members_aggregate_and_empty() {
        let item = |id: &str, ty: &str, status: &str, related: Vec<&str>| ItemInfo {
            id: id.into(),
            title: format!("{id} title"),
            item_type: ty.into(),
            status: status.into(),
            priority: "-".into(),
            assignee: "-".into(),
            related: related.into_iter().map(String::from).collect(),
            depends_on: vec![],
            rank: None,
        };
        // Campaign CA-1 with THREE member voyages: VY-a (1/2, active) + VY-b (1/1, active) + VY-c
        // (2/2, COMPLETED). The completed member must STILL count toward the program %-done — the
        // regression DGX2 caught, since group_by_voyage drops fully-done voyages.
        let items = vec![
            item("CA-1", "campaign", "ready", vec![]),
            item("VY-a", "voyage", "ready", vec![]),
            item("EX-a1", "expedition", "done", vec!["VY-a"]),
            item("EX-a2", "expedition", "backlog", vec!["VY-a"]),
            item("VY-b", "voyage", "ready", vec![]),
            item("EX-b1", "expedition", "done", vec!["VY-b"]),
            item("VY-c", "voyage", "complete", vec![]),
            item("EX-c1", "expedition", "done", vec!["VY-c"]),
            item("EX-c2", "expedition", "done", vec!["VY-c"]),
        ];
        let members = vec!["VY-a".to_string(), "VY-b".to_string(), "VY-c".to_string()];

        let out = format_campaign_roadmap("CA-1", &members, &items);
        assert!(out.contains("Campaign CA-1"), "header: {out}");
        // All THREE members counted, including the completed VY-c.
        assert!(
            out.contains("3 member voyage(s)"),
            "member count incl. completed: {out}"
        );
        // Aggregate includes the completed member: 4/5, NOT 2/3 (the inversion DGX2 caught).
        assert!(
            out.contains("4/5 expeditions done"),
            "aggregate incl. completed member: {out}"
        );
        // The active-work section lists the two members with open work; completed VY-c is dropped
        // there (group_by_voyage skips it) but still counted in the aggregate above.
        assert!(
            out.contains("VY-a") && out.contains("VY-b"),
            "active members listed: {out}"
        );

        // Zero members → the empty-view header, never an error.
        let empty = format_campaign_roadmap("CA-9", &[], &items);
        assert!(
            empty.contains("0 member voyage(s)"),
            "empty header: {empty}"
        );
        assert!(empty.contains("no members"), "empty guidance: {empty}");
    }
}
