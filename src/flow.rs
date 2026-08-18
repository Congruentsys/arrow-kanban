// SPDX-License-Identifier: MIT
//! Flow & WIP charting — WIP vs limits, aging, throughput, cumulative flow.
//!
//! Everything derives from the `runs` table (the complete status-change audit
//! trail) joined to `items`; no new capture mechanism and no new persistence.
//! All computations are pure library functions over the store's batches so the
//! local CLI and the server render identically (the `handle_roadmap` pattern).
//!
//! One deliberate modelling choice, ruled rather than stylistic: **chore and
//! expedition are ONE combined series.** Item-type usage has drifted
//! (feature-sized work filed as chores), so segmenting flow by type would
//! report noise as signal. Voyages/campaigns are WIP-exempt via config.

use crate::config::BoardConfig;
use crate::crud::KanbanStore;
use arrow::array::{Array, BooleanArray, StringArray, TimestampMillisecondArray};
use std::collections::{BTreeMap, BTreeSet};

/// One status transition, from the runs audit trail.
#[derive(Debug, Clone)]
pub struct Transition {
    pub item_id: String,
    pub from: Option<String>,
    pub to: String,
    pub ts_ms: i64,
    pub forced: bool,
}

/// The item fields flow charts join against.
#[derive(Debug, Clone)]
pub struct FlowItem {
    pub id: String,
    pub item_type: String,
    pub board: String,
    pub status: String,
    pub assignee: String,
}

/// Extract every transition, sorted by timestamp (stable within an item).
pub fn transitions(store: &KanbanStore) -> Vec<Transition> {
    let mut out = Vec::new();
    for batch in store.runs_batches() {
        let ids = col_str(batch, "item_id");
        let froms = col_str(batch, "from_status");
        let tos = col_str(batch, "to_status");
        let forced = batch
            .column_by_name("forced")
            .and_then(|c| c.as_any().downcast_ref::<BooleanArray>());
        let ts = batch
            .column_by_name("timestamp")
            .and_then(|c| c.as_any().downcast_ref::<TimestampMillisecondArray>());
        let (Some(ids), Some(tos), Some(ts)) = (ids, tos, ts) else {
            continue;
        };
        for i in 0..batch.num_rows() {
            out.push(Transition {
                item_id: ids.value(i).to_string(),
                from: froms
                    .filter(|f| !f.is_null(i))
                    .map(|f| f.value(i).to_string()),
                to: tos.value(i).to_string(),
                ts_ms: if ts.is_null(i) { 0 } else { ts.value(i) },
                forced: forced.is_some_and(|f| !f.is_null(i) && f.value(i)),
            });
        }
    }
    out.sort_by_key(|t| t.ts_ms);
    out
}

/// Non-deleted items with the fields flow charts need.
pub fn flow_items(store: &KanbanStore) -> Vec<FlowItem> {
    let mut out = Vec::new();
    for batch in store.items_batches() {
        let ids = col_str(batch, "id");
        let types = col_str(batch, "item_type");
        let boards = col_str(batch, "board");
        let statuses = col_str(batch, "status");
        let assignees = col_str(batch, "assignee");
        let deleted = batch
            .column_by_name("deleted")
            .and_then(|c| c.as_any().downcast_ref::<BooleanArray>());
        let (Some(ids), Some(statuses)) = (ids, statuses) else {
            continue;
        };
        for i in 0..batch.num_rows() {
            if deleted.is_some_and(|d| !d.is_null(i) && d.value(i)) {
                continue;
            }
            out.push(FlowItem {
                id: ids.value(i).to_string(),
                item_type: types.map(|t| t.value(i).to_string()).unwrap_or_default(),
                board: boards.map(|b| b.value(i).to_string()).unwrap_or_default(),
                status: statuses.value(i).to_string(),
                assignee: assignees
                    .map(|a| a.value(i).to_string())
                    .unwrap_or_default(),
            });
        }
    }
    out
}

fn col_str<'a>(batch: &'a arrow::record_batch::RecordBatch, name: &str) -> Option<&'a StringArray> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
}

// ── Phase 1: the time-bucketed status series ─────────────────────────────────

/// Status counts per time bucket: for each bucket boundary, how many (combined
/// chore+expedition) items sat in each status. Derived by folding each item's
/// ordered transitions; an item exists from its first transition (creation has
/// `from = None`).
pub fn status_series(
    trans: &[Transition],
    items: &[FlowItem],
    bucket_ms: i64,
    board: &str,
) -> Vec<(i64, BTreeMap<String, usize>)> {
    let in_scope: BTreeSet<&str> = items
        .iter()
        .filter(|i| i.board == board && combined_series_type(&i.item_type))
        .map(|i| i.id.as_str())
        .collect();
    let mut per_item: BTreeMap<&str, Vec<&Transition>> = BTreeMap::new();
    for t in trans {
        if in_scope.contains(t.item_id.as_str()) {
            per_item.entry(t.item_id.as_str()).or_default().push(t);
        }
    }
    if per_item.is_empty() {
        return Vec::new();
    }
    let start = trans
        .iter()
        .map(|t| t.ts_ms)
        .filter(|t| *t > 0)
        .min()
        .unwrap_or(0);
    let end = trans.iter().map(|t| t.ts_ms).max().unwrap_or(start);
    let bucket_ms = bucket_ms.max(1);
    let mut out = Vec::new();
    let mut at = start;
    while at <= end {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for transitions in per_item.values() {
            // The status at time `at` is the `to` of the last transition <= at.
            if let Some(last) = transitions.iter().rev().find(|t| t.ts_ms <= at) {
                *counts.entry(last.to.clone()).or_default() += 1;
            }
        }
        out.push((at, counts));
        at += bucket_ms;
    }
    out
}

/// The ruled combined series: chore + expedition as ONE (type usage has
/// drifted; segmenting by a drifted dimension reports noise as signal).
pub fn combined_series_type(item_type: &str) -> bool {
    matches!(item_type.to_lowercase().as_str(), "chore" | "expedition")
}

// ── Phase 2: WIP vs limits ───────────────────────────────────────────────────

#[derive(Debug)]
pub struct WipRow {
    pub status: String,
    pub count: usize,
    pub limit: Option<u32>,
    pub breach: bool,
}

/// Current WIP per status against configured limits, WIP-exempt types
/// excluded, plus the per-agent breakdown.
pub fn wip(
    items: &[FlowItem],
    config: &BoardConfig,
) -> (Vec<WipRow>, BTreeMap<String, BTreeMap<String, usize>>) {
    let mut per_status: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_agent: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for item in items {
        if item.board != config.name || config.is_wip_exempt(&item.item_type) {
            continue;
        }
        *per_status.entry(item.status.clone()).or_default() += 1;
        let agent = if item.assignee.is_empty() {
            "-"
        } else {
            &item.assignee
        };
        *per_agent
            .entry(agent.to_string())
            .or_default()
            .entry(item.status.clone())
            .or_default() += 1;
    }
    let rows = per_status
        .into_iter()
        .map(|(status, count)| {
            let limit = config.wip_limit(&status);
            let breach = limit.is_some_and(|l| count > l as usize);
            WipRow {
                status,
                count,
                limit,
                breach,
            }
        })
        .collect();
    (rows, per_agent)
}

// ── Phase 3: WIP aging ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct AgingRow {
    pub id: String,
    pub status: String,
    /// ms since the last transition INTO the current status (runs.timestamp —
    /// never `updated_at`, which any edit bumps).
    pub in_status_ms: i64,
}

/// Age of every non-terminal item in its CURRENT status. Items whose current
/// status never appears in runs (pre-audit-trail rows) are skipped rather than
/// guessed — an unmeasurable age is not a young one.
pub fn aging(
    items: &[FlowItem],
    trans: &[Transition],
    terminal: &BTreeSet<String>,
    now_ms: i64,
) -> Vec<AgingRow> {
    let mut out = Vec::new();
    for item in items {
        if terminal.contains(&item.status) {
            continue;
        }
        let entered = trans
            .iter()
            .rev()
            .find(|t| t.item_id == item.id && t.to == item.status)
            .map(|t| t.ts_ms);
        if let Some(ts) = entered {
            out.push(AgingRow {
                id: item.id.clone(),
                status: item.status.clone(),
                in_status_ms: (now_ms - ts).max(0),
            });
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.in_status_ms));
    out
}

// ── Phase 4: throughput ──────────────────────────────────────────────────────

/// Items reaching a terminal status per bucket (an item counts once per bucket
/// even if it bounced through terminal twice), chore+expedition combined,
/// optional per-agent split via the transition's `by_agent`… the join uses the
/// ITEM's assignee at read time — the transition rows carry `by_agent` but the
/// mover is not necessarily the owner, and the owner is the throughput story.
pub fn throughput(
    trans: &[Transition],
    items: &[FlowItem],
    terminal: &BTreeSet<String>,
    bucket_ms: i64,
) -> Vec<(i64, usize)> {
    let scope: BTreeMap<&str, &FlowItem> = items
        .iter()
        .filter(|i| combined_series_type(&i.item_type))
        .map(|i| (i.id.as_str(), i))
        .collect();
    let bucket_ms = bucket_ms.max(1);
    let mut per_bucket: BTreeMap<i64, BTreeSet<&str>> = BTreeMap::new();
    for t in trans {
        if terminal.contains(&t.to) && scope.contains_key(t.item_id.as_str()) && t.ts_ms > 0 {
            per_bucket
                .entry((t.ts_ms / bucket_ms) * bucket_ms)
                .or_default()
                .insert(t.item_id.as_str());
        }
    }
    per_bucket.into_iter().map(|(b, s)| (b, s.len())).collect()
}

// ── Renderers (text; shared by both serving paths) ───────────────────────────

pub fn render_wip(
    rows: &[WipRow],
    per_agent: &BTreeMap<String, BTreeMap<String, usize>>,
) -> String {
    let mut s = String::from(
        "WIP vs limits (voyage/campaign exempt; chore+expedition combined per the type-drift ruling)\n",
    );
    for r in rows {
        let lim = r.limit.map(|l| l.to_string()).unwrap_or_else(|| "-".into());
        let flag = if r.breach { "  ⚠ BREACH" } else { "" };
        s.push_str(&format!(
            "  {:<14} {:>4} / limit {}{}\n",
            r.status, r.count, lim, flag
        ));
    }
    s.push_str("per agent:\n");
    for (agent, statuses) in per_agent {
        let detail: Vec<String> = statuses.iter().map(|(st, n)| format!("{st}={n}")).collect();
        s.push_str(&format!("  {:<10} {}\n", agent, detail.join(" ")));
    }
    s
}

pub fn render_aging(rows: &[AgingRow]) -> String {
    let mut s = String::from("WIP aging (time in CURRENT status, from the runs audit trail)\n");
    for r in rows {
        let h = r.in_status_ms / 3_600_000;
        let (n, unit) = if h >= 48 { (h / 24, "d") } else { (h, "h") };
        s.push_str(&format!(
            "  {:<10} {:<14} {:>4}{}\n",
            r.id, r.status, n, unit
        ));
    }
    if rows.is_empty() {
        s.push_str("  (no measurable non-terminal items)\n");
    }
    s
}

pub fn render_throughput(buckets: &[(i64, usize)], bucket_ms: i64) -> String {
    let unit = if bucket_ms >= 604_800_000 {
        "week"
    } else {
        "day"
    };
    let mut s = format!("throughput (terminal arrivals per {unit}, chore+expedition combined)\n");
    for (b, n) in buckets {
        s.push_str(&format!(
            "  {}  {:>4}  {}\n",
            fmt_day(*b),
            n,
            "#".repeat((*n).min(60))
        ));
    }
    if buckets.is_empty() {
        s.push_str("  (no terminal transitions recorded)\n");
    }
    s
}

pub fn render_cfd(series: &[(i64, BTreeMap<String, usize>)]) -> String {
    let mut statuses: BTreeSet<&str> = BTreeSet::new();
    for (_, counts) in series {
        statuses.extend(counts.keys().map(|s| s.as_str()));
    }
    let mut s = String::from("cumulative flow (items per status per bucket)\n  bucket");
    for st in &statuses {
        s.push_str(&format!("  {st:>12}"));
    }
    s.push('\n');
    for (b, counts) in series {
        s.push_str(&format!("  {}", fmt_day(*b)));
        for st in &statuses {
            s.push_str(&format!("  {:>12}", counts.get(*st).copied().unwrap_or(0)));
        }
        s.push('\n');
    }
    if series.is_empty() {
        s.push_str("  (no transitions in scope)\n");
    }
    s
}

fn fmt_day(ts_ms: i64) -> String {
    // ms epoch -> YYYY-MM-DD without pulling a date crate: chrono is already a dep.
    use chrono::TimeZone;
    chrono::Utc
        .timestamp_millis_opt(ts_ms)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| ts_ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, from: Option<&str>, to: &str, ts: i64, forced: bool) -> Transition {
        Transition {
            item_id: id.into(),
            from: from.map(Into::into),
            to: to.into(),
            ts_ms: ts,
            forced,
        }
    }
    fn item(id: &str, ty: &str, status: &str, assignee: &str) -> FlowItem {
        FlowItem {
            id: id.into(),
            item_type: ty.into(),
            board: "development".into(),
            status: status.into(),
            assignee: assignee.into(),
        }
    }

    /// The Phase-1 series over a synthetic runs fixture with a NULL-from
    /// creation and a FORCED move — the verification's named shapes.
    #[test]
    fn series_folds_known_transitions() {
        let trans = vec![
            t("CH-1", None, "backlog", 1_000, false),
            t("EX-2", None, "backlog", 1_000, false),
            t("CH-1", Some("backlog"), "in_progress", 2_000, false),
            t("EX-2", Some("backlog"), "in_progress", 3_000, true), // forced
            t("CH-1", Some("in_progress"), "done", 4_000, false),
        ];
        let items = vec![
            item("CH-1", "chore", "done", "A"),
            item("EX-2", "expedition", "in_progress", "B"),
        ];
        let series = status_series(&trans, &items, 1_000, "development");
        // At t=1000 both backlog; at t=3000 both in_progress; at t=4000 one done one in_progress.
        assert_eq!(series[0].1.get("backlog"), Some(&2), "{series:?}");
        assert_eq!(series[2].1.get("in_progress"), Some(&2), "{series:?}");
        assert_eq!(series[3].1.get("done"), Some(&1), "{series:?}");
        assert_eq!(series[3].1.get("in_progress"), Some(&1), "{series:?}");
    }

    /// The COMBINED grouping is asserted, not just implemented: a voyage's
    /// transitions never enter the series even on the same board.
    #[test]
    fn chore_and_expedition_are_one_series_and_voyages_are_not() {
        assert!(combined_series_type("chore"));
        assert!(combined_series_type("expedition"));
        assert!(!combined_series_type("voyage"));
        assert!(!combined_series_type("campaign"));
        // hazard/signal were dead terms in the old three-clause boolean (CH-8304) —
        // pin that they were never, and are still not, part of the combined series.
        assert!(!combined_series_type("hazard"));
        assert!(!combined_series_type("signal"));
        let trans = vec![
            t("CH-1", None, "backlog", 1_000, false),
            t("VY-9", None, "backlog", 1_000, false),
        ];
        let items = vec![
            item("CH-1", "chore", "backlog", "-"),
            item("VY-9", "voyage", "backlog", "-"),
        ];
        let series = status_series(&trans, &items, 1_000, "development");
        assert_eq!(
            series[0].1.get("backlog"),
            Some(&1),
            "voyage leaked into the combined series"
        );
    }

    /// WIP honours limits and exemption; the breach flag fires above the limit.
    #[test]
    fn wip_flags_breach_and_exempts_voyages() {
        let yaml = crate::config::default_config_yaml();
        let full: crate::config::ConfigFile = serde_yaml::from_str(yaml).expect("default config");
        let mut cfg = full.default_board().expect("dev board").clone();
        cfg.wip_limits.insert("in_progress".into(), 1);
        let items = vec![
            item("CH-1", "chore", "in_progress", "A"),
            item("CH-2", "chore", "in_progress", "B"),
            item("VY-9", "voyage", "in_progress", "-"),
        ];
        let (rows, per_agent) = wip(&items, &cfg);
        let ip = rows
            .iter()
            .find(|r| r.status == "in_progress")
            .expect("row");
        assert_eq!(ip.count, 2, "voyage must be exempt: {rows:?}");
        assert!(ip.breach, "2 > limit 1 must flag: {rows:?}");
        assert_eq!(
            per_agent.get("A").and_then(|m| m.get("in_progress")),
            Some(&1)
        );
    }

    /// Aging reads the LAST transition INTO the current status — a bounce back
    /// into in_progress restarts the clock, and an unmeasurable item is
    /// SKIPPED, never guessed young.
    #[test]
    fn aging_uses_entry_into_current_status_and_skips_unmeasurable() {
        let trans = vec![
            t("CH-1", None, "backlog", 1_000, false),
            t("CH-1", Some("backlog"), "in_progress", 2_000, false),
            t("CH-1", Some("in_progress"), "backlog", 3_000, false),
            t("CH-1", Some("backlog"), "in_progress", 5_000, false), // re-entry
        ];
        let items = vec![
            item("CH-1", "chore", "in_progress", "A"),
            item("CH-2", "chore", "in_progress", "B"), // no runs rows at all
        ];
        let terminal: BTreeSet<String> = ["done".to_string()].into();
        let rows = aging(&items, &trans, &terminal, 10_000);
        assert_eq!(rows.len(), 1, "the unmeasurable item is skipped: {rows:?}");
        assert_eq!(
            rows[0].in_status_ms, 5_000,
            "clock restarts at RE-entry: {rows:?}"
        );
    }

    /// Throughput counts terminal arrivals per bucket, combined series only.
    #[test]
    fn throughput_counts_terminal_arrivals() {
        let trans = vec![
            t("CH-1", Some("review"), "done", 1_000, false),
            t("EX-2", Some("review"), "done", 1_500, false),
            t("VY-9", Some("review"), "done", 1_600, false), // out of the combined series
            t("CH-3", Some("review"), "done", 90_000_000, false),
        ];
        let items = vec![
            item("CH-1", "chore", "done", "-"),
            item("EX-2", "expedition", "done", "-"),
            item("VY-9", "voyage", "done", "-"),
            item("CH-3", "chore", "done", "-"),
        ];
        let terminal: BTreeSet<String> = ["done".to_string()].into();
        let buckets = throughput(&trans, &items, &terminal, 86_400_000);
        assert_eq!(buckets.len(), 2, "{buckets:?}");
        assert_eq!(
            buckets[0].1, 2,
            "voyage excluded from the combined series: {buckets:?}"
        );
        assert_eq!(buckets[1].1, 1);
    }
}
