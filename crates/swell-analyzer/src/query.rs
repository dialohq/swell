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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferredColumn {
    pub name: String,
    pub oid: u32,
    pub nullable: bool,
    pub ts_type: String,
}
