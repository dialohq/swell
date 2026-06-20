use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredQuery {
    pub sql: String,
    pub params: Vec<InferredParam>,
    pub columns: Vec<InferredColumn>,
    /// Row-level variants for queries that produce a discriminated
    /// union — FULL OUTER JOIN (three "matched left / right / both"
    /// shapes) and GROUPING SETS (one shape per grouping set, with
    /// un-grouped keys nullable). Each variant is a per-column
    /// TS-type override; columns not listed keep the type from
    /// `columns`. Codegen renders `row_variants` as a TS union.
    #[serde(default)]
    pub row_variants: Vec<RowVariant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowVariant {
    /// Column name → TS type for this variant. The final value
    /// substitutes whatever `render_typed` would have emitted for
    /// the base column.
    pub overrides: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredParam {
    pub oid: u32,
    pub ts_type: String,
    /// True iff the call site may pass `null` for this `$N`. False when
    /// at least one textual reference to `$N` binds *directly* to a
    /// NOT NULL column (INSERT VALUES target, UPDATE SET target). All
    /// other contexts (WHERE, function args, expression wrappers) stay
    /// nullable — passing null there is well-defined.
    #[serde(default = "default_nullable")]
    pub nullable: bool,
    /// Column this `$N` directly binds to. Set when `$N` is a direct
    /// child of INSERT VALUES at a known column position, or a UPDATE
    /// SET target's value. None otherwise (WHERE, expression wrappers,
    /// function call args). Used by codegen to emit `Table["col"]`
    /// instead of a raw type.
    #[serde(default)]
    pub table_ref: Option<TableColRef>,
}

fn default_nullable() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredColumn {
    pub name: String,
    pub oid: u32,
    pub nullable: bool,
    pub ts_type: String,
    /// Base-table column this result column directly references.
    /// Postgres's RowDescription reports `(table_oid, attnum)` for direct
    /// column refs; computed/aggregated/casted expressions report 0/0
    /// and leave this `None`. Codegen uses it to emit `Table["col"]`
    /// instead of duplicating the type literal.
    #[serde(default)]
    pub table_ref: Option<TableColRef>,
}

/// Reference back to a base-table column. Schema is the namespace
/// (`scheduler`, `public`, `app_private`, …); table is the unqualified
/// relation name; column is the attribute name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableColRef {
    pub schema: String,
    pub table: String,
    pub column: String,
}

/// Full schema of one referenced table, returned by
/// `Analyzer::table_schema`. Codegen emits one `interface Schema` per
/// unique (schema, table) seen across all queries — gives users a
/// reusable per-table type they can pull into application code as
/// `SchedulerCampaigns["id"]` etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    pub schema: String,
    pub table: String,
    pub columns: Vec<TableSchemaColumn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchemaColumn {
    pub name: String,
    pub oid: u32,
    pub ts_type: String,
    pub not_null: bool,
}
