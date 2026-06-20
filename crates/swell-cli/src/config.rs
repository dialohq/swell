use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub database: Database,
    #[serde(default)]
    pub scan: Scan,
    #[serde(default)]
    pub output: Output,
    #[serde(default)]
    pub cache: Cache,
    #[serde(default)]
    pub diagnostics: Diagnostics,
    #[serde(default)]
    pub types: Types,

    /// Project root, set after load. Not in the TOML.
    #[serde(skip)]
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Database {
    /// Postgres URL. Falls back to $DATABASE_URL if absent.
    pub url: Option<String>,
    #[serde(default = "default_schemas")]
    pub schemas: Vec<String>,
}

fn default_schemas() -> Vec<String> { vec!["public".into()] }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scan {
    #[serde(default = "default_include")]
    pub include: Vec<String>,
    #[serde(default = "default_exclude")]
    pub exclude: Vec<String>,
    /// Modules (besides `@dialo/swell`) that re-export `q`. Defaults
    /// cover the per-package codegen output.
    #[serde(default = "default_q_modules")]
    pub q_modules: Vec<String>,
}

impl Default for Scan {
    fn default() -> Self {
        Self {
            include: default_include(),
            exclude: default_exclude(),
            q_modules: default_q_modules(),
        }
    }
}

fn default_include() -> Vec<String> { vec!["src/**/*.ts".into(), "src/**/*.tsx".into()] }
fn default_exclude() -> Vec<String> { vec!["**/*.test.ts".into(), "node_modules/**".into()] }
fn default_q_modules() -> Vec<String> {
    vec![
        "./swell.generated".into(),
        "../swell.generated".into(),
        "../../swell.generated".into(),
        "../../../swell.generated".into(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    #[serde(default = "default_output_file")]
    pub file: PathBuf,
    #[serde(default = "default_pretty")]
    pub pretty: bool,
    /// Extra `import type { ... }` lines injected at the top of the
    /// generated file (for per-column overrides that reference
    /// project-local types).
    #[serde(default)]
    pub imports: Vec<ImportSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSpec {
    pub from: String,
    pub names: Vec<String>,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            file: default_output_file(),
            pretty: true,
            imports: Vec::new(),
        }
    }
}

fn default_output_file() -> PathBuf { PathBuf::from("src/swell.generated.ts") }
fn default_pretty() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default = "default_cache_dir")]
    pub dir: PathBuf,
}

impl Default for Cache {
    fn default() -> Self { Self { dir: default_cache_dir() } }
}

fn default_cache_dir() -> PathBuf { PathBuf::from(".swell") }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostics {
    #[serde(default = "default_on_error")]
    pub on_error: OnError,
}

impl Default for Diagnostics {
    fn default() -> Self { Self { on_error: default_on_error() } }
}

fn default_on_error() -> OnError { OnError::Fail }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OnError { Skip, Fail }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Types {
    /// Per-typname overrides. Each entry is either a bare string (both
    /// parse + serialize) or `{ parse, serialize }` for drivers like
    /// node-pg where the two directions diverge.
    #[serde(default)]
    pub by_name: std::collections::BTreeMap<String, TypeOverride>,
    #[serde(default)]
    pub column: Vec<ColumnOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TypeOverride {
    Both(String),
    Split { parse: String, serialize: String },
}

impl TypeOverride {
    pub fn parse(&self) -> &str { match self { Self::Both(s) | Self::Split { parse: s, .. } => s } }
    pub fn serialize(&self) -> &str { match self { Self::Both(s) | Self::Split { serialize: s, .. } => s } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnOverride {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub ts: String,
}

pub fn load(path: &Path) -> Result<Config> {
    let path = path
        .canonicalize()
        .with_context(|| format!("config file not found: {}", path.display()))?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read config: {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&text)
        .with_context(|| format!("parse config: {}", path.display()))?;
    cfg.root = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    // Env-var fallback for the database URL.
    if cfg.database.url.is_none() {
        if let Ok(env_url) = std::env::var("DATABASE_URL") {
            cfg.database.url = Some(env_url);
        }
    }
    Ok(cfg)
}
