use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredQuery {
    pub sql: String,
    pub params: Vec<InferredParam>,
    pub columns: Vec<InferredColumn>,
    /// Row-level variants for FULL OUTER JOIN (left-only, right-only,
    /// both) and GROUPING SETS (one per set). Each variant is a
    /// per-column TS override; codegen renders the list as a union.
    #[serde(default)]
    pub row_variants: Vec<RowVariant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowVariant {
    pub overrides: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredParam {
    pub ts_type: String,
    /// True iff the call site may pass `null`. Tightened to false when
    /// `$N` directly binds to a NOT NULL column (INSERT VALUES /
    /// UPDATE SET target).
    #[serde(default = "default_nullable")]
    pub nullable: bool,
    /// Set when `$N` is a direct child of INSERT VALUES / UPDATE SET —
    /// codegen emits `Table["col"]` instead of a raw type.
    #[serde(default)]
    pub table_ref: Option<TableColRef>,
}

fn default_nullable() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredColumn {
    pub name: String,
    pub nullable: bool,
    pub ts_type: String,
    /// `(schema, table, column)` for direct base-column refs.
    /// Computed / aggregated / cast results leave this `None`.
    #[serde(default)]
    pub table_ref: Option<TableColRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableColRef {
    pub schema: String,
    pub table: String,
    pub column: String,
}

/// One referenced table's full schema, used by codegen to emit
/// `interface SchemaTable`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    pub schema: String,
    pub table: String,
    pub columns: Vec<TableSchemaColumn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchemaColumn {
    pub name: String,
    pub ts_type: String,
    pub not_null: bool,
}
