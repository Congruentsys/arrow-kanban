// SPDX-License-Identifier: MIT
//! SHACL-driven conformance checking for kanban items.
//!
//! Implements structural validation (sh:minCount checks) against the shapes defined
//! in `ontology/shapes/{dev,research}/`.  No SPARQL engine — we check field
//! presence and allowed-value sets directly against the Arrow RecordBatch.
//!
//! # Usage
//!
//! ```no_run
//! use arrow_kanban::validate::{validate_item, suggest_fixes};
//! # use arrow::array::RecordBatch;
//! # let batch: RecordBatch = unimplemented!();
//! let report = validate_item(&batch);
//! if !report.is_conformant() {
//!     let fixes = suggest_fixes(&report);
//! }
//! ```
//!
//! # Shape rules applied
//!
//! | Type            | Field      | Rule                                    | Severity |
//! |-----------------|------------|-----------------------------------------|----------|
//! | all             | body       | sh:minCount 1 → must be non-null/empty  | Error    |
//! | expedition      | priority   | sh:minCount 1, sh:in (low/medium/…)     | Error    |
//! | expedition      | assignee   | sh:minCount 1                           | Warning  |
//! | voyage          | assignee   | sh:minCount 1                           | Warning  |
//! | chore           | (body)     | body required                           | Error    |
//! | hypothesis      | body       | body required                           | Error    |
//! | experiment      | body       | body required                           | Error    |
//! | paper           | body       | body required                           | Error    |
//! | idea            | body       | body required                           | Error    |
//! | literature      | body       | body required                           | Error    |
//! | measure         | body       | body required                           | Error    |
//! | hazard          | body       | body required                           | Warning  |
//! | signal          | body       | body required                           | Warning  |
//! | feature         | priority   | priority required                        | Error    |

use crate::item_type::ItemType;
use crate::schema::items_col;
use arrow::array::{Array, ListArray, RecordBatch, StringArray};
use std::collections::HashMap;

// ─── Public Types ──────────────────────────────────────────────────────────

/// Severity of a validation violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    /// Hard conformance failure (sh:minCount 1 violated on a required field).
    Error,
    /// Advisory warning — best practice not followed but item is usable.
    Warning,
}

/// A single constraint violation for an item field.
#[derive(Debug, Clone)]
pub struct Violation {
    /// The field name (e.g. "assignee", "body", "priority").
    pub field: String,
    /// Human-readable description (sh:message content).
    pub message: String,
    /// Severity level.
    pub severity: Severity,
}

/// Aggregated report for one item.
#[derive(Debug)]
pub struct ValidationReport {
    /// Item ID (e.g. "EX-3212").
    pub item_id: String,
    /// Item type string (e.g. "expedition").
    pub item_type: String,
    /// All constraint violations found.
    pub violations: Vec<Violation>,
}

impl ValidationReport {
    /// Returns `true` when there are no Error-severity violations.
    pub fn is_conformant(&self) -> bool {
        !self
            .violations
            .iter()
            .any(|v| v.severity == Severity::Error)
    }
}

// ─── Core Validation ───────────────────────────────────────────────────────

/// Validate a single item (one-row RecordBatch) against its type's shape rules.
///
/// Implements structural SHACL (sh:minCount checks only) — no SPARQL engine required.
pub fn validate_item(batch: &RecordBatch) -> ValidationReport {
    let item_id = get_string(batch, items_col::ID).unwrap_or_default();
    let item_type_str = get_string(batch, items_col::ITEM_TYPE).unwrap_or_default();
    let mut violations = Vec::new();

    let item_type = ItemType::from_str_loose(&item_type_str);

    // ── Body (required for most types) ────────────────────────────────────
    let body = get_nullable_string(batch, items_col::BODY);
    let body_empty = body.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true);

    let body_severity = match item_type {
        // Hazard and Signal are ephemeral — body is advisory
        Some(ItemType::Hazard) | Some(ItemType::Signal) => Some(Severity::Warning),
        // All other types: body is required
        Some(_) => Some(Severity::Error),
        // Unknown type: treat as error
        None => Some(Severity::Error),
    };

    if body_empty && let Some(sev) = body_severity {
        violations.push(Violation {
            field: "body".to_string(),
            message: "body is empty — use: nk update <ID> --body-file /tmp/body.md".to_string(),
            severity: sev,
        });
    }

    // ── Priority (required for expedition, voyage, chore, feature) ─────────
    let priority = get_nullable_string(batch, items_col::PRIORITY);
    let priority_required = matches!(
        item_type,
        Some(ItemType::Expedition)
            | Some(ItemType::Voyage)
            | Some(ItemType::Chore)
            | Some(ItemType::Feature)
    );

    if priority_required && priority.is_none() {
        violations.push(Violation {
            field: "priority".to_string(),
            message: "priority required: low|medium|high|critical".to_string(),
            severity: Severity::Error,
        });
    }

    // ── Assignee (advisory for expedition + voyage) ────────────────────────
    let assignee = get_nullable_string(batch, items_col::ASSIGNEE);
    let assignee_required = matches!(
        item_type,
        Some(ItemType::Expedition) | Some(ItemType::Voyage)
    );

    if assignee_required && assignee.is_none() {
        violations.push(Violation {
            field: "assignee".to_string(),
            message: "assignee required: M5|DGX|Mini|unassigned".to_string(),
            severity: Severity::Warning,
        });
    }

    // ── Warn when assignee is "unassigned" for in-progress items ──────────
    let status = get_string(batch, items_col::STATUS).unwrap_or_default();
    if status == "in_progress"
        && let Some(ref a) = assignee
        && a == "unassigned"
    {
        violations.push(Violation {
            field: "assignee".to_string(),
            message: "item is in_progress but assignee is 'unassigned'".to_string(),
            severity: Severity::Warning,
        });
    }

    ValidationReport {
        item_id,
        item_type: item_type_str,
        violations,
    }
}

// ─── Fix Suggestions ───────────────────────────────────────────────────────

/// Generate suggested fix commands for all violations in a report.
///
/// Returns one `nk update ...` command per violation, formatted so the user
/// can copy-paste to fix the issue.
pub fn suggest_fixes(report: &ValidationReport) -> Vec<String> {
    report
        .violations
        .iter()
        .map(|v| match v.field.as_str() {
            "assignee" => format!("nk update {} --assign M5", report.item_id),
            "body" => format!("nk update {} --body-file /tmp/body.md", report.item_id),
            "priority" => format!("nk update {} --priority medium", report.item_id),
            _ => format!("nk update {} # fix {}", report.item_id, v.field),
        })
        .collect()
}

// ─── Board-Wide Validation ─────────────────────────────────────────────────

/// Validate all items in a slice of RecordBatches (one batch = one item row).
///
/// Returns one `ValidationReport` per item.
pub fn validate_all(batches: &[RecordBatch]) -> Vec<ValidationReport> {
    batches.iter().map(validate_item).collect()
}

// ─── Ratification body⇔tag consistency (CH-6502) ────────────────────────────
//
// Kills the "body says PENDING RATIFICATION but the item carries no
// `pending-ratification` tag" pollution class (VY-6451 incident; EX-6419/EX-6427
// recurrences). The watcher's Phase-4b claim-sweep gates on the TAG, so a
// body-pending-but-untagged item is treated as claimable, then a careful
// `/workit` (step 1.4c) bounces on the body line — the item churns and the fleet
// falsely reports "out of work". The two states must be consistent:
// *body-says-pending ⇔ `pending-ratification` tag present* (on the item OR its
// parent voyage — Phase-4b honours voyage-inherited pending state).
//
// This is a *board-level* check by construction: the parent-voyage exemption
// needs the whole item set, and pollution is a property of the SET, not one item.

/// The tag whose presence marks an item (or its voyage) as pending ratification.
const PENDING_TAG: &str = "pending-ratification";

/// Terminal statuses — never flagged (a closed item's stale body line is harmless).
const TERMINAL_STATUSES: &[&str] = &["done", "complete", "abandoned", "retired"];

/// Body phrases that mean "this item is pending Captain ratification". Deliberately ANCHORED to
/// the word "ratif" so a bare "pending"/"stale"/"bounce it" in legitimate prose is NOT a match.
/// All comparisons are lowercase. See [`body_says_pending`] for the leading-marker anchor that
/// separates a genuine STATUS marker from a prose mention of the concept.
const PENDING_BODY_PHRASES: &[&str] = &[
    "pending captain ratification",
    "pending ratification",
    "awaiting captain ratification",
    "awaiting ratification",
];

/// The kind of body⇔tag inconsistency found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RatificationIssue {
    /// Body says pending, but neither the item nor its parent voyage carries the tag.
    /// This is the Phase-4b pollution class — an ERROR (the item churns claim→bounce).
    BodyPendingNoTag,
    /// The item carries the tag, but its body has no ratification marker — a WARNING
    /// (possibly a stale tag never stripped, or a body that lost its marker).
    TagNoBodyMarker,
    /// The item carries the tag, but its parent voyage exists, is non-terminal, and is NOT
    /// pending — a WARNING (the tag was likely not stripped at the voyage's sprint-start).
    TagButParentVoyageRatified,
}

/// One ratification body⇔tag inconsistency.
#[derive(Debug, Clone)]
pub struct RatificationFinding {
    /// Item ID (e.g. "EX-6419").
    pub item_id: String,
    /// Item type string.
    pub item_type: String,
    /// Which inconsistency.
    pub issue: RatificationIssue,
    /// Severity (BodyPendingNoTag = Error; the reverse checks = Warning).
    pub severity: Severity,
    /// Human-readable description + fix guidance.
    pub message: String,
}

/// True if `body` opens with a pending-ratification STATUS MARKER.
///
/// The pollution marker is a **leading, emphasized status line** — the fleet convention is
/// `**PENDING CAPTAIN RATIFICATION** (VY-XXXX).` at the very top of the body. Crucially this must
/// NOT fire on a *prose mention* of the concept, which is where a naive substring match drowns in
/// false positives (verified live against the store): a meta-item that quotes the phrase
/// (`... an item's body says "PENDING CAPTAIN RATIFICATION" ...` — CH-6502), a released voyage that
/// says "pending-ratification **cleared**" (VY-6451), a retro that reports "0 pending-ratification"
/// (SG-4909). So we look ONLY at the FIRST non-empty line, strip leading markdown emphasis
/// (`*`/`#`/`>`/`-`/`` ` ``) and whitespace, and require it to START with a pending phrase.
fn body_says_pending(body: &str) -> bool {
    let Some(first) = body.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return false;
    };
    let head = first
        .trim_start_matches(|c: char| c.is_whitespace() || "*#>-`".contains(c))
        .to_ascii_lowercase();
    PENDING_BODY_PHRASES
        .iter()
        .any(|phrase| head.starts_with(phrase))
}

/// Normalize a `vy-NNNN` voyage-member TAG to its canonical voyage id `VY-NNNN`, or `None` if the
/// tag is not a voyage-member tag. The fleet links a child item to its voyage via this tag (not
/// always a `Related` edge — e.g. EX-6420 carries `vy-6416`), so the parent-voyage exemption must
/// recognize it or it false-flags tag-linked children of a pending voyage (CH-6503).
fn voyage_id_from_tag(tag: &str) -> Option<String> {
    let rest = tag
        .strip_prefix("vy-")
        .or_else(|| tag.strip_prefix("VY-"))?;
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        Some(format!("VY-{rest}"))
    } else {
        None
    }
}

/// Check body⇔tag ratification consistency across a whole board's items (one batch = one item).
///
/// Returns a finding for each inconsistency. `BodyPendingNoTag` is an Error (the pollution class);
/// the two reverse checks are Warnings. Terminal items are skipped. The parent-voyage exemption is
/// evaluated against the same batch set: a child whose body is pending is NOT flagged if it — OR a
/// parent voyage named in its `related` edges — carries the `pending-ratification` tag.
pub fn check_ratification_consistency(batches: &[RecordBatch]) -> Vec<RatificationFinding> {
    // Index every item by ID → (tags, status) so parent-voyage tags resolve in one pass.
    let mut by_id: HashMap<String, (Vec<String>, String)> = HashMap::with_capacity(batches.len());
    for batch in batches {
        if let Some(id) = get_string(batch, items_col::ID) {
            let tags = get_string_list(batch, items_col::TAGS);
            let status = get_string(batch, items_col::STATUS).unwrap_or_default();
            by_id.insert(id, (tags, status));
        }
    }

    let is_terminal = |status: &str| TERMINAL_STATUSES.contains(&status);
    let has_pending_tag = |tags: &[String]| tags.iter().any(|t| t == PENDING_TAG);

    let mut findings = Vec::new();
    for batch in batches {
        let Some(item_id) = get_string(batch, items_col::ID) else {
            continue;
        };
        let status = get_string(batch, items_col::STATUS).unwrap_or_default();
        if is_terminal(&status) {
            continue; // a closed item's stale body line is harmless
        }
        let item_type = get_string(batch, items_col::ITEM_TYPE).unwrap_or_default();
        let tags = get_string_list(batch, items_col::TAGS);
        let related = get_string_list(batch, items_col::RELATED);
        let body = get_nullable_string(batch, items_col::BODY).unwrap_or_default();

        let item_pending_tag = has_pending_tag(&tags);
        let body_pending = body_says_pending(&body);

        // Parent voyages: VY-* IDs in the related edges (partOf/related project to the flat list)
        // AND the `vy-NNNN` voyage-member TAG every child carries (CH-6503: EX-6420 links to
        // VY-6416 via its `vy-6416` tag, NOT a Related edge — a Related-only check false-flags
        // tag-linked children of a pending voyage). Normalize the tag form to the voyage id.
        let mut parent_voyages: Vec<String> = related
            .iter()
            .filter(|r| r.starts_with("VY-"))
            .cloned()
            .collect();
        for t in &tags {
            if let Some(vy) = voyage_id_from_tag(t) {
                parent_voyages.push(vy);
            }
        }
        let a_parent_voyage_is_pending = parent_voyages.iter().any(|vy| {
            by_id
                .get(vy)
                .map(|(vtags, _)| has_pending_tag(vtags))
                .unwrap_or(false)
        });

        // ── The pollution class (ERROR): body pending, but no tag anywhere ────────────────────
        if body_pending && !item_pending_tag && !a_parent_voyage_is_pending {
            findings.push(RatificationFinding {
                item_id: item_id.clone(),
                item_type: item_type.clone(),
                issue: RatificationIssue::BodyPendingNoTag,
                severity: Severity::Error,
                message: format!(
                    "body says pending-ratification but neither {item_id} nor its parent voyage \
                     carries the `pending-ratification` tag — Phase-4b will treat it as claimable \
                     then /workit bounces on the body line (the VY-6451 pollution class). Fix: ADD \
                     the tag if genuinely pending, or DROP the body line if ratified."
                ),
            });
            continue; // one item, one primary finding
        }

        // ── Reverse (WARNING): tagged but body carries no ratification marker ─────────────────
        if item_pending_tag && !body_pending {
            findings.push(RatificationFinding {
                item_id: item_id.clone(),
                item_type: item_type.clone(),
                issue: RatificationIssue::TagNoBodyMarker,
                severity: Severity::Warning,
                message: format!(
                    "{item_id} is tagged `pending-ratification` but its body has no ratification \
                     marker — a possibly-stale tag (never stripped at sprint-start), or a body that \
                     lost its marker. Verify: strip the tag if ratified, or restore the body line."
                ),
            });
            continue;
        }

        // ── Optional hardening (WARNING): tagged, but parent voyage already ratified ──────────
        if item_pending_tag && !a_parent_voyage_is_pending && !parent_voyages.is_empty() {
            // A parent voyage exists in the set, is non-terminal, and is NOT pending.
            let ratified_parent = parent_voyages.iter().find(|vy| {
                by_id
                    .get(vy.as_str())
                    .map(|(vtags, vstatus)| !has_pending_tag(vtags) && !is_terminal(vstatus))
                    .unwrap_or(false)
            });
            if let Some(vy) = ratified_parent {
                findings.push(RatificationFinding {
                    item_id: item_id.clone(),
                    item_type: item_type.clone(),
                    issue: RatificationIssue::TagButParentVoyageRatified,
                    severity: Severity::Warning,
                    message: format!(
                        "{item_id} is still tagged `pending-ratification` but its parent voyage \
                         {vy} is not pending — the tag was likely not stripped at the voyage's \
                         sprint-start (`nk ratify` normally does this). Strip it if ratified."
                    ),
                });
            }
        }
    }

    findings
}

/// Format ratification findings for terminal output. `show_fixes` appends the fix guidance already
/// embedded in each message; the returned string is empty when there are no findings.
pub fn format_ratification_findings(findings: &[RatificationFinding]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    let errors = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    let warns = findings.len() - errors;

    let mut lines = Vec::new();
    lines.push(format!(
        "Ratification body⇔tag consistency (CH-6502): {errors} error(s), {warns} warning(s)"
    ));
    for f in findings {
        let label = match f.severity {
            Severity::Error => "  ERROR  ",
            Severity::Warning => "  WARNING",
        };
        lines.push(format!("{label} {} — {}", f.item_id, f.message));
    }
    lines.join("\n")
}

// ─── Formatting ────────────────────────────────────────────────────────────

/// Format a single `ValidationReport` for terminal output.
pub fn format_report(report: &ValidationReport, show_fixes: bool) -> String {
    let mut lines = Vec::new();

    if report.violations.is_empty() {
        lines.push(format!("{} {} — OK", report.item_id, report.item_type));
    } else {
        let error_count = report
            .violations
            .iter()
            .filter(|v| v.severity == Severity::Error)
            .count();
        let warn_count = report
            .violations
            .iter()
            .filter(|v| v.severity == Severity::Warning)
            .count();

        let summary = match (error_count, warn_count) {
            (0, w) => format!("WARNINGS ({})", w),
            (e, 0) => format!("VIOLATIONS ({})", e),
            (e, w) => format!("VIOLATIONS ({} errors, {} warnings)", e, w),
        };

        lines.push(format!(
            "{} {} — {}",
            report.item_id, report.item_type, summary
        ));

        for v in &report.violations {
            let label = match v.severity {
                Severity::Error => "  ERROR  ",
                Severity::Warning => "  WARNING",
            };
            lines.push(format!("{} {}: {}", label, v.field, v.message));
        }

        if show_fixes {
            let fixes = suggest_fixes(report);
            if !fixes.is_empty() {
                lines.push(String::new());
                lines.push("Suggested fixes:".to_string());
                for fix in &fixes {
                    lines.push(format!("  {fix}"));
                }
            }
        }
    }

    lines.join("\n")
}

/// Format a summary table for board-wide validation.
///
/// Shows conformant count, violation count, and lists violating items.
pub fn format_board_summary(reports: &[ValidationReport]) -> String {
    let conformant: Vec<&ValidationReport> = reports.iter().filter(|r| r.is_conformant()).collect();
    let violating: Vec<&ValidationReport> = reports.iter().filter(|r| !r.is_conformant()).collect();

    let mut lines = Vec::new();
    lines.push(format!(
        "Validation summary: {} conformant, {} with violations",
        conformant.len(),
        violating.len()
    ));

    if violating.is_empty() {
        lines.push("All items conform to their SHACL shapes.".to_string());
    } else {
        lines.push(String::new());
        lines.push("Items with violations:".to_string());
        for r in &violating {
            let error_count = r
                .violations
                .iter()
                .filter(|v| v.severity == Severity::Error)
                .count();
            let warn_count = r
                .violations
                .iter()
                .filter(|v| v.severity == Severity::Warning)
                .count();
            lines.push(format!(
                "  {} ({}) — {} errors, {} warnings",
                r.item_id, r.item_type, error_count, warn_count
            ));
        }
    }

    lines.join("\n")
}

// ─── Arrow Helpers ─────────────────────────────────────────────────────────

/// Extract a non-nullable string value from a column in a single-row batch.
fn get_string(batch: &RecordBatch, col_idx: usize) -> Option<String> {
    batch
        .column(col_idx)
        .as_any()
        .downcast_ref::<StringArray>()
        .and_then(|arr| {
            if arr.is_empty() {
                None
            } else {
                Some(arr.value(0).to_string())
            }
        })
}

/// Extract a list-of-strings column value from a single-row batch (e.g. `tags`, `related`).
/// Returns an empty vec for a null/absent list. Null elements inside the list are skipped.
fn get_string_list(batch: &RecordBatch, col_idx: usize) -> Vec<String> {
    batch
        .column(col_idx)
        .as_any()
        .downcast_ref::<ListArray>()
        .and_then(|arr| {
            if arr.is_empty() || arr.is_null(0) {
                return None;
            }
            arr.value(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .map(|s| {
                    (0..s.len())
                        .filter(|&i| !s.is_null(i))
                        .map(|i| s.value(i).to_string())
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// Extract a nullable string value from a column in a single-row batch.
fn get_nullable_string(batch: &RecordBatch, col_idx: usize) -> Option<String> {
    batch
        .column(col_idx)
        .as_any()
        .downcast_ref::<StringArray>()
        .and_then(|arr| {
            if arr.is_empty() || arr.is_null(0) {
                None
            } else {
                let s = arr.value(0);
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            }
        })
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crud::{CreateItemInput, KanbanStore};

    // ── Helper ──────────────────────────────────────────────────────────────

    /// Create a minimal item in a store and return the single-row RecordBatch.
    fn make_item(
        item_type: ItemType,
        priority: Option<&str>,
        assignee: Option<&str>,
        body: Option<&str>,
        status: &str,
    ) -> RecordBatch {
        let mut store = KanbanStore::new();
        let id = store
            .create_item(&CreateItemInput {
                title: "Test Item".to_string(),
                item_type,
                priority: priority.map(|s| s.to_string()),
                assignee: assignee.map(|s| s.to_string()),
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: body.map(|s| s.to_string()),
            })
            .expect("create_item");

        if status != "backlog" {
            store
                .update_status(&id, status, None, true, None)
                .expect("update_status");
        }

        store.get_item(&id).expect("get_item")
    }

    // ── Body validation ─────────────────────────────────────────────────────

    #[test]
    fn test_expedition_without_body_produces_error() {
        let batch = make_item(
            ItemType::Expedition,
            Some("high"),
            Some("M5"),
            None,
            "backlog",
        );
        let report = validate_item(&batch);
        assert!(!report.is_conformant());
        assert!(report.violations.iter().any(|v| v.field == "body"));
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.field == "body" && v.severity == Severity::Error)
        );
    }

    #[test]
    fn test_expedition_with_body_no_body_violation() {
        let batch = make_item(
            ItemType::Expedition,
            Some("high"),
            Some("M5"),
            Some("## Phase 1\nDo the thing."),
            "backlog",
        );
        let report = validate_item(&batch);
        assert!(
            !report.violations.iter().any(|v| v.field == "body"),
            "should have no body violation when body is set"
        );
    }

    #[test]
    fn test_chore_without_body_produces_error() {
        let batch = make_item(ItemType::Chore, Some("low"), None, None, "backlog");
        let report = validate_item(&batch);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.field == "body" && v.severity == Severity::Error)
        );
    }

    #[test]
    fn test_hazard_without_body_produces_warning_not_error() {
        let batch = make_item(ItemType::Hazard, None, None, None, "backlog");
        let report = validate_item(&batch);
        let body_violations: Vec<_> = report
            .violations
            .iter()
            .filter(|v| v.field == "body")
            .collect();
        assert!(
            !body_violations.is_empty(),
            "hazard without body should have a body violation"
        );
        // All body violations for hazard must be Warning, not Error
        assert!(
            body_violations
                .iter()
                .all(|v| v.severity == Severity::Warning)
        );
    }

    #[test]
    fn test_signal_without_body_produces_warning_not_error() {
        let batch = make_item(ItemType::Signal, None, None, None, "backlog");
        let report = validate_item(&batch);
        let body_violations: Vec<_> = report
            .violations
            .iter()
            .filter(|v| v.field == "body")
            .collect();
        assert!(
            body_violations
                .iter()
                .all(|v| v.severity == Severity::Warning)
        );
    }

    // ── Priority validation ─────────────────────────────────────────────────

    #[test]
    fn test_expedition_without_priority_produces_error() {
        let batch = make_item(
            ItemType::Expedition,
            None,
            Some("M5"),
            Some("body"),
            "backlog",
        );
        let report = validate_item(&batch);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.field == "priority" && v.severity == Severity::Error)
        );
    }

    #[test]
    fn test_voyage_without_priority_produces_error() {
        let batch = make_item(ItemType::Voyage, None, None, Some("body"), "backlog");
        let report = validate_item(&batch);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.field == "priority" && v.severity == Severity::Error)
        );
    }

    #[test]
    fn test_hypothesis_does_not_require_priority() {
        // Hypothesis is a research type — no priority requirement
        let batch = make_item(ItemType::Hypothesis, None, None, Some("body"), "backlog");
        let report = validate_item(&batch);
        assert!(
            !report.violations.iter().any(|v| v.field == "priority"),
            "hypothesis should not require priority"
        );
    }

    // ── Assignee validation ─────────────────────────────────────────────────

    #[test]
    fn test_expedition_without_assignee_produces_warning() {
        let batch = make_item(
            ItemType::Expedition,
            Some("high"),
            None,
            Some("body"),
            "backlog",
        );
        let report = validate_item(&batch);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.field == "assignee" && v.severity == Severity::Warning)
        );
    }

    #[test]
    fn test_chore_without_assignee_no_violation() {
        // Chore doesn't require assignee
        let batch = make_item(ItemType::Chore, Some("low"), None, Some("body"), "backlog");
        let report = validate_item(&batch);
        assert!(
            !report.violations.iter().any(|v| v.field == "assignee"),
            "chore should not require assignee"
        );
    }

    #[test]
    fn test_in_progress_with_unassigned_produces_warning() {
        let batch = make_item(
            ItemType::Expedition,
            Some("high"),
            Some("unassigned"),
            Some("body"),
            "in_progress",
        );
        let report = validate_item(&batch);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.field == "assignee" && v.severity == Severity::Warning),
            "in_progress item with 'unassigned' should produce assignee warning"
        );
    }

    // ── Conformance ─────────────────────────────────────────────────────────

    #[test]
    fn test_fully_conformant_expedition() {
        let batch = make_item(
            ItemType::Expedition,
            Some("high"),
            Some("M5"),
            Some("## Phase 1\nDo the thing."),
            "backlog",
        );
        let report = validate_item(&batch);
        // No errors (warnings are ok for conformance)
        assert!(
            report.is_conformant(),
            "fully-set expedition should be conformant (no errors)"
        );
        assert!(
            !report
                .violations
                .iter()
                .any(|v| v.severity == Severity::Error)
        );
    }

    #[test]
    fn test_is_conformant_false_when_errors_exist() {
        let batch = make_item(ItemType::Expedition, None, None, None, "backlog");
        let report = validate_item(&batch);
        assert!(!report.is_conformant());
    }

    // ── suggest_fixes ───────────────────────────────────────────────────────

    #[test]
    fn test_suggest_fixes_returns_nk_update_commands() {
        let batch = make_item(ItemType::Expedition, None, None, None, "backlog");
        let report = validate_item(&batch);
        let fixes = suggest_fixes(&report);
        assert!(!fixes.is_empty());
        assert!(
            fixes.iter().all(|f| f.starts_with("nk update ")),
            "all fix suggestions should start with 'nk update'"
        );
    }

    #[test]
    fn test_suggest_fixes_assignee_command() {
        let batch = make_item(
            ItemType::Expedition,
            Some("high"),
            None,
            Some("body"),
            "backlog",
        );
        let report = validate_item(&batch);
        let fixes = suggest_fixes(&report);
        assert!(
            fixes.iter().any(|f| f.contains("--assign")),
            "should suggest --assign fix"
        );
    }

    #[test]
    fn test_suggest_fixes_body_command() {
        let batch = make_item(
            ItemType::Expedition,
            Some("high"),
            Some("M5"),
            None,
            "backlog",
        );
        let report = validate_item(&batch);
        let fixes = suggest_fixes(&report);
        assert!(
            fixes.iter().any(|f| f.contains("--body-file")),
            "should suggest --body-file fix"
        );
    }

    #[test]
    fn test_suggest_fixes_priority_command() {
        let batch = make_item(
            ItemType::Expedition,
            None,
            Some("M5"),
            Some("body"),
            "backlog",
        );
        let report = validate_item(&batch);
        let fixes = suggest_fixes(&report);
        assert!(
            fixes.iter().any(|f| f.contains("--priority")),
            "should suggest --priority fix"
        );
    }

    // ── Board-wide validation ───────────────────────────────────────────────

    #[test]
    fn test_validate_all_returns_one_report_per_item() {
        let mut store = KanbanStore::new();
        store
            .create_item(&CreateItemInput {
                title: "A".to_string(),
                item_type: ItemType::Expedition,
                priority: None,
                assignee: None,
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: None,
            })
            .unwrap();
        store
            .create_item(&CreateItemInput {
                title: "B".to_string(),
                item_type: ItemType::Chore,
                priority: Some("low".to_string()),
                assignee: None,
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: Some("body".to_string()),
            })
            .unwrap();

        let batches = store.query_items(None, None, None, None);
        let reports = validate_all(&batches);
        assert_eq!(reports.len(), 2);
    }

    #[test]
    fn test_validate_all_conformant_item_passes() {
        let mut store = KanbanStore::new();
        store
            .create_item(&CreateItemInput {
                title: "Good".to_string(),
                item_type: ItemType::Expedition,
                priority: Some("high".to_string()),
                assignee: Some("M5".to_string()),
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: Some("## Phase 1\nDo the thing.".to_string()),
            })
            .unwrap();

        let batches = store.query_items(None, None, None, None);
        let reports = validate_all(&batches);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].is_conformant());
    }

    // ── format_report ───────────────────────────────────────────────────────

    #[test]
    fn test_format_report_conformant_shows_ok() {
        let batch = make_item(
            ItemType::Expedition,
            Some("high"),
            Some("M5"),
            Some("body"),
            "backlog",
        );
        let report = validate_item(&batch);
        let out = format_report(&report, false);
        assert!(out.contains("OK"), "conformant item should show OK");
    }

    #[test]
    fn test_format_report_violations_shows_error_label() {
        let batch = make_item(ItemType::Expedition, None, None, None, "backlog");
        let report = validate_item(&batch);
        let out = format_report(&report, false);
        assert!(out.contains("ERROR"), "report should show ERROR label");
        assert!(
            out.contains("VIOLATIONS"),
            "report header should say VIOLATIONS"
        );
    }

    #[test]
    fn test_format_report_with_fixes_shows_suggestions() {
        let batch = make_item(ItemType::Expedition, None, None, None, "backlog");
        let report = validate_item(&batch);
        let out = format_report(&report, true);
        assert!(out.contains("Suggested fixes:"));
        assert!(out.contains("nk update "));
    }

    // ── format_board_summary ────────────────────────────────────────────────

    #[test]
    fn test_format_board_summary_all_ok() {
        let mut store = KanbanStore::new();
        store
            .create_item(&CreateItemInput {
                title: "Good".to_string(),
                item_type: ItemType::Expedition,
                priority: Some("high".to_string()),
                assignee: Some("M5".to_string()),
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: Some("body".to_string()),
            })
            .unwrap();

        let batches = store.query_items(None, None, None, None);
        let reports = validate_all(&batches);
        let summary = format_board_summary(&reports);
        assert!(summary.contains("1 conformant"));
        assert!(summary.contains("0 with violations"));
        assert!(summary.contains("All items conform"));
    }

    // ── Ratification body⇔tag consistency (CH-6502) ─────────────────────────

    /// Create an item with tags + related + body and return its ID.
    fn make_full_item(
        store: &mut KanbanStore,
        item_type: ItemType,
        tags: &[&str],
        related: &[&str],
        body: Option<&str>,
    ) -> String {
        store
            .create_item(&CreateItemInput {
                title: "T".to_string(),
                item_type,
                priority: Some("high".to_string()),
                assignee: None,
                tags: tags.iter().map(|s| s.to_string()).collect(),
                related: related.iter().map(|s| s.to_string()).collect(),
                depends_on: vec![],
                body: body.map(|s| s.to_string()),
            })
            .expect("create_item")
    }

    fn find_finding<'a>(
        findings: &'a [RatificationFinding],
        id: &str,
    ) -> Option<&'a RatificationFinding> {
        findings.iter().find(|f| f.item_id == id)
    }

    #[test]
    fn test_body_says_pending_matches_only_a_leading_status_marker() {
        // TRUE — a leading, emphasized status marker (the fleet convention).
        assert!(body_says_pending(
            "**PENDING CAPTAIN RATIFICATION** (VY-6426). PR class: in-plane."
        ));
        assert!(body_says_pending("PENDING RATIFICATION\n\nPhase 1: ..."));
        assert!(body_says_pending(
            "> **Awaiting Captain ratification** of the scope."
        ));
        assert!(body_says_pending("\n\n  **pending ratification** — rev2"));
    }

    #[test]
    fn test_body_says_pending_rejects_prose_mentions_the_live_false_positives() {
        // CH-6502 shape: a meta-item that QUOTES the phrase mid-prose.
        assert!(!body_says_pending(
            "THE RECURRING BUG: an item's BODY says \"PENDING CAPTAIN RATIFICATION\" but carries no tag."
        ));
        // VY-6451 shape: a RELEASED voyage that says the state was cleared.
        assert!(!body_says_pending(
            "**✅ RELEASED 2026-07-24 — FLEET-READY.** ... pending-ratification cleared."
        ));
        // SG-4909 shape: a retro reporting the count.
        assert!(!body_says_pending(
            "Converging, not thrashing: Q=0, 0 pending-ratification, no stop-train."
        ));
        // The bare-word false positives the hand-audit hit.
        assert!(!body_says_pending("their sealed floor is stale"));
        assert!(!body_says_pending("this is pending review by a peer"));
        assert!(!body_says_pending("bounce it back to the author"));
    }

    #[test]
    fn test_body_pending_no_tag_is_an_error() {
        let mut store = KanbanStore::new();
        let id = make_full_item(
            &mut store,
            ItemType::Expedition,
            &["v20"],
            &[],
            Some("**PENDING CAPTAIN RATIFICATION** (VY-1). Phase 1..."),
        );
        let batches = store.query_items(None, None, None, None);
        let findings = check_ratification_consistency(&batches);
        let f = find_finding(&findings, &id).expect("should flag the pollution");
        assert_eq!(f.issue, RatificationIssue::BodyPendingNoTag);
        assert_eq!(f.severity, Severity::Error);
    }

    #[test]
    fn test_body_pending_with_tag_clears() {
        let mut store = KanbanStore::new();
        let id = make_full_item(
            &mut store,
            ItemType::Expedition,
            &["v20", "pending-ratification"],
            &[],
            Some("**PENDING RATIFICATION**. Phase 1..."),
        );
        let batches = store.query_items(None, None, None, None);
        let findings = check_ratification_consistency(&batches);
        assert!(
            find_finding(&findings, &id).is_none(),
            "an item that carries the tag is consistent — no finding"
        );
    }

    #[test]
    fn test_body_pending_child_exempt_when_parent_voyage_pending() {
        let mut store = KanbanStore::new();
        let voyage = make_full_item(
            &mut store,
            ItemType::Voyage,
            &["v20", "pending-ratification"],
            &[],
            Some("Voyage body pending ratification"),
        );
        // Child: body pending, NO own tag, but related to the pending voyage → EXEMPT.
        let child = make_full_item(
            &mut store,
            ItemType::Expedition,
            &["v20"],
            &[voyage.as_str()],
            Some("**PENDING RATIFICATION** — inherits from the voyage"),
        );
        let batches = store.query_items(None, None, None, None);
        let findings = check_ratification_consistency(&batches);
        assert!(
            find_finding(&findings, &child).is_none(),
            "a child inherits pending from its parent voyage's tag — not pollution"
        );
    }

    #[test]
    fn test_voyage_id_from_tag_normalizes_only_voyage_member_tags() {
        assert_eq!(voyage_id_from_tag("vy-6416"), Some("VY-6416".to_string()));
        assert_eq!(voyage_id_from_tag("VY-6416"), Some("VY-6416".to_string()));
        assert_eq!(voyage_id_from_tag("v20"), None);
        assert_eq!(voyage_id_from_tag("vy-"), None);
        assert_eq!(voyage_id_from_tag("vy-abc"), None);
        assert_eq!(voyage_id_from_tag("safety"), None);
    }

    #[test]
    fn test_body_pending_child_exempt_via_vy_tag_not_just_related() {
        // CH-6503 guard fix: a child links to its voyage via the `vy-NNNN` TAG (the fleet's
        // convention), NOT a Related edge — the Related-only check false-flagged it (EX-6420).
        let mut store = KanbanStore::new();
        let voyage = make_full_item(
            &mut store,
            ItemType::Voyage,
            &["v20", "pending-ratification"],
            &[],
            Some("Voyage pending ratification"),
        );
        let vy_tag = voyage.to_lowercase(); // e.g. "VY-3001" -> "vy-3001"
        let child = make_full_item(
            &mut store,
            ItemType::Expedition,
            &["v20", vy_tag.as_str()], // linked by the vy-tag, NO own pending tag, NO Related edge
            &[],
            Some("**PENDING CAPTAIN RATIFICATION** — inherits via the vy-tag"),
        );
        let batches = store.query_items(None, None, None, None);
        let findings = check_ratification_consistency(&batches);
        assert!(
            find_finding(&findings, &child).is_none(),
            "a child linked to a pending voyage by its `vy-NNNN` TAG (not Related) inherits pending \
             — not pollution (the CH-6503 false-positive class this fix closes)"
        );
    }

    #[test]
    fn test_tag_without_body_marker_is_a_warning() {
        let mut store = KanbanStore::new();
        let id = make_full_item(
            &mut store,
            ItemType::Chore,
            &["v20", "pending-ratification"],
            &[],
            Some("A perfectly ratified body with no marker."),
        );
        let batches = store.query_items(None, None, None, None);
        let findings = check_ratification_consistency(&batches);
        let f = find_finding(&findings, &id).expect("should warn on tag-without-marker");
        assert_eq!(f.issue, RatificationIssue::TagNoBodyMarker);
        assert_eq!(f.severity, Severity::Warning);
    }

    #[test]
    fn test_terminal_item_is_never_flagged() {
        let mut store = KanbanStore::new();
        let id = make_full_item(
            &mut store,
            ItemType::Expedition,
            &["v20"],
            &[],
            Some("**PENDING CAPTAIN RATIFICATION** (long since shipped)."),
        );
        store
            .update_status(&id, "done", Some("completed"), true, None)
            .expect("close");
        let batches = store.query_items(None, None, None, None);
        let findings = check_ratification_consistency(&batches);
        assert!(
            find_finding(&findings, &id).is_none(),
            "a terminal item's stale body line is harmless — never flagged"
        );
    }

    #[test]
    fn test_tag_but_parent_voyage_ratified_is_a_warning() {
        let mut store = KanbanStore::new();
        // Voyage exists, non-terminal, NOT pending (ratified).
        let voyage = make_full_item(
            &mut store,
            ItemType::Voyage,
            &["v20"],
            &[],
            Some("Ratified voyage body."),
        );
        // Child still tagged pending, body also pending, parent ratified → the stale-tag warning.
        let child = make_full_item(
            &mut store,
            ItemType::Expedition,
            &["v20", "pending-ratification"],
            &[voyage.as_str()],
            Some("**PENDING RATIFICATION**. child body."),
        );
        let batches = store.query_items(None, None, None, None);
        let findings = check_ratification_consistency(&batches);
        let f = find_finding(&findings, &child).expect("should warn: tag but parent ratified");
        assert_eq!(f.issue, RatificationIssue::TagButParentVoyageRatified);
        assert_eq!(f.severity, Severity::Warning);
    }

    #[test]
    fn test_clean_board_has_no_findings() {
        let mut store = KanbanStore::new();
        make_full_item(
            &mut store,
            ItemType::Expedition,
            &["v20"],
            &[],
            Some("A clean body with no ratification marker and no tag."),
        );
        let batches = store.query_items(None, None, None, None);
        let findings = check_ratification_consistency(&batches);
        assert!(findings.is_empty(), "a consistent board yields no findings");
        assert!(format_ratification_findings(&findings).is_empty());
    }

    #[test]
    fn test_format_ratification_findings_shows_counts_and_labels() {
        let mut store = KanbanStore::new();
        make_full_item(
            &mut store,
            ItemType::Expedition,
            &["v20"],
            &[],
            Some("PENDING CAPTAIN RATIFICATION"),
        );
        let batches = store.query_items(None, None, None, None);
        let findings = check_ratification_consistency(&batches);
        let out = format_ratification_findings(&findings);
        assert!(out.contains("CH-6502"));
        assert!(out.contains("ERROR"));
        assert!(out.contains("1 error"));
    }

    #[test]
    fn test_format_board_summary_with_violations() {
        let mut store = KanbanStore::new();
        // One bad item (no body, no priority)
        store
            .create_item(&CreateItemInput {
                title: "Bad".to_string(),
                item_type: ItemType::Expedition,
                priority: None,
                assignee: None,
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: None,
            })
            .unwrap();
        // One good item
        store
            .create_item(&CreateItemInput {
                title: "Good".to_string(),
                item_type: ItemType::Chore,
                priority: Some("low".to_string()),
                assignee: None,
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: Some("body".to_string()),
            })
            .unwrap();

        let batches = store.query_items(None, None, None, None);
        let reports = validate_all(&batches);
        let summary = format_board_summary(&reports);
        assert!(summary.contains("1 conformant"));
        assert!(summary.contains("1 with violations"));
    }
}
