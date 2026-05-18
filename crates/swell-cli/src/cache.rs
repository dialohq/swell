//! Content-addressed analyzer cache.
//!
//! Layout: `<cache_dir>/<sha256(sql)>.json`. Each file holds one
//! `CacheEntry` with the inferred query plus the schema fingerprint and
//! tool version that produced it. Online runs check the fingerprint and
//! re-analyse stale entries; offline (`swell check`) trusts the file as
//! committed.
//!
//! Layout matches SQLx's `.sqlx/` philosophy — the cache is committed so CI
//! can run offline.

use anyhow::{Context, Result};
use swell_analyzer::InferredQuery;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

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
    fs::create_dir_all(dir)
        .with_context(|| format!("create cache dir: {}", dir.display()))?;
    let path = path_for(dir, k);
    fs::write(&path, serde_json::to_string_pretty(entry)?)
        .with_context(|| format!("write cache file: {}", path.display()))?;
    Ok(())
}

pub fn list_keys(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<String> = entries.flatten()
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
                params: vec![InferredParam { oid: 23, ts_type: "number".into() }],
                columns: vec![InferredColumn {
                    name: "n".into(), oid: 23, nullable: false, ts_type: "number".into(),
                }],
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
