use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredQuery {
    pub sql: String,
    pub params: Vec<InferredParam>,
    pub columns: Vec<InferredColumn>,
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
    /// Cross-column row-level refinement from `CHECK` constraints
    /// (e.g. `num_nonnulls(a, b) = 1`, CASE-keyed unions). When
    /// non-empty, codegen renders the table type as
    /// `Base & (variant | variant | …)` rather than a flat interface.
    #[serde(default)]
    pub row_variants: Vec<TableRowVariant>,
}

/// One variant in a row-level union. Each entry overrides a column's
/// TS type within this variant; columns not listed keep their base
/// type from `columns`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRowVariant {
    pub columns: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchemaColumn {
    pub name: String,
    pub oid: u32,
    pub ts_type: String,
    pub not_null: bool,
}
