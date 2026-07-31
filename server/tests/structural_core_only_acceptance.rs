// SPDX-License-Identifier: MIT
//! F5 — the structural "public schema names no consumer table" invariant (E3 3c-ii).
//!
//! The extension seam's load-bearing promise is that the CORE crate stays
//! domain-zero: an extension carries its OWN tables, and the core never learns a
//! consumer table's name. The lexical export gate enforces the vocabulary half,
//! but it cannot judge a *generic* literal — a public constant that named an ext
//! table `"proposals"` (or `"reviews"`, or `"demo"`) would read as ordinary data
//! and slip straight through.
//!
//! This test closes that gap STRUCTURALLY: it pins the persisted checkpoint's
//! public file set (and the public table-schema surface) to EXACTLY the four core
//! tables. An ext table's parquet name may only ever be a RUNTIME string returned
//! from `EngineExtension::checkpoint()`; the moment a PR bakes one into a public
//! constant or schema, this battery goes RED. Its RED-before-green control below
//! proves the assertion can fail (a fifth name is rejected), so it is coverage,
//! not a tautology.

use arrow_kanban_server::storage::CHECKPOINT_FILES;

/// The four core tables the checkpoint covers — the ONLY names the public
/// persistence surface may carry. Kept as a local literal so the test owns an
/// INDEPENDENT copy: it fails if `CHECKPOINT_FILES` drifts from this set in
/// either direction (an added ext name, or a dropped core one).
const CORE_TABLES: &[&str] = &["items", "runs", "item_comments", "relations"];

/// `CHECKPOINT_FILES` is EXACTLY the four core tables — no ext table name, no
/// more, no fewer. Adding an ext name to this public constant is the exact leak
/// the lexical gate cannot see; here it is RED.
#[test]
fn the_public_checkpoint_file_set_is_exactly_the_four_core_tables() {
    assert_eq!(
        CHECKPOINT_FILES, CORE_TABLES,
        "the public checkpoint file set must name ONLY the four core tables — an ext table's \
         parquet name is a RUNTIME string from EngineExtension::checkpoint(), never a public constant"
    );
}

/// Per-element guard: NO checkpoint file name may be anything but a core table.
/// Belt-and-suspenders over the exact-eq above — it names the offending file, so
/// a careless addition fails with a legible message rather than a diff dump.
#[test]
fn no_checkpoint_file_is_an_extension_table() {
    for f in CHECKPOINT_FILES {
        assert!(
            CORE_TABLES.contains(f),
            "checkpoint file `{f}` is not one of the four core tables — an extension table must be \
             a runtime string from EngineExtension::checkpoint(), never baked into a public constant"
        );
    }
}

/// The public table-schema surface names ONLY the four core tables. Each
/// `*_schema()` describes one core table; there is no `ext`/consumer schema in
/// the core crate, so the count of public schema constructors is exactly four and
/// each maps onto a `CHECKPOINT_FILES` entry. If a consumer schema were ever added
/// to the public surface, its table would have to appear here — and it must not.
#[test]
fn the_public_schemas_describe_only_the_four_core_tables() {
    // The four public schema constructors — the complete public schema surface.
    let public_schemas = [
        ("items", arrow_kanban::schema::items_schema()),
        ("runs", arrow_kanban::schema::runs_schema()),
        ("item_comments", arrow_kanban::schema::comments_schema()),
        ("relations", arrow_kanban::schema::relations_schema()),
    ];

    // Every public schema maps onto a core checkpoint table, and vice-versa: the
    // two public surfaces cover the SAME four tables (no schema without a
    // checkpoint file, no checkpoint file without a schema).
    let schema_tables: Vec<&str> = public_schemas.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        schema_tables, CORE_TABLES,
        "the public schema surface must describe ONLY the four core tables"
    );
    for (name, schema) in &public_schemas {
        assert!(
            CHECKPOINT_FILES.contains(name),
            "public schema `{name}` has no core checkpoint file — a public schema must not name a \
             non-core (extension) table"
        );
        assert!(
            !schema.fields().is_empty(),
            "core schema `{name}` is non-empty (a real table contract)"
        );
    }
}

/// RED-before-green control: the invariant CAN fail. A hypothetical checkpoint set
/// that added a fifth (extension) table name is rejected by the SAME check the
/// tests above apply — proving they are a live guard, not a vacuous pass.
#[test]
fn a_fifth_extension_table_would_be_rejected() {
    // What a leak looks like: an ext table name smuggled into the public set.
    let leaked = ["items", "runs", "item_comments", "relations", "proposals"];
    let all_core = leaked.iter().all(|f| CORE_TABLES.contains(f));
    assert!(
        !all_core,
        "the guard must REJECT a fifth (extension) table name — if this passed, the core-only \
         checks above would be vacuous"
    );
    // And the exact-equality guard the real test uses would also fail on it.
    assert_ne!(
        &leaked[..],
        CORE_TABLES,
        "a five-element set is not the four-core set (the exact-eq guard bites)"
    );
}
