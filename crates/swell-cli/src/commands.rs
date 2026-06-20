//! All four CLI subcommands: `gen`, `watch`, `check`, `prepare`.

use crate::cache;
use crate::config::{Config, OnError};
use anyhow::{anyhow, bail, Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use swell_analyzer::{Analyzer, AnalyzerOptions, InferredQuery, TableSchema};
use swell_codegen::{render, CodegenOptions};
use swell_scanner::{scan_file, ScanOptions, ScannedQuery};
use tracing::{info, warn};

#[derive(Clone, Copy)]
pub struct RunOpts {
    /// May this run touch the database?
    pub allow_db: bool,
    /// Should we delete cache files this run didn't reference?
    pub prune: bool,
    /// Should missing-from-cache queries be a hard error (CI gate)?
    pub require_cache: bool,
}

impl RunOpts {
    // `gen` and `prepare` share semantics — DB-on, prune-on, require-off.
    pub const GEN: Self = Self {
        allow_db: true,
        prune: true,
        require_cache: false,
    };
    pub const PREPARE: Self = Self::GEN;
    pub const CHECK: Self = Self {
        allow_db: false,
        prune: false,
        require_cache: true,
    };
}

pub struct RunSummary {
    pub hits: usize,
    pub errors: bool,
}

// ------------ swell gen / prepare / check ------------

pub async fn gen(cfg: &Config) -> Result<()> {
    run_pipeline(cfg, RunOpts::GEN).await.map(|_| ())
}

pub async fn prepare(cfg: &Config) -> Result<()> {
    info!("populating cache from live DB");
    run_pipeline(cfg, RunOpts::PREPARE).await.map(|_| ())
}

pub async fn check(cfg: &Config) -> Result<()> {
    info!("verifying cache covers every call site");
    let s = run_pipeline(cfg, RunOpts::CHECK).await?;
    if s.errors {
        bail!("some queries are missing from the cache. Run `swell prepare` against a live database to populate it.");
    }
    info!("ok: {} cached queries", s.hits);
    Ok(())
}

// ------------ swell watch ------------

const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

pub async fn watch(cfg: &Config) -> Result<()> {
    info!("initial gen");
    if let Err(e) = run_pipeline(cfg, RunOpts::GEN).await {
        warn!("initial gen failed: {e:#}");
    }

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("creating file watcher")?;
    watcher
        .watch(&cfg.root, RecursiveMode::Recursive)
        .context("watching project root")?;

    info!("watching {} (Ctrl-C to exit)", cfg.root.display());

    let mut last: Option<Instant> = None;
    let mut pending = false;
    loop {
        match rx.recv_timeout(WATCH_DEBOUNCE) {
            Ok(Ok(ev)) if is_relevant(&ev, cfg) => {
                last = Some(Instant::now());
                pending = true;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if pending && last.is_some_and(|t| t.elapsed() >= WATCH_DEBOUNCE) {
            pending = false;
            if let Err(e) = run_pipeline(cfg, RunOpts::GEN).await {
                warn!("regen failed: {e:#}");
            }
        }
    }
    Ok(())
}

fn is_relevant(ev: &Event, cfg: &Config) -> bool {
    if !matches!(
        ev.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    let cache_dir = cfg.cache.dir.to_string_lossy();
    let out_file = cfg.output.file.to_string_lossy();
    ev.paths.iter().any(|p| {
        let s = p.to_string_lossy();
        if s.contains(&*cache_dir) || s.ends_with(&*out_file) {
            return false;
        }
        matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("ts" | "tsx" | "sql" | "toml")
        )
    })
}

// ------------ shared pipeline ------------

async fn run_pipeline(cfg: &Config, opts: RunOpts) -> Result<RunSummary> {
    info!("scanning project at {}", cfg.root.display());

    let scanned = scan_project(cfg)?;
    info!("found {} sql() call sites", scanned.len());

    let mut unique: BTreeMap<String, &ScannedQuery> = BTreeMap::new();
    for q in &scanned {
        let k = q.static_parts.first().cloned().unwrap_or_default();
        unique.entry(k).or_insert(q);
    }
    info!("{} unique queries", unique.len());

    let cache_dir = cfg.root.join(&cfg.cache.dir);
    let analyzer = maybe_connect(cfg, opts).await?;

    let schema_fp = match &analyzer {
        Some(an) => an
            .schema_fingerprint(&cfg.database.schemas)
            .await
            .unwrap_or_default(),
        None => String::new(),
    };

    let mut inferred: Vec<InferredQuery> = Vec::with_capacity(unique.len());
    let mut seen: Vec<String> = Vec::with_capacity(unique.len());
    let mut hits = 0usize;
    let mut misses = 0usize;
    let mut errors = false;

    for (sql, scan) in &unique {
        let key = cache::key(sql);
        seen.push(key.clone());

        let cached = cache::read(&cache_dir, &key);
        let stale = cached.as_ref().is_some_and(|e| {
            e.tool_version != cache::TOOL_VERSION
                || (analyzer.is_some() && e.schema_fingerprint != schema_fp)
        });

        if let Some(entry) = cached.filter(|_| !stale) {
            hits += 1;
            inferred.push(entry.query);
            continue;
        }

        let Some(an) = analyzer.as_ref() else {
            errors = true;
            warn!(
                "offline mode: query at {}:{}:{} not found in cache",
                scan.file, scan.line, scan.col
            );
            continue;
        };

        match an.analyze(sql).await {
            Ok(q) => {
                misses += 1;
                let entry = cache::CacheEntry {
                    schema_fingerprint: schema_fp.clone(),
                    tool_version: cache::TOOL_VERSION.to_string(),
                    query: q.clone(),
                };
                if let Err(e) = cache::write(&cache_dir, &key, &entry) {
                    warn!("could not write cache: {e:#}");
                }
                inferred.push(q);
            }
            Err(e) => {
                errors = true;
                warn!("query at {}:{}:{}: {e:#}", scan.file, scan.line, scan.col);
            }
        }
    }

    if opts.prune {
        let referenced: std::collections::HashSet<&str> = seen.iter().map(String::as_str).collect();
        for k in cache::list_keys(&cache_dir) {
            if !referenced.contains(k.as_str()) {
                let p = cache::path_for(&cache_dir, &k);
                if let Err(e) = std::fs::remove_file(&p) {
                    warn!("could not prune {}: {e}", p.display());
                }
            }
        }
    }

    let extra_imports: Vec<(String, Vec<String>)> = cfg
        .output
        .imports
        .iter()
        .map(|i| (i.from.clone(), i.names.clone()))
        .collect();

    // Base tables referenced by any column / param. Offline: skipped
    // (codegen falls back to inline types).
    let tables = match analyzer.as_ref() {
        Some(an) => fetch_referenced_tables(an, &inferred).await,
        None => Vec::new(),
    };

    let dts = render(
        &inferred,
        CodegenOptions {
            extra_imports: &extra_imports,
            tables: &tables,
        },
    );
    let out_path = cfg.root.join(&cfg.output.file);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir: {}", parent.display()))?;
    }
    write_if_changed(&out_path, &dts)?;
    info!(
        "wrote {} ({} entries; {hits} hits, {misses} misses)",
        out_path.display(),
        inferred.len()
    );

    if errors && cfg.diagnostics.on_error == OnError::Fail {
        return Err(anyhow!(
            "one or more queries failed analysis (diagnostics.on_error = fail)"
        ));
    }
    Ok(RunSummary { hits, errors })
}

async fn maybe_connect(cfg: &Config, opts: RunOpts) -> Result<Option<Analyzer>> {
    if !opts.allow_db || cfg.database.url.is_none() {
        return Ok(None);
    }
    let type_overrides = cfg
        .types
        .by_name
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                swell_analyzer::TypeOverride {
                    parse: v.parse().to_string(),
                    serialize: v.serialize().to_string(),
                },
            )
        })
        .collect();
    let res = Analyzer::connect(AnalyzerOptions {
        database_url: cfg.database.url.clone().unwrap(),
        schemas: cfg.database.schemas.clone(),
        type_overrides,
    })
    .await;
    match res {
        Ok(a) => Ok(Some(a)),
        Err(e) if !opts.require_cache => {
            warn!("could not connect to Postgres: {e:#}; trying offline cache");
            Ok(None)
        }
        Err(e) => Err(e).context("connecting to dev Postgres"),
    }
}

/// Distinct `(schema, table)` pairs referenced by any analysed query
/// → `TableSchema`s in one round trip. Missing tables are skipped.
async fn fetch_referenced_tables(an: &Analyzer, queries: &[InferredQuery]) -> Vec<TableSchema> {
    let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for q in queries {
        for c in &q.columns {
            if let Some(r) = &c.table_ref {
                pairs.insert((r.schema.clone(), r.table.clone()));
            }
        }
        for p in &q.params {
            if let Some(r) = &p.table_ref {
                pairs.insert((r.schema.clone(), r.table.clone()));
            }
        }
    }
    let pairs_vec: Vec<(String, String)> = pairs.into_iter().collect();
    match an.table_schemas(&pairs_vec).await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("table_schemas failed: {e:#}");
            Vec::new()
        }
    }
}

fn scan_project(cfg: &Config) -> Result<Vec<ScannedQuery>> {
    let include = build_globset(&cfg.scan.include)?;
    let exclude = build_globset(&cfg.scan.exclude)?;
    let q_refs: Vec<&str> = cfg.scan.q_modules.iter().map(String::as_str).collect();
    let opts = ScanOptions { q_modules: &q_refs };

    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&cfg.root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let rel = abs.strip_prefix(&cfg.root).unwrap_or(abs);
        if exclude.is_match(rel) || !include.is_match(rel) {
            continue;
        }

        let src = match std::fs::read_to_string(abs) {
            Ok(s) => s,
            Err(e) => {
                warn!("could not read {}: {e}", abs.display());
                continue;
            }
        };
        match scan_file(abs, &src, opts.clone()) {
            Ok(qs) => out.extend(qs),
            Err(e) => warn!("scan error in {}: {e}", abs.display()),
        }
    }
    Ok(out)
}

fn is_hidden(e: &walkdir::DirEntry) -> bool {
    e.file_name()
        .to_str()
        .map(|s| s.starts_with('.') && s != "." && s != ".." && s != ".swell")
        .unwrap_or(false)
}

fn build_globset(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut b = globset::GlobSetBuilder::new();
    for p in patterns {
        b.add(globset::Glob::new(p).with_context(|| format!("bad glob: {p}"))?);
    }
    Ok(b.build()?)
}

fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    std::fs::write(path, contents).with_context(|| format!("write: {}", path.display()))
}
