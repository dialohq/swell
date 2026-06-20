//! Content-addressed analyzer cache: `<cache_dir>/<sha256(sql)>.json`.
//! Online runs check `schema_fingerprint` + `tool_version` and
//! re-analyse stale entries; `swell check` trusts the file as
//! committed (CI / offline mode, à la SQLx's `.sqlx/`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use swell_analyzer::InferredQuery;

pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub schema_fingerprint: String,
    pub tool_version: String,
    pub query: InferredQuery,
}

/// Stable key for a query — content-addresses by SQL text only.
pub fn key(sql: &str) -> String {
    let mut h = Sha256::new();
    h.update(sql.as_bytes());
    hex::encode(h.finalize())
}

pub fn path_for(dir: &Path, k: &str) -> PathBuf {
    dir.join(format!("{}.json", k))
}

pub fn read(dir: &Path, k: &str) -> Option<CacheEntry> {
    let text = fs::read_to_string(path_for(dir, k)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write(dir: &Path, k: &str, entry: &CacheEntry) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create cache dir: {}", dir.display()))?;
    let path = path_for(dir, k);
    fs::write(&path, serde_json::to_string_pretty(entry)?)
        .with_context(|| format!("write cache file: {}", path.display()))?;
    Ok(())
}

pub fn list_keys(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|s| s.to_str()) == Some("json"))
                .then(|| p.file_stem()?.to_str().map(String::from))?
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use swell_analyzer::{InferredColumn, InferredParam};
    use tempfile::tempdir;

    fn entry() -> CacheEntry {
        CacheEntry {
            schema_fingerprint: "fp".into(),
            tool_version: TOOL_VERSION.into(),
            query: InferredQuery {
                sql: "SELECT 1".into(),
                params: vec![InferredParam {
                    ts_type: "number".into(),
                    nullable: true,
                    table_ref: None,
                }],
                columns: vec![InferredColumn {
                    name: "n".into(),
                    nullable: false,
                    ts_type: "number".into(),
                    table_ref: None,
                }],
                row_variants: Vec::new(),
            },
        }
    }

    #[test]
    fn key_is_stable_and_sql_only() {
        assert_eq!(key("SELECT 1"), key("SELECT 1"));
        assert_ne!(key("SELECT 1"), key("SELECT 2"));
    }

    #[test]
    fn round_trip() {
        let dir = tempdir().unwrap();
        let e = entry();
        let k = key(&e.query.sql);
        write(dir.path(), &k, &e).unwrap();
        assert_eq!(read(dir.path(), &k).unwrap().query.sql, e.query.sql);
    }
}
