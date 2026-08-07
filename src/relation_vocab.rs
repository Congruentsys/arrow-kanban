// SPDX-License-Identifier: MIT
//! The typed relationship vocabulary.
//!
//! # Why this exists
//!
//! `nk` had a typed relation store (`RelationsStore`, `relations_schema`) whose doc
//! comment already named `implements` / `spawns` / `blocks` — the graph was designed. But
//! nothing reached it: `relations.parquet` never existed on disk, no CLI command called
//! `add_relation`, and all real relationship data lived in two flat, untyped list columns
//! (`items.related`, `items.depends_on`) that `arrow-kanban create` could not even set.
//!
//! So an H→M→EXPR research trio was wired with `--related`, which discards **direction**
//! and **kind**: you could not ask *"which experiment validates this hypothesis?"* without
//! reading prose. Five of the seven relationships an untyped `--related` edge cannot model
//! had no representation at all — and they were exactly the research-side ones.
//!
//! This module is the vocabulary: a superset of `.arrow-kanban/ontology/kanban.ttl` plus
//! the predicates its SHACL shapes reference.
//!
//! # One source of truth
//!
//! `related` and `dependsOn` are ALSO stored as typed edges. The flat `items.related` /
//! `items.depends_on` columns are a **projection** of those edges, written in the same
//! operation — never independently. Everything downstream (`roadmap`, `critical-path`,
//! `worklist`) keeps reading the flat columns unchanged; they simply stop being the place
//! relationships are *authored*. Two independently-written stores would drift, which is the
//! failure this fleet keeps paying for.

use crate::item_type::ItemType;

/// A relationship predicate: the kind of a typed edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Predicate {
    /// Canonical name as stored in `relations.predicate` and accepted on the CLI.
    pub name: &'static str,
    /// Item types allowed as the SOURCE, or empty for "any item".
    pub domain: &'static [&'static str],
    /// Item types allowed as the TARGET, or empty for "any item".
    pub range: &'static [&'static str],
    /// The inverse predicate's name, if this edge has a named inverse.
    ///
    /// Only ONE direction is ever stored. Storing both invites them to disagree; the
    /// inverse is derived at query time.
    pub inverse: Option<&'static str>,
    /// One-line meaning, surfaced in `--help` and in validation errors.
    pub doc: &'static str,
}

/// Type prefixes, matching the ID scheme (`EX-`, `VY-`, `H-`, `EXPR-`, `M-`, `PAPER-`).
/// One graph-admitted ENTITY CLASS: a kind the vocabulary may
/// constrain on (`rdfs:domain`/`rdfs:range`) that is NOT necessarily a board
/// [`ItemType`]. Loaded from `owl:Class` declarations in the embedded ontology,
/// exactly the way predicates are — class-addition is DATA-DRIVEN.
///
/// `id_prefix` (the optional `kb:idPrefix "PROP"` clause) makes ids of the kind
/// resolvable by [`type_from_id`], which is what lets an edge constrain on the
/// class at validation time. A class with no prefix is still a legal
/// domain/range citizen; its ids just cannot be prefix-typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityClass {
    /// Lowercased local name (the `validate_edge` comparison form).
    pub name: &'static str,
    /// UPPERCASE id prefix (`"PROP"`), when declared.
    pub id_prefix: Option<&'static str>,
}

const DEV_ANY: &[&str] = &[];
const EXPEDITION: &[&str] = &["expedition"];
const VOYAGE: &[&str] = &["voyage"];
const HYPOTHESIS: &[&str] = &["hypothesis"];
const PAPER: &[&str] = &["paper"];
const EXPERIMENT: &[&str] = &["experiment"];
const MEASURE: &[&str] = &["measure"];

/// The compiled-in FALLBACK vocabulary.
///
/// The runtime source of truth is `ontology/kanban.ttl` (embedded via `include_str!`,
/// parsed once at first use — see [`predicates`]). This const exists ONLY as the
/// fail-closed landing spot: if the `.ttl` is ever malformed, the vocabulary falls back
/// to this known-strict set (with a stderr warning) rather than silently opening up or
/// crashing the CLI. A unit test pins `.ttl` ⇔ fallback parity, so any divergence is a
/// red test, not a silent drift.
///
/// Deliberately NOT included: `kb:acfDimension` is a value constraint
/// (`sh:in ("AC1" … "AC5")`) — a typed attribute on a hypothesis, not an item→item edge.
/// Modelling it here would misrepresent it.
const FALLBACK_PREDICATES: &[Predicate] = &[
    // ── Item ↔ Item (the two that already existed, now typed) ──
    Predicate {
        name: "related",
        domain: DEV_ANY,
        range: DEV_ANY,
        inverse: None, // symmetric in practice
        doc: "informational link between any two items (the existing --related)",
    },
    Predicate {
        name: "dependsOn",
        domain: DEV_ANY,
        range: DEV_ANY,
        inverse: Some("blocks"),
        doc: "this item is BLOCKED BY the target (the existing --depends-on)",
    },
    Predicate {
        name: "blocks",
        domain: DEV_ANY,
        range: DEV_ANY,
        inverse: Some("dependsOn"),
        doc: "this item BLOCKS the target (inverse of dependsOn; stored one way only)",
    },
    Predicate {
        name: "parent",
        domain: DEV_ANY,
        range: DEV_ANY,
        inverse: None,
        doc: "hierarchical containment — this item is contained by the target",
    },
    // ── Dev-board structure ──
    Predicate {
        name: "implements",
        domain: EXPEDITION,
        range: VOYAGE,
        inverse: Some("spawns"),
        doc: "an expedition delivers work toward a voyage",
    },
    Predicate {
        name: "spawns",
        domain: VOYAGE,
        range: EXPEDITION,
        inverse: Some("implements"),
        doc: "a voyage creates and coordinates an expedition",
    },
    // ── Research board: the H → M → EXPR trio, finally expressible ──
    Predicate {
        name: "tests",
        domain: HYPOTHESIS,
        range: PAPER,
        inverse: None,
        doc: "a hypothesis is tested in the context of a paper",
    },
    Predicate {
        name: "validates",
        domain: EXPERIMENT,
        range: HYPOTHESIS,
        inverse: None,
        doc: "an experiment produces evidence for or against a hypothesis",
    },
    Predicate {
        name: "measures",
        domain: MEASURE,
        // The ontology declares Measure → Experiment, but `/hypothesize` links measures to
        // the HYPOTHESIS. Rather than canonise one and silently invalidate existing
        // practice, both targets are accepted — the mismatch is real and documented in
        // left for a follow-up ruling, not resolved by fiat here.
        range: &["experiment", "hypothesis"],
        inverse: None,
        doc: "a measure tracks the outcome of an experiment (or, per current practice, a hypothesis)",
    },
];

/// The kanban ontology, compiled into the binary — the single source of truth for the
/// edge vocabulary. Embedding (rather than a runtime path) means the data file
/// travels with every installed binary and "file not found" is impossible; the project /
/// user override files the ontology header describes are deliberately NOT consulted for
/// the EDGE VOCABULARY — a local override could silently open validate_edge, violating
/// the fail-closed constraint. (SHACL/template consumers may still honor overrides.)
const KANBAN_TTL: &str = include_str!("../ontology/kanban.ttl");

/// The live vocabulary: `kanban.ttl`'s `owl:ObjectProperty` declarations, parsed once at
/// first use. Fail-closed — a malformed `.ttl` (or one declaring no properties) falls
/// back to [`FALLBACK_PREDICATES`] with a stderr warning, never an opened-up vocabulary.
pub fn predicates() -> &'static [Predicate] {
    static VOCAB: std::sync::LazyLock<&'static [Predicate]> = std::sync::LazyLock::new(|| {
        match parse_object_properties(KANBAN_TTL) {
            Ok(v) if !v.is_empty() => &*Box::leak(v.into_boxed_slice()),
            Ok(_) => {
                eprintln!(
                    "warning: kanban.ttl declares no owl:ObjectProperty — using the compiled fallback vocabulary"
                );
                FALLBACK_PREDICATES
            }
            Err(e) => {
                eprintln!(
                    "warning: kanban.ttl failed to parse ({e}) — using the compiled fallback vocabulary"
                );
                FALLBACK_PREDICATES
            }
        }
    });
    &VOCAB
}

/// The graph-admitted entity classes, loaded from the embedded ontology's
/// `owl:Class` declarations. FAIL-CLOSED like [`predicates`]:
/// a malformed `.ttl` falls back to the BOARD-ONLY compiled set (the
/// [`ItemType`] classes + the three hierarchy classes) — never an opened
/// vocabulary, and never extra prefixes.
pub fn classes() -> &'static [EntityClass] {
    static CLASSES: std::sync::LazyLock<&'static [EntityClass]> =
        std::sync::LazyLock::new(|| match parse_entity_classes(KANBAN_TTL) {
            Ok(v) if !v.is_empty() => &*Box::leak(v.into_boxed_slice()),
            Ok(_) | Err(_) => {
                eprintln!(
                    "warning: kanban.ttl yielded no owl:Class declarations (or failed to parse) — \
                     using the compiled board-only class set"
                );
                let mut fallback: Vec<EntityClass> = ItemType::DEV
                    .iter()
                    .chain(ItemType::RESEARCH.iter())
                    .map(|t| EntityClass {
                        name: t.as_str(),
                        id_prefix: None,
                    })
                    .collect();
                for hierarchy in ["item", "devitem", "researchitem"] {
                    fallback.push(EntityClass {
                        name: hierarchy,
                        id_prefix: None,
                    });
                }
                &*Box::leak(fallback.into_boxed_slice())
            }
        });
    &CLASSES
}

/// Reassemble logical Turtle statements from physical lines (shared by the
/// predicate and class parsers — one reading of the file format, not two).
fn logical_statements(ttl: &str) -> Result<Vec<String>, String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    for raw in ttl.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        current.push(' ');
        current.push_str(line);
        if line == "." || line.ends_with(" .") {
            statements.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        return Err("unterminated statement at end of file".to_string());
    }
    Ok(statements)
}

/// Parse the `owl:Class` declarations. Same subset reader posture as
/// [`parse_object_properties`]: anything unreadable is an `Err`, which
/// [`classes`] treats as fail-closed.
fn parse_entity_classes(ttl: &str) -> Result<Vec<EntityClass>, String> {
    let mut out = Vec::new();
    for stmt in logical_statements(ttl)? {
        let stmt = stmt.trim().trim_end_matches('.').trim();
        let mut clauses = split_outside_quotes(stmt, ';').into_iter();
        let head = clauses.next().unwrap_or("").trim();
        let subject = head.split_whitespace().next().unwrap_or("");
        if !subject.starts_with("kb:") || !head.contains("owl:Class") {
            continue;
        }
        let local = kb_local(subject)?;
        let mut id_prefix = None;
        for clause in clauses {
            let clause = clause.trim();
            if clause.is_empty() {
                continue;
            }
            let (pred, rest) = clause
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("bad clause '{clause}' on kb:{local}"))?;
            if pred == "kb:idPrefix" {
                id_prefix = Some(leak_str(quoted(rest)?.to_ascii_uppercase()));
            }
        }
        out.push(EntityClass {
            name: leak_str(local.to_ascii_lowercase()),
            id_prefix,
        });
    }
    Ok(out)
}

fn leak_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn leak_strs(v: Vec<&'static str>) -> &'static [&'static str] {
    Box::leak(v.into_boxed_slice())
}

/// Extract the local name from a `kb:xxx` term.
fn kb_local(term: &str) -> Result<&str, String> {
    term.trim()
        .strip_prefix("kb:")
        .ok_or_else(|| format!("expected a kb: term, got '{term}'"))
}

/// Map a comma-separated `kb:Class` object list to a domain/range slice.
/// `kb:Item` means "any item type" and maps to the EMPTY slice (the `validate_edge`
/// convention); a comma list of concrete classes is read as a UNION (either accepted) —
/// see the `kb:measures` note in the `.ttl`.
fn class_list(rest: &str) -> Result<&'static [&'static str], String> {
    let mut out = Vec::new();
    for term in rest.split(',') {
        let local = kb_local(term)?;
        if local == "Item" {
            return Ok(&[]); // any — dominates a union
        }
        out.push(leak_str(local.to_ascii_lowercase()));
    }
    if out.is_empty() {
        return Err(format!("empty class list '{rest}'"));
    }
    Ok(leak_strs(out))
}

/// Extract a quoted literal (`"..."`) from a clause remainder.
fn quoted(rest: &str) -> Result<&'static str, String> {
    let start = rest
        .find('"')
        .ok_or_else(|| format!("expected a quoted literal in '{rest}'"))?;
    let end = rest
        .rfind('"')
        .filter(|e| *e > start)
        .ok_or_else(|| format!("unterminated literal in '{rest}'"))?;
    Ok(leak_str(rest[start + 1..end].to_string()))
}

/// Split on `sep`, but never inside a `"..."` literal (so a comment may contain `;`).
fn split_outside_quotes(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut in_quotes) = (0usize, false);
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c == sep && !in_quotes => {
                parts.push(&s[start..i]);
                start = i + sep.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Parse the `owl:ObjectProperty` declarations out of the embedded ontology.
///
/// A deliberately small Turtle SUBSET reader for this one file (no new dependency):
/// line-oriented, `#` full-line comments, single-line `"..."` literals, statements
/// terminated by a line ending in ` .`, clauses separated by `;` (quote-aware, so
/// literals may contain `;`), objects by `,`. Anything the reader cannot make sense of
/// is an `Err` — which [`predicates`] treats as fail-closed, never as "accept
/// everything".
fn parse_object_properties(ttl: &str) -> Result<Vec<Predicate>, String> {
    let statements = logical_statements(ttl)?;

    let mut out: Vec<Predicate> = Vec::new();
    for stmt in &statements {
        let stmt = stmt.trim().trim_end_matches('.').trim();
        let mut clauses = split_outside_quotes(stmt, ';').into_iter();
        let head = clauses.next().unwrap_or("").trim();
        // Only `kb:<name> a owl:ObjectProperty` statements are vocabulary.
        let subject = head.split_whitespace().next().unwrap_or("");
        if !subject.starts_with("kb:") || !head.contains("owl:ObjectProperty") {
            continue;
        }
        let local = kb_local(subject)?;

        let (mut domain, mut range): (&'static [&'static str], &'static [&'static str]) =
            (&[], &[]);
        let (mut inverse, mut label, mut doc) = (None, None, None);
        for clause in clauses {
            let clause = clause.trim();
            if clause.is_empty() {
                continue;
            }
            let (pred, rest) = clause
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("bad clause '{clause}' on kb:{local}"))?;
            match pred {
                "rdfs:domain" => domain = class_list(rest)?,
                "rdfs:range" => range = class_list(rest)?,
                "owl:inverseOf" => inverse = Some(leak_str(kb_local(rest)?.to_string())),
                "rdfs:label" => label = Some(quoted(rest)?),
                "rdfs:comment" => doc = Some(quoted(rest)?),
                // Tolerate future metadata clauses — fail-closed is about the edge
                // SET and its domain/range, not about extra annotations.
                _ => {}
            }
        }
        out.push(Predicate {
            name: label.unwrap_or_else(|| leak_str(local.to_string())),
            domain,
            range,
            inverse,
            doc: doc.unwrap_or(""),
        });
    }

    // Referential sanity: every declared inverse must resolve within the set.
    for p in &out {
        if let Some(inv) = p.inverse
            && !out.iter().any(|q| q.name == inv)
        {
            return Err(format!("'{}' names a missing inverse '{inv}'", p.name));
        }
    }
    Ok(out)
}

/// Look up a predicate by name (case-insensitive; `depends_on`/`depends-on` accepted for
/// `dependsOn`, so the CLI spelling of the existing flag keeps working).
pub fn lookup(name: &str) -> Option<&'static Predicate> {
    let n = name.trim().to_ascii_lowercase().replace(['_', '-'], "");
    predicates()
        .iter()
        .find(|p| p.name.to_ascii_lowercase() == n)
}

/// All predicate names, for `--help` and error messages.
pub fn names() -> Vec<&'static str> {
    predicates().iter().map(|p| p.name).collect()
}

/// Why a proposed edge was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeError {
    /// The predicate is not in the vocabulary.
    UnknownPredicate {
        given: String,
        known: Vec<&'static str>,
    },
    /// The source item's type is not in the predicate's domain.
    BadDomain {
        predicate: &'static str,
        got: String,
        allowed: Vec<&'static str>,
    },
    /// The target item's type is not in the predicate's range.
    BadRange {
        predicate: &'static str,
        got: String,
        allowed: Vec<&'static str>,
    },
    /// An item cannot relate to itself.
    SelfEdge { id: String },
}

impl std::fmt::Display for EdgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EdgeError::UnknownPredicate { given, known } => write!(
                f,
                "unknown relationship '{given}' — known: {}",
                known.join(", ")
            ),
            EdgeError::BadDomain {
                predicate,
                got,
                allowed,
            } => write!(
                f,
                "'{predicate}' cannot start from a {got} (allowed: {})",
                allowed.join(", ")
            ),
            EdgeError::BadRange {
                predicate,
                got,
                allowed,
            } => write!(
                f,
                "'{predicate}' cannot point at a {got} (allowed: {})",
                allowed.join(", ")
            ),
            EdgeError::SelfEdge { id } => write!(f, "{id} cannot relate to itself"),
        }
    }
}

/// Validate a proposed edge against the vocabulary.
///
/// `source_type` / `target_type` are the item types (e.g. `"expedition"`). A `None` target
/// type means the target could not be resolved — validation of RANGE is then skipped
/// rather than guessed, so a cross-board edge to an item this store cannot see is still
/// allowed. Refusing an edge because we could not look up its target would make the
/// research trio unusable across boards, which is the point of adding it.
pub fn validate_edge(
    predicate: &str,
    source_id: &str,
    source_type: &str,
    target_id: &str,
    target_type: Option<&str>,
) -> std::result::Result<&'static Predicate, EdgeError> {
    let p = lookup(predicate).ok_or_else(|| EdgeError::UnknownPredicate {
        given: predicate.to_string(),
        known: names(),
    })?;

    if source_id.eq_ignore_ascii_case(target_id) {
        return Err(EdgeError::SelfEdge {
            id: source_id.to_string(),
        });
    }

    if !p.domain.is_empty() && !p.domain.iter().any(|d| d.eq_ignore_ascii_case(source_type)) {
        return Err(EdgeError::BadDomain {
            predicate: p.name,
            got: source_type.to_string(),
            allowed: p.domain.to_vec(),
        });
    }
    if let Some(tt) = target_type
        && !p.range.is_empty()
        && !p.range.iter().any(|r| r.eq_ignore_ascii_case(tt))
    {
        return Err(EdgeError::BadRange {
            predicate: p.name,
            got: tt.to_string(),
            allowed: p.range.to_vec(),
        });
    }
    Ok(p)
}

/// Parse a `predicate:TARGET-ID` CLI argument.
///
/// Splits on the FIRST colon only, so an ID containing a colon survives.
pub fn parse_spec(spec: &str) -> Option<(&str, &str)> {
    let (pred, target) = spec.split_once(':')?;
    let (pred, target) = (pred.trim(), target.trim());
    (!pred.is_empty() && !target.is_empty()).then_some((pred, target))
}

/// Whether a predicate's edges are ALSO projected into the flat `items.related` /
/// `items.depends_on` columns, so existing readers (`roadmap`, `critical-path`,
/// `worklist`, `arrow-kanban show`) keep working unchanged.
pub fn flat_column_for(predicate: &str) -> Option<&'static str> {
    match lookup(predicate)?.name {
        "related" => Some("related"),
        "dependsOn" => Some("depends_on"),
        _ => None,
    }
}

/// The item type of an ID, inferred from its prefix — for validating an edge whose target
/// this store cannot resolve (e.g. a cross-board research item).
pub fn type_from_id(id: &str) -> Option<&'static str> {
    let prefix = id.split('-').next()?.to_ascii_uppercase();
    // Data-driven classes first consult nothing: the compiled arms below stay
    // authoritative for board types; a prefix they do not know falls through to
    // the ontology-declared entity classes (`kb:idPrefix`), so e.g. a `PROP-`
    // prefixed id types as `proposal` the moment the `.ttl` declares it.
    let compiled = match prefix.as_str() {
        "EX" | "EXP" => "expedition",
        "VY" | "VOY" => "voyage",
        "CA" => "campaign",
        "CH" | "CHORE" => "chore",
        "HZ" => "hazard",
        "SG" | "SIG" => "signal",
        "FT" | "FEAT" => "feature",
        "H" => "hypothesis",
        "EXPR" => "experiment",
        "M" => "measure",
        "PAPER" => "paper",
        "LIT" => "literature",
        "IDEA" => "idea",
        _ => "",
    };
    if !compiled.is_empty() {
        return Some(compiled);
    }
    classes()
        .iter()
        .find(|c| c.id_prefix.is_some_and(|p| p == prefix))
        .map(|c| c.name)
}

/// Best-effort item type for an ID: the store's answer if it has one, else the prefix.
pub fn resolve_type(id: &str, from_store: Option<&str>) -> Option<String> {
    from_store
        .map(str::to_string)
        .or_else(|| type_from_id(id).map(str::to_string))
}

/// Convenience: does this ItemType satisfy the named predicate's domain?
pub fn domain_allows(predicate: &str, t: ItemType) -> bool {
    match lookup(predicate) {
        Some(p) => {
            p.domain.is_empty() || p.domain.iter().any(|d| d.eq_ignore_ascii_case(t.as_str()))
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vocabulary_covers_declared_predicates() {
        // The seven declared owl:ObjectProperty terms from .arrow-kanban/ontology/kanban.ttl…
        for p in [
            "related",
            "dependsOn",
            "implements",
            "spawns",
            "tests",
            "validates",
            "measures",
        ] {
            assert!(lookup(p).is_some(), "missing declared predicate: {p}");
        }
        // …plus the two its SHACL shapes reference.
        for p in ["blocks", "parent"] {
            assert!(
                lookup(p).is_some(),
                "missing shape-referenced predicate: {p}"
            );
        }
    }

    /// acfDimension is an sh:in value constraint, not an edge. Modelling it as a
    /// relationship would misrepresent the ontology.
    #[test]
    fn acf_dimension_is_not_a_relationship() {
        assert!(lookup("acfDimension").is_none());
    }

    #[test]
    fn lookup_is_forgiving_about_cli_spelling() {
        assert_eq!(lookup("dependsOn").map(|p| p.name), Some("dependsOn"));
        assert_eq!(lookup("depends_on").map(|p| p.name), Some("dependsOn"));
        assert_eq!(lookup("depends-on").map(|p| p.name), Some("dependsOn"));
        assert_eq!(lookup("DEPENDSON").map(|p| p.name), Some("dependsOn"));
        assert!(lookup("nonsense").is_none());
    }

    #[test]
    fn the_research_trio_is_expressible() {
        // The whole point: H → M → EXPR, which --related could not express with direction.
        assert!(
            validate_edge(
                "validates",
                "EXPR-1",
                "experiment",
                "H-2",
                Some("hypothesis")
            )
            .is_ok()
        );
        assert!(validate_edge("measures", "M-3", "measure", "EXPR-1", Some("experiment")).is_ok());
        assert!(validate_edge("tests", "H-2", "hypothesis", "PAPER-4", Some("paper")).is_ok());
    }

    /// The ontology says Measure → Experiment; /hypothesize links measures to the
    /// HYPOTHESIS. Both are accepted rather than silently invalidating live practice.
    #[test]
    fn measures_accepts_both_the_ontology_and_current_practice() {
        assert!(validate_edge("measures", "M-1", "measure", "EXPR-2", Some("experiment")).is_ok());
        assert!(validate_edge("measures", "M-1", "measure", "H-2", Some("hypothesis")).is_ok());
        // But not something unrelated.
        assert!(validate_edge("measures", "M-1", "measure", "CH-9", Some("chore")).is_err());
    }

    #[test]
    fn domain_and_range_are_enforced() {
        // A chore cannot implement a voyage — implements is Expedition → Voyage.
        let e = validate_edge("implements", "CH-1", "chore", "VY-2", Some("voyage")).unwrap_err();
        assert!(matches!(e, EdgeError::BadDomain { .. }), "got {e:?}");
        // An expedition cannot implement a chore.
        let e =
            validate_edge("implements", "EX-1", "expedition", "CH-2", Some("chore")).unwrap_err();
        assert!(matches!(e, EdgeError::BadRange { .. }), "got {e:?}");
    }

    /// A target this store cannot resolve must NOT be rejected — the research trio is
    /// cross-board, and refusing unresolvable targets would make it unusable.
    #[test]
    fn an_unresolvable_target_skips_range_validation_rather_than_failing() {
        assert!(validate_edge("validates", "EXPR-1", "experiment", "H-999", None).is_ok());
        // Domain is still enforced — we DO know the source's type.
        assert!(validate_edge("implements", "CH-1", "chore", "VY-999", None).is_err());
    }

    #[test]
    fn self_edges_are_refused() {
        let e =
            validate_edge("related", "EX-1", "expedition", "EX-1", Some("expedition")).unwrap_err();
        assert!(matches!(e, EdgeError::SelfEdge { .. }));
        // …case-insensitively.
        assert!(
            validate_edge("related", "EX-1", "expedition", "ex-1", Some("expedition")).is_err()
        );
    }

    #[test]
    fn unknown_predicates_name_the_alternatives() {
        let e =
            validate_edge("frobnicates", "EX-1", "expedition", "VY-2", Some("voyage")).unwrap_err();
        match &e {
            EdgeError::UnknownPredicate { known, given } => {
                assert_eq!(given, "frobnicates");
                assert!(
                    known.contains(&"implements"),
                    "the error should list real options"
                );
            }
            other => panic!("expected UnknownPredicate, got {other:?}"),
        }
        // The message a user actually sees must name both the bad input and the way out.
        let msg = e.to_string();
        assert!(
            msg.contains("frobnicates"),
            "message should name the bad predicate: {msg}"
        );
        assert!(
            msg.contains("implements"),
            "message should list known predicates: {msg}"
        );
    }

    #[test]
    fn spec_parsing_splits_on_the_first_colon_only() {
        assert_eq!(
            parse_spec("implements:VY-1234"),
            Some(("implements", "VY-1234"))
        );
        assert_eq!(parse_spec(" validates : H-5 "), Some(("validates", "H-5")));
        // An ID containing a colon survives.
        assert_eq!(parse_spec("related:NS:ID-7"), Some(("related", "NS:ID-7")));
        assert_eq!(parse_spec("noseparator"), None);
        assert_eq!(parse_spec("implements:"), None);
        assert_eq!(parse_spec(":VY-1"), None);
    }

    /// Only related/dependsOn project into the flat columns — that projection is what
    /// keeps roadmap/critical-path/worklist working unchanged.
    #[test]
    fn only_the_two_legacy_predicates_project_to_flat_columns() {
        assert_eq!(flat_column_for("related"), Some("related"));
        assert_eq!(flat_column_for("dependsOn"), Some("depends_on"));
        assert_eq!(flat_column_for("depends_on"), Some("depends_on"));
        assert_eq!(flat_column_for("implements"), None);
        assert_eq!(flat_column_for("validates"), None);
    }

    #[test]
    fn ids_resolve_to_types_by_prefix() {
        assert_eq!(type_from_id("EX-6109"), Some("expedition"));
        assert_eq!(type_from_id("VY-5855"), Some("voyage"));
        // CA- (campaign) must resolve, else partOf:CA-XXXX against an
        // unresolvable target types as None and range validation silently skips.
        assert_eq!(type_from_id("CA-6256"), Some("campaign"));
        assert_eq!(type_from_id("CH-1"), Some("chore"));
        assert_eq!(type_from_id("H-5924"), Some("hypothesis"));
        assert_eq!(type_from_id("EXPR-6073"), Some("experiment"));
        assert_eq!(type_from_id("M-6072"), Some("measure"));
        assert_eq!(type_from_id("PAPER-122"), Some("paper"));
        // FT (Feature) was missing — an unresolvable FT-XXXX target typed as None,
        // silently SKIPPING range validation (the DGX1-caught CA class of hole). Now covered.
        assert_eq!(type_from_id("FT-6256"), Some("feature"));
        assert_eq!(type_from_id("FEAT-1"), Some("feature")); // legacy prefix too
        assert_eq!(type_from_id("WAT-1"), None);
    }

    /// Structural guard against the NEXT such gap: every board item type's canonical
    /// `prefix()` must resolve through `type_from_id` back to that type. `type_from_id` is the
    /// fallback typer for edge targets the store cannot resolve; a prefix with no arm types as
    /// `None` and range validation silently skips (FT was the latest instance; CA was DGX1's on
    /// variant). Iterating `DEV ∪ RESEARCH` means a newly-added ItemType (e.g. Campaign) turns
    /// this RED until its `type_from_id` arm is added — the gap can't ship silently again.
    #[test]
    fn every_item_type_prefix_round_trips_through_type_from_id() {
        for t in ItemType::DEV.iter().chain(ItemType::RESEARCH.iter()) {
            let id = format!("{}-1", t.prefix());
            assert_eq!(
                type_from_id(&id),
                Some(t.as_str()),
                "ItemType::{t:?} (prefix '{}') has NO type_from_id arm — an unresolvable \
                 {id} would type as None and silently skip range validation",
                t.prefix()
            );
        }
    }

    /// Inverses are declared but only ONE direction is ever stored — storing both would
    /// let them disagree.
    #[test]
    fn inverses_are_declared_and_mutually_consistent() {
        let d = lookup("dependsOn").unwrap();
        let b = lookup("blocks").unwrap();
        assert_eq!(d.inverse, Some("blocks"));
        assert_eq!(b.inverse, Some("dependsOn"));
        let i = lookup("implements").unwrap();
        let s = lookup("spawns").unwrap();
        assert_eq!(i.inverse, Some("spawns"));
        assert_eq!(s.inverse, Some("implements"));
        // Every declared inverse must itself be a real predicate pointing back.
        for p in predicates() {
            if let Some(inv) = p.inverse {
                let q = lookup(inv)
                    .unwrap_or_else(|| panic!("{} names a missing inverse {inv}", p.name));
                assert_eq!(q.inverse, Some(p.name), "{} and {inv} disagree", p.name);
            }
        }
    }

    // ── The .ttl is the source of truth; the const is a fail-closed fallback ──

    /// The acceptance test, later relaxed from strict equality to
    /// fallback ⊆ loaded: every compiled-fallback predicate must appear in the `.ttl`
    /// with identical (domain, range, inverse) — so the original nine can never drift
    /// silently — while the `.ttl` may GROW data-only predicates (`partOf`/`hasMember`
    /// are the first) that the strict-fallback deliberately does not carry. The
    /// fallback staying narrower is the fail-closed direction: a malformed `.ttl`
    /// loses the data-only additions, never gains an open vocabulary.
    #[test]
    fn every_fallback_predicate_is_pinned_in_the_ttl() {
        let loaded = parse_object_properties(KANBAN_TTL).expect("embedded kanban.ttl parses");
        assert!(
            loaded.len() >= FALLBACK_PREDICATES.len(),
            "the .ttl lost predicates: ttl={:?} fallback={:?}",
            loaded.iter().map(|p| p.name).collect::<Vec<_>>(),
            FALLBACK_PREDICATES
                .iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
        );
        for f in FALLBACK_PREDICATES {
            let l = loaded
                .iter()
                .find(|p| p.name == f.name)
                .unwrap_or_else(|| panic!("'{}' missing from the .ttl", f.name));
            assert_eq!(l.domain, f.domain, "'{}' domain differs", f.name);
            assert_eq!(l.range, f.range, "'{}' range differs", f.name);
            assert_eq!(l.inverse, f.inverse, "'{}' inverse differs", f.name);
        }
    }

    // ── The vocabulary grows by editing DATA — the open-graph property ──

    /// Acceptance test: `partOf`/`hasMember` exist ONLY in `kanban.ttl`
    /// — not in the compiled fallback — yet validate, look up, and inverse-resolve
    /// exactly like the compiled nine. This is add-a-relation-via-data-only working.
    #[test]
    fn partof_membership_pair_is_loaded_from_data_only() {
        // Not in the fallback const — the growth is data-only, no Rust vocab edit.
        for name in ["partOf", "hasMember"] {
            assert!(
                FALLBACK_PREDICATES.iter().all(|p| p.name != name),
                "'{name}' must NOT be compiled in — it proves the .ttl is the source"
            );
        }
        let p = lookup("partOf").expect("partOf loaded from kanban.ttl");
        assert_eq!(
            p.domain,
            &[] as &[&str],
            "membership crosses types: any → any"
        );
        assert_eq!(p.range, &[] as &[&str]);
        assert_eq!(p.inverse, Some("hasMember"));
        let h = lookup("hasMember").expect("hasMember loaded from kanban.ttl");
        assert_eq!(h.inverse, Some("partOf"));
        // CLI spellings are as forgiving as for the compiled predicates.
        assert_eq!(lookup("part_of").map(|p| p.name), Some("partOf"));
        assert_eq!(lookup("part-of").map(|p| p.name), Some("partOf"));
        // A membership edge validates end-to-end, including a cross-board target the
        // store cannot resolve (typed-by-prefix / None → range check skipped).
        assert!(validate_edge("partOf", "VY-1", "voyage", "CAMP-9", None).is_ok());
        assert!(
            validate_edge("partOf", "VY-1", "voyage", "VY-2", Some("voyage")).is_ok(),
            "voyage → campaign-node-as-voyage membership must validate"
        );
        // Unknown predicates are still refused — loading MORE never opened the vocab.
        assert!(validate_edge("memberOfCampaign", "VY-1", "voyage", "VY-2", None).is_err());
        // Explicit design decision: partOf does NOT auto-project into a
        // flat items column — no current consumer reads one, and the campaign rollup
        // traverses the typed edges.
        assert_eq!(flat_column_for("partOf"), None);
        assert_eq!(flat_column_for("hasMember"), None);
    }

    /// Regression pin: the four DECISION/OBLIGATION pairs — closes/closedBy,
    /// supersedes/supersededBy, leavesRemainder/remainderOf, ruledBy/scopes — exist ONLY
    /// in `kanban.ttl`, not in the compiled fallback, yet validate, look up, and
    /// inverse-resolve exactly like the compiled set. Same acceptance shape as
    /// `partof_membership_pair_is_loaded_from_data_only`, because it guards the same
    /// property: the vocabulary grows by editing DATA. Pinned so that this test
    /// goes RED when the .ttl blocks are removed, so a green pre-existing suite can
    /// never again read as "the predicates loaded" while they did not.
    #[test]
    fn decision_obligation_pairs_are_loaded_from_data_only() {
        let pairs = [
            ("closes", "closedBy"),
            ("supersedes", "supersededBy"),
            ("leavesRemainder", "remainderOf"),
            ("ruledBy", "scopes"),
        ];
        for (fwd, inv) in pairs {
            for name in [fwd, inv] {
                assert!(
                    FALLBACK_PREDICATES.iter().all(|p| p.name != name),
                    "'{name}' must NOT be compiled in — it proves the .ttl is the source"
                );
            }
            let p = lookup(fwd).unwrap_or_else(|| panic!("'{fwd}' not loaded from kanban.ttl"));
            if fwd == "closes" {
                // The earlier any→any domain/range was explicitly provisional
                // ("a Proposal is not a registered ItemType until kanban-v3");
                // Proposal is registered now, so the real domain applies.
                assert_eq!(p.domain, &["proposal"], "'closes' is Proposal → Item");
                assert_eq!(p.range, &[] as &[&str]);
            } else {
                assert_eq!(p.domain, &[] as &[&str], "'{fwd}' is any -> any (kb:Item)");
                assert_eq!(p.range, &[] as &[&str]);
            }
            assert_eq!(p.inverse, Some(inv), "'{fwd}' names its inverse");
            let q = lookup(inv).unwrap_or_else(|| panic!("'{inv}' not loaded from kanban.ttl"));
            assert_eq!(q.inverse, Some(fwd), "'{inv}' points back at '{fwd}'");
            if fwd == "closes" {
                assert_eq!(q.range, &["proposal"], "'closedBy' range is Proposal");
            }
        }
        // A decision edge validates end-to-end, including a proposal-shaped source id the
        // item store cannot resolve (relations rows are plain Utf8; PROP- is a valid
        // source). `PROP-` now prefix-types as `proposal` (kb:idPrefix), so the
        // tightened Proposal→Item domain validates — and a NON-proposal source REFUSES,
        // which is the enforcement the provisional edge shipped without.
        assert_eq!(type_from_id("PROP-1"), Some("proposal"));
        assert!(validate_edge("closes", "PROP-1", "proposal", "CH-2", Some("chore")).is_ok());
        assert!(
            validate_edge("closes", "CH-9", "chore", "VY-2", Some("voyage")).is_err(),
            "a Chore must no longer be able to 'close' a Voyage (the entity-class tightening)"
        );
        assert!(validate_edge("remainderOf", "CH-3", "chore", "PROP-1", None).is_ok());
        // Loading MORE never opened the vocabulary.
        assert!(validate_edge("closesish", "PROP-1", "chore", "CH-2", None).is_err());
        // No flat-column projection for any of the eight — consumers traverse the
        // typed edges (same posture as partOf/hasMember).
        for (fwd, inv) in pairs {
            assert_eq!(flat_column_for(fwd), None);
            assert_eq!(flat_column_for(inv), None);
        }
    }

    /// The entity classes are LOADED FROM DATA — removing the `.ttl`
    /// class block turns this RED (the red-first pin prescribed for every
    /// vocabulary addition: a green pre-existing suite proves nothing loaded).
    #[test]
    fn ex7101_entity_classes_are_loaded_from_data_only() {
        // None of the five is a board ItemType — they must come from the ontology.
        for name in ["proposal", "comment", "run", "experimentrun", "ciresult"] {
            assert!(
                classes().iter().any(|c| c.name == name),
                "'{name}' not loaded from kanban.ttl — the class block is missing or unparsed"
            );
            assert!(
                !ItemType::DEV
                    .iter()
                    .chain(ItemType::RESEARCH.iter())
                    .any(|t| t.as_str() == name),
                "'{name}' must NOT be a board ItemType — EntityClass is the non-board register"
            );
        }
        // Prefix resolution is data-driven: PROP/CMT come from kb:idPrefix, with the
        // compiled board arms untouched and authoritative first.
        assert_eq!(type_from_id("PROP-2001"), Some("proposal"));
        assert_eq!(type_from_id("CMT-CH-1-99"), Some("comment"));
        assert_eq!(
            type_from_id("EX-1"),
            Some("expedition"),
            "compiled arms unchanged"
        );
        assert_eq!(
            type_from_id("WAT-1"),
            None,
            "unknown prefixes still resolve to None"
        );
        // The FK vocabulary (Phase 4) is loaded and correctly constrained.
        let c = lookup("commentOn").expect("commentOn loaded");
        assert_eq!(c.domain, &["comment"]);
        let e = lookup("evaluates").expect("evaluates loaded");
        assert_eq!(
            (e.domain, e.range),
            (&["ciresult"] as &[&str], &["proposal"] as &[&str])
        );
        let t = lookup("transitions").expect("transitions loaded");
        assert_eq!(t.domain, &["run"]);
        // And loading more never opened the vocabulary or the class set.
        assert!(validate_edge("commentOn", "CH-1", "chore", "CH-2", Some("chore")).is_err());
        assert!(classes().iter().all(|c| c.name != "watnot"));
    }

    /// The live vocabulary IS the .ttl-loaded one (not the fallback): the loader
    /// succeeded on the embedded file, so `predicates()` must expose a set identical to
    /// a fresh parse (i.e. the fallback never silently took over).
    #[test]
    fn live_vocabulary_is_the_ttl_loaded_set() {
        let fresh = parse_object_properties(KANBAN_TTL).expect("embedded kanban.ttl parses");
        let live = predicates();
        assert_eq!(live.len(), fresh.len());
        for f in &fresh {
            assert!(
                live.iter()
                    .any(|p| p.name == f.name && p.doc == f.doc && p.inverse == f.inverse),
                "'{}' not served by predicates()",
                f.name
            );
        }
    }

    /// Fail-closed: malformed input must ERROR (and thereby trigger the fallback),
    /// never yield a partial/open vocabulary.
    #[test]
    fn parser_fails_closed_on_malformed_input() {
        // Unterminated statement.
        assert!(
            parse_object_properties("kb:x a owl:ObjectProperty ;\n  rdfs:label \"x\"").is_err()
        );
        // A non-kb domain term.
        assert!(
            parse_object_properties(
                "kb:x a owl:ObjectProperty ; rdfs:domain foaf:Agent ; rdfs:label \"x\" ."
            )
            .is_err()
        );
        // An inverse that names a predicate not in the set.
        assert!(
            parse_object_properties(
                "kb:x a owl:ObjectProperty ; owl:inverseOf kb:ghost ; rdfs:label \"x\" ."
            )
            .is_err()
        );
        // An unterminated string literal.
        assert!(parse_object_properties("kb:x a owl:ObjectProperty ; rdfs:label \"x .").is_err());
    }

    /// `kb:Item` maps to the empty slice (= any item type), and a comma list of concrete
    /// classes is read as a union — the two domain/range conventions the validator uses.
    #[test]
    fn class_lists_map_item_to_any_and_commas_to_unions() {
        let v = parse_object_properties(
            "kb:x a owl:ObjectProperty ; rdfs:domain kb:Item ; rdfs:range kb:Experiment , kb:Hypothesis ; rdfs:label \"x\" .",
        )
        .expect("parses");
        assert_eq!(v[0].domain, &[] as &[&str], "kb:Item means ANY");
        assert_eq!(v[0].range, &["experiment", "hypothesis"]);
    }

    /// Class and datatype-property statements in the same file are ignored — only
    /// owl:ObjectProperty declarations become vocabulary.
    #[test]
    fn non_object_property_statements_are_ignored() {
        let v = parse_object_properties(
            "kb:Item a owl:Class ; rdfs:label \"Item\" .\n\
             kb:status a owl:DatatypeProperty ; rdfs:domain kb:Item ; rdfs:label \"status\" .\n\
             kb:rel a owl:ObjectProperty ; rdfs:domain kb:Item ; rdfs:range kb:Item ; rdfs:label \"rel\" .",
        )
        .expect("parses");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "rel");
    }

    // ── Single-source-of-truth guards + back-compat by construction ──────

    /// Guard (structural sanity): every domain/range entry of every LOADED
    /// predicate must name a real [`ItemType`]. A typo'd class in the `.ttl`
    /// (`kb:Expeditionn`) would otherwise create a DEAD constraint — an edge no item
    /// type can ever satisfy — silently.
    ///
    /// Iterates `DEV ∪ RESEARCH` (the same auto-covering shape as the item-type guard's
    /// round-trip guard), so a future ItemType is known here the moment it joins a
    /// board — no hand-list to update. The original `campaign` ahead-of-code
    /// allowlist was tightened away once the enum variant landed (the
    /// cross-review agreement).
    #[test]
    fn every_ttl_domain_and_range_names_a_real_item_type() {
        // The known universe is board ItemTypes ∪ the ontology's own
        // declared entity classes — a typo'd class STILL fails (it is declared
        // nowhere), while a declared non-board class (Proposal) is legal.
        let mut known: Vec<&str> = ItemType::DEV
            .iter()
            .chain(ItemType::RESEARCH.iter())
            .map(|t| t.as_str())
            .collect();
        known.extend(classes().iter().map(|c| c.name));
        assert!(
            known.contains(&"campaign"),
            "Campaign must be a board type (EX-6249) — the allowlist era is over"
        );
        for p in predicates() {
            for entry in p.domain.iter().chain(p.range.iter()) {
                assert!(
                    known.contains(entry),
                    "'{}' constrains on unknown item type '{entry}' — a dead constraint \
                     (typo in kanban.ttl?)",
                    p.name
                );
            }
        }
    }

    /// Guard (no divergent second copy): the loader deliberately reads ONLY
    /// the embedded ontology (see [`KANBAN_TTL`]), so a `.arrow-kanban/ontology/`
    /// copy would be dead data — and a DIVERGENT dead copy is worse: the next reader
    /// trusts whichever they open first. No such copy exists today (this asserts
    /// vacuously); if one ever appears it must be byte-identical to the embedded
    /// source of truth or this goes red.
    #[test]
    fn any_override_ontology_copy_must_match_the_embedded_source_of_truth() {
        let workspace_copy = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.arrow-kanban/ontology/kanban.ttl");
        if let Ok(contents) = std::fs::read_to_string(&workspace_copy) {
            assert_eq!(
                contents, KANBAN_TTL,
                "a second kanban.ttl exists at {workspace_copy:?} and DIVERGES from the \
                 embedded source of truth — delete it or sync it (the loader ignores it \
                 either way, so divergence only misleads readers)"
            );
        }
    }

    /// Back-compat, pinned BY CONSTRUCTION rather than by a point-in-time
    /// store sweep — the three links of the argument, each checkable:
    ///
    /// 1. FLAT edges (`items.related`/`items.depends_on`, the entire pre-typed-edge
    ///    population) resolve to loaded predicates that are UNCONSTRAINED
    ///    (any→any) — so no stored flat edge can fail domain/range under the loaded
    ///    vocab, ever. (This test.)
    /// 2. TYPED edges were validated by `validate_edge` AT WRITE TIME, and
    ///    `every_fallback_predicate_is_pinned_in_the_ttl` proves the vocabulary
    ///    only ever GREW (the original nine keep identical domain/range/inverse) —
    ///    monotonic growth cannot invalidate a previously-accepted edge.
    /// 3. `lookup` normalization (`depends_on`/`depends-on` → `dependsOn`) still
    ///    resolves the CLI spellings stored histories may reference.
    ///
    /// (A live-store sweep was considered and rejected: typed edges currently have
    /// NO CLI read surface — flagged during review — and flat edges are proven safe
    /// here more strongly than any sample could.)
    #[test]
    fn stored_edges_cannot_be_invalidated_by_the_loaded_vocab() {
        for flat in ["related", "dependsOn"] {
            let p = lookup(flat).unwrap_or_else(|| panic!("'{flat}' missing from vocab"));
            assert!(
                p.domain.is_empty() && p.range.is_empty(),
                "'{flat}' must stay unconstrained (any→any) — narrowing it would \
                 retroactively invalidate stored flat edges"
            );
        }
        // The CLI spellings that may appear in histories still resolve.
        assert_eq!(lookup("depends_on").map(|p| p.name), Some("dependsOn"));
        assert_eq!(lookup("depends-on").map(|p| p.name), Some("dependsOn"));
        // And every loaded predicate validates a well-typed edge end-to-end (the
        // write path all typed edges passed through).
        assert!(
            validate_edge("partOf", "VY-6244", "voyage", "EX-6245", None).is_ok(),
            "an unresolvable-target edge must remain acceptable (cross-board rule)"
        );
    }

    /// Count `kb:<name> a owl:ObjectProperty` DECLARATIONS in the ontology text, without
    /// calling the production parser.
    ///
    /// Re-derives the parser's own discrimination rule (`relation_vocab::parse_object_properties`:
    /// a statement whose SUBJECT starts with `kb:` and whose HEAD clause contains
    /// `owl:ObjectProperty`) in a deliberately different, simpler way — statement-terminator
    /// split rather than the quote-aware clause reader.
    ///
    /// WHY NOT the obvious `ttl.matches("owl:ObjectProperty").count()`: that counts MENTIONS,
    /// not declarations, and `kanban.ttl` contains a prose comment naming the term. The naive
    /// count is 23; the real declaration count is 22. That one-off is not hypothetical — it is
    /// how a published write-up came to invite readers to "verify the 23-predicate count", a
    /// number no test asserted and which does not reproduce. Use-vs-mention, in the wild.
    ///
    /// HONEST LIMIT, stated rather than implied: an independent re-derivation catches "the
    /// loader dropped a declaration" and "the .ttl gained one the loader cannot see". It cannot
    /// catch a rule BOTH implementations get wrong the same way. Calling the parser here would
    /// remove even that, by comparing the parser to itself.
    fn declared_object_properties(ttl: &str) -> usize {
        ttl.split('\n')
            .map(|l| l.split('#').next().unwrap_or("")) // drop full-line + trailing comments
            .collect::<Vec<_>>()
            .join("\n")
            .split(" .")
            .filter(|stmt| {
                let head = stmt.split(';').next().unwrap_or("").trim();
                let subject = head.split_whitespace().next().unwrap_or("");
                subject.starts_with("kb:") && head.contains("owl:ObjectProperty")
            })
            .count()
    }

    /// The loaded predicate COUNT is pinned to the ontology, never to a literal.
    ///
    /// The existing data-only guards pin specific PAIRS; none pinned the total, so the number
    /// drifts silently every time someone adds a predicate — and a published "verify N" claim
    /// had nothing behind it.
    ///
    /// BOTH SIDES ARE DERIVED. A hardcoded `assert_eq!(22)` would need editing on every
    /// legitimate addition and would go RED for the RIGHT reason at the WRONG time — a correct
    /// change failing a test, which is how a test gets deleted instead of fixed.
    #[test]
    fn loaded_predicate_count_matches_the_ontology_declaration_count() {
        let declared = declared_object_properties(KANBAN_TTL);
        let loaded = predicates().len();

        // Non-vacuity: a counter that returns 0 would make the assertion below trivially
        // satisfiable if the loader ever fell back to an empty set.
        assert!(
            declared >= FALLBACK_PREDICATES.len(),
            "the ontology must declare at least the compiled fallback set ({} declared, {} \
             compiled) — a smaller count means the counter is not seeing the declarations",
            declared,
            FALLBACK_PREDICATES.len()
        );

        assert_eq!(
            loaded, declared,
            "the loader produced {loaded} predicate(s) but ontology/kanban.ttl declares \
             {declared}. Neither number is hardcoded here: if you added a predicate to the \
             .ttl this test stays green by construction, so a mismatch means the loader and \
             the ontology genuinely disagree — most likely a parse the loader rejected (it \
             fails CLOSED to the compiled fallback) rather than a miscount."
        );
    }
}
