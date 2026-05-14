use serde::{Deserialize, Serialize};

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
}

fn default_nullable() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredColumn {
    pub name: String,
    pub oid: u32,
    pub nullable: bool,
    pub ts_type: String,
}
