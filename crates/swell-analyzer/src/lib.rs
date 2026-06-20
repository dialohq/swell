//! Query analysis pipeline.
//!
//! `Analyzer` owns one `tokio_postgres::Client` and a cached `TypeCatalog`.
//! Each call to `analyze` runs PARSE + DESCRIBE for the supplied SQL, then
//! enriches the result with `pg_attribute.attnotnull`, `EXPLAIN VERBOSE`
//! join nullability, and JSON shape inference.

pub mod describe;
pub mod catalog;
pub mod nullability;
pub mod param_nullability;
pub mod json_shape;
pub mod overrides;
pub mod ts_types;
pub mod query;

pub use query::{
    InferredColumn, InferredParam, InferredQuery, TableColRef, TableSchema, TableSchemaColumn,
};
pub use ts_types::{Direction, TypeCatalog, TypeOverride};

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use tokio_postgres::{Client, Config, NoTls};

pub struct Analyzer {
    pub client: Client,
    pub catalog: TypeCatalog,
}

pub struct AnalyzerOptions {
    pub database_url: String,
    pub schemas: Vec<String>,
    pub type_overrides: BTreeMap<String, ts_types::TypeOverride>,
}

impl Analyzer {
    /// Connect to Postgres and load the type catalog.
    ///
    /// Pins `plan_cache_mode = force_generic_plan` on the session so the
    /// EXPLAIN plans we'll inspect for nullability match what PARSE/DESCRIBE
    /// produces.
    pub async fn connect(opts: AnalyzerOptions) -> Result<Self> {
        let mut cfg: Config = opts.database_url.parse()
            .with_context(|| format!("invalid DATABASE_URL: {}", opts.database_url))?;
        cfg.options("-c plan_cache_mode=force_generic_plan");

        let (client, connection) = cfg.connect(NoTls).await
            .context("connecting to Postgres")?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!("postgres connection error: {e}");
            }
        });

        let mut catalog = catalog::load_type_catalog(&client, &opts.schemas).await
            .context("loading pg_catalog")?;
        catalog.by_name = opts.type_overrides;

        Ok(Self { client, catalog })
    }

    /// Cheap fingerprint over the relevant schemas — invalidates caches.
    pub async fn schema_fingerprint(&self, schemas: &[String]) -> Result<String> {
        catalog::schema_fingerprint(&self.client, schemas).await
    }

    /// Run PARSE + DESCRIBE + nullability + JSON shape inference for the
    /// query, returning a fully-typed `InferredQuery`.
    pub async fn analyze(&self, sql: &str) -> Result<InferredQuery> {
        let described = describe::describe(&self.client, sql).await?;

        let pairs: Vec<(u32, i16)> = described.columns.iter()
            .filter(|c| c.table_oid != 0 && c.attnum > 0)
            .map(|c| (c.table_oid, c.attnum))
            .collect();
        // One round trip resolves both `attnotnull` and the
        // `(schema, table, column)` triple for every referenced base
        // column — the two used to be separate queries.
        let column_meta = resolve_column_meta(&self.client, &pairs).await;
        let attnotnull: std::collections::HashMap<(u32, i16), bool> = column_meta
            .iter().map(|(k, v)| (*k, v.not_null)).collect();

        let null_hints = nullability::explain_nullability(
            &self.client, sql, &described.params, described.columns.len(),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::debug!("EXPLAIN failed for `{sql}`: {e}");
            nullability::NullabilityHints::unknown(described.columns.len())
        });

        // Extra verdict refinement: a `COALESCE(...)` whose args include any
        // NOT-NULL base column is non-null. `classify` already handles the
        // trailing-literal case; this handles `COALESCE(nullable_col,
        // not_null_col)` by looking up attnotnull for each `<alias>.<col>`
        // arg using the plan's scan map.
        let coalesce_refined = refine_coalesce_non_null(
            &self.client,
            &null_hints,
        ).await;

        let json_shapes = json_shape::infer_shapes(
            &self.client, &self.catalog, sql, described.columns.len(),
        ).await;

        let param_info = param_nullability::infer(
            &self.client, sql, described.params.len(),
        ).await;

        let params = described.params.iter().enumerate()
            .map(|(i, t)| {
                let info = param_info.get(i).cloned().unwrap_or_default();
                InferredParam {
                    oid: t.oid(),
                    ts_type: catalog::render_for_oid(&self.catalog, t.oid(), t, Direction::Write),
                    nullable: info.nullable,
                    table_ref: info.table_ref,
                }
            })
            .collect();

        let columns = described.columns.iter().enumerate()
            .map(|(i, c)| {
                let verdict = if coalesce_refined.get(i).copied().unwrap_or(false) {
                    nullability::NullVerdict::NotNullable
                } else {
                    null_hints.by_column.get(i).copied()
                        .unwrap_or(nullability::NullVerdict::Unknown)
                };
                let inferred_nullable = decide_nullability(c, &attnotnull, verdict);
                let oid_ts = catalog::render_for_oid(&self.catalog, c.type_.oid(), &c.type_, Direction::Read);
                let json_ts = json_shapes.by_target.get(i).cloned().flatten();
                let inferred_ts = json_ts.unwrap_or(oid_ts);

                let ov = overrides::parse(&c.name);
                let table_ref = column_meta.get(&(c.table_oid, c.attnum))
                    .map(|m| m.table_ref.clone());
                InferredColumn {
                    name: ov.clean_name,
                    oid: c.type_.oid(),
                    nullable: ov.force_nullable.unwrap_or(inferred_nullable),
                    ts_type: inferred_ts,
                    table_ref,
                }
            })
            .collect();

        Ok(InferredQuery { sql: sql.to_string(), params, columns })
    }

    /// Fetch the full column list for every requested `(schema, table)`
    /// in one round trip. Codegen passes the distinct base tables every
    /// analysed query referenced; we return one `TableSchema` per pair
    /// that actually resolves (dropped / missing tables are skipped
    /// silently — caller falls back to inlining types).
    pub async fn table_schemas(
        &self, pairs: &[(String, String)],
    ) -> Result<Vec<TableSchema>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let schemas: Vec<&str> = pairs.iter().map(|(s, _)| s.as_str()).collect();
        let tables:  Vec<&str> = pairs.iter().map(|(_, t)| t.as_str()).collect();
        let rows = self.client.query(
            r#"
            WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
            SELECT n.nspname, c.relname, a.attname, a.atttypid::bigint, t.typname,
                   a.attnotnull, a.attnum
            FROM ask
            JOIN pg_namespace n ON n.nspname = ask.schema
            JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
            JOIN pg_attribute a ON a.attrelid = c.oid
            JOIN pg_type t      ON t.oid = a.atttypid
            WHERE a.attnum > 0 AND NOT a.attisdropped
            ORDER BY n.nspname, c.relname, a.attnum
            "#,
            &[&schemas, &tables],
        ).await?;
        let mut grouped: std::collections::BTreeMap<(String, String), Vec<TableSchemaColumn>> =
            std::collections::BTreeMap::new();
        for row in &rows {
            let schema: String = row.get(0);
            let table:  String = row.get(1);
            let name:   String = row.get(2);
            let oid:    i64    = row.get(3);
            let typname: String = row.get(4);
            let not_null: bool = row.get(5);
            grouped.entry((schema, table)).or_default().push(TableSchemaColumn {
                name,
                oid: oid as u32,
                ts_type: self.catalog.render_oid(oid as u32, &typname, Direction::Read),
                not_null,
            });
        }
        Ok(grouped.into_iter()
            .map(|((schema, table), columns)| TableSchema { schema, table, columns })
            .collect())
    }
}

/// Per-(table_oid, attnum) result of the one-shot column-metadata
/// lookup: the originating `(schema, table, column)` triple plus the
/// base column's `attnotnull` bit. Used by `analyze` to fill both
/// `InferredColumn.table_ref` and the join-nullability verdict from
/// `decide_nullability`.
struct ColumnMeta {
    table_ref: TableColRef,
    not_null: bool,
}

/// Resolve `(table_oid, attnum)` → `ColumnMeta` in one round trip,
/// fusing what used to be separate `fetch_attnotnull` and
/// `resolve_column_refs` queries.
async fn resolve_column_meta(
    client: &Client,
    pairs: &[(u32, i16)],
) -> HashMap<(u32, i16), ColumnMeta> {
    if pairs.is_empty() {
        return HashMap::new();
    }
    let mut unique: std::collections::HashSet<(u32, i16)> = std::collections::HashSet::new();
    for p in pairs { unique.insert(*p); }
    let mut tables = Vec::with_capacity(unique.len());
    let mut attnums = Vec::with_capacity(unique.len());
    for (t, a) in &unique {
        tables.push(*t as i64);
        attnums.push(*a as i32);
    }
    let rows = match client.query(
        r#"
        WITH ask(t, a) AS (SELECT * FROM unnest($1::bigint[], $2::int[]))
        SELECT n.nspname, c.relname, att.attname, ask.t, ask.a, att.attnotnull
        FROM ask
        JOIN pg_attribute att ON att.attrelid::bigint = ask.t AND att.attnum = ask.a::smallint
        JOIN pg_class c       ON c.oid = att.attrelid
        JOIN pg_namespace n   ON n.oid = c.relnamespace
        WHERE att.attnum > 0 AND NOT att.attisdropped
        "#,
        &[&tables, &attnums],
    ).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("resolve_column_meta: {e}");
            return HashMap::new();
        }
    };
    let mut out = HashMap::with_capacity(rows.len());
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let column: String = row.get(2);
        let t: i64 = row.get(3);
        let a: i32 = row.get(4);
        let not_null: bool = row.get(5);
        out.insert((t as u32, a as i16), ColumnMeta {
            table_ref: TableColRef { schema, table, column },
            not_null,
        });
    }
    out
}

/// For each output column whose EXPLAIN expression is `COALESCE(...)`,
/// check whether any arg is a NOT NULL base column reference and return
/// `true` for that column index. The caller upgrades the column's
/// verdict to `NotNullable` when this returns true.
///
/// `classify` already handles the trailing-literal case (e.g.
/// `coalesce(x, 'lit')`); this picks up the cases where the non-null
/// guarantor is a NOT NULL column.
async fn refine_coalesce_non_null(
    client: &Client,
    hints: &nullability::NullabilityHints,
) -> Vec<bool> {
    use std::collections::{HashMap, HashSet};
    let mut out = vec![false; hints.exprs.len()];

    // Collect every (schema, table) we need to know attnotnull for.
    // Args may be `alias.col` or bare `col` (PG omits the alias when
    // there's only one relation in scope). For the bare case we have
    // to query the catalog for every scan table and figure out which
    // one owns the column.
    let mut needed: HashSet<(String, String)> = HashSet::new();
    let mut per_column_args: Vec<Vec<String>> = vec![Vec::new(); hints.exprs.len()];
    for (i, expr) in hints.exprs.iter().enumerate() {
        let args = match coalesce_args(expr) {
            Some(args) => args,
            None => continue,
        };
        per_column_args[i] = args.clone();
        for arg in &args {
            match parse_column_ref(arg) {
                ColumnRefShape::Qualified(alias, _) => {
                    if let Some(table) = hints.alias_to_table.get(&alias) {
                        needed.insert((table.0.clone(), table.1.clone()));
                    }
                }
                ColumnRefShape::Bare(_) => {
                    // Bare column — include every scan table; the lookup
                    // disambiguates by which table actually has that column.
                    for table in hints.alias_to_table.values() {
                        needed.insert((table.0.clone(), table.1.clone()));
                    }
                }
                ColumnRefShape::None => {}
            }
        }
    }
    if needed.is_empty() {
        // No coalesce args reference a base column; only the literal
        // path matters and `classify` already handled that.
        // Still need to scan per-column args for pure literals (covered
        // by classify, but be defensive).
        for (i, args) in per_column_args.iter().enumerate() {
            for arg in args {
                if is_literal_token(arg) {
                    out[i] = true;
                    break;
                }
            }
        }
        return out;
    }

    // Bulk-query attnotnull for every (schema, table) once.
    let schemas: Vec<String> = needed.iter().map(|p| p.0.clone()).collect();
    let tables: Vec<String> = needed.iter().map(|p| p.1.clone()).collect();
    let mut attnotnull: HashMap<(String, String, String), bool> = HashMap::new();
    let rows_res = client.query(
        r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT n.nspname::text, c.relname::text, a.attname::text, a.attnotnull
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
        JOIN pg_attribute a ON a.attrelid = c.oid
        WHERE a.attnum > 0 AND NOT a.attisdropped
        "#,
        &[&schemas, &tables],
    ).await;
    if let Ok(rows) = rows_res {
        for row in &rows {
            attnotnull.insert(
                (row.get(0), row.get(1), row.get(2)),
                row.get::<_, bool>(3),
            );
        }
    }

    for (i, args) in per_column_args.iter().enumerate() {
        for arg in args {
            if is_literal_token(arg) {
                out[i] = true;
                break;
            }
            let resolved = match parse_column_ref(arg) {
                ColumnRefShape::Qualified(alias, col) => hints.alias_to_table.get(&alias)
                    .map(|(s, t)| (s.clone(), t.clone(), col)),
                ColumnRefShape::Bare(col) => {
                    // Pick the unique table that has a column by this name;
                    // if multiple tables have it, leave as unknown.
                    let mut hit: Option<(String, String, String)> = None;
                    let mut ambiguous = false;
                    for (schema, table) in hints.alias_to_table.values() {
                        let k = (schema.clone(), table.clone(), col.clone());
                        if attnotnull.contains_key(&k) {
                            if hit.is_some() { ambiguous = true; break; }
                            hit = Some(k);
                        }
                    }
                    if ambiguous { None } else { hit }
                }
                ColumnRefShape::None => None,
            };
            if let Some(k) = resolved {
                if attnotnull.get(&k).copied().unwrap_or(false) {
                    out[i] = true;
                    break;
                }
            }
        }
    }
    out
}

enum ColumnRefShape {
    Qualified(String, String),
    Bare(String),
    None,
}

/// Return the args of a top-level `coalesce(...)` expression in
/// EXPLAIN-VERBOSE form. EXPLAIN sometimes wraps the whole expression
/// in extra parens (`(coalesce(a, b))`); we step inward through
/// balanced parens until we find the `coalesce` head.
fn coalesce_args(expr: &str) -> Option<Vec<String>> {
    let mut s = expr.trim();
    // Peel one balanced `(...)` wrapper at a time.
    loop {
        if !s.starts_with('(') || !s.ends_with(')') { break; }
        if !is_balanced_paren_wrapper(s) { break; }
        s = &s[1..s.len() - 1];
        s = s.trim();
    }
    let lower = s.to_ascii_lowercase();
    if !lower.starts_with("coalesce(") {
        return None;
    }
    let body_start = "coalesce(".len();
    let bytes = s.as_bytes();
    let mut depth = 1;
    let mut i = body_start;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' { i += 2; continue; }
                in_string = false;
            }
        } else {
            match b {
                b'\'' => in_string = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 { break; }
                }
                _ => {}
            }
        }
        i += 1;
    }
    if depth != 0 || i >= bytes.len() {
        return None;
    }
    Some(split_top_level_args(&s[body_start..i]))
}

/// True iff the leading `(` of `s` is closed by the very last `)` of
/// `s` — i.e. the whole string is wrapped in one balanced pair.
fn is_balanced_paren_wrapper(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes[0] != b'(' || *bytes.last().unwrap() != b')' {
        return false;
    }
    let mut depth = 0;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' { i += 2; continue; }
                in_string = false;
            }
        } else {
            match b {
                b'\'' => in_string = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 && i != bytes.len() - 1 {
                        return false;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    depth == 0
}

fn split_top_level_args(body: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let bytes = body.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            cur.push(b as char);
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    cur.push('\''); i += 2; continue;
                }
                in_string = false;
            }
        } else {
            match b {
                b'\'' => { in_string = true; cur.push('\''); }
                b'(' => { depth += 1; cur.push('('); }
                b')' => { depth -= 1; cur.push(')'); }
                b',' if depth == 0 => {
                    args.push(cur.trim().to_string());
                    cur.clear();
                }
                c => cur.push(c as char),
            }
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        args.push(cur.trim().to_string());
    }
    args
}

/// Recognise a non-null literal token: quoted string, signed number,
/// or `true` / `false`. Peels off a trailing `::cast` if any.
fn is_literal_token(arg: &str) -> bool {
    let s = arg.split("::").next().unwrap_or(arg).trim();
    if s.is_empty() { return false; }
    if s.starts_with('\'') { return true; }
    let first = s.chars().next().unwrap();
    if first == '-' || first == '+' || first.is_ascii_digit() {
        return s.chars().skip(1).all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-');
    }
    matches!(s.to_ascii_lowercase().as_str(), "true" | "false")
}

/// Parse a column reference, peeling off any `::cast` and bracketing
/// parens. Returns whether it's a qualified `<alias>.<col>` or a bare
/// `<col>` reference (PG omits the alias in EXPLAIN when there's only
/// one relation in scope).
fn parse_column_ref(arg: &str) -> ColumnRefShape {
    let s = arg.split("::").next().unwrap_or(arg).trim();
    let s = s.trim_start_matches('(').trim_end_matches(')').trim();
    let is_ident = |s: &str| {
        !s.is_empty()
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !s.chars().next().unwrap().is_ascii_digit()
    };
    if let Some(dot) = s.find('.') {
        let alias = &s[..dot];
        let col = &s[dot + 1..];
        if is_ident(alias) && is_ident(col) {
            return ColumnRefShape::Qualified(alias.to_string(), col.to_string());
        }
        return ColumnRefShape::None;
    }
    if is_ident(s) {
        ColumnRefShape::Bare(s.to_string())
    } else {
        ColumnRefShape::None
    }
}

/// Combine attnotnull and EXPLAIN evidence into a final nullable verdict.
///
/// | base table col? | attnotnull | EXPLAIN          | nullable |
/// |-----------------|------------|------------------|----------|
/// | yes             | NOT NULL   | Nullable         | yes (outer-join trumps) |
/// | yes             | NOT NULL   | otherwise        | no       |
/// | yes             | nullable   | *                | yes      |
/// | no              | n/a        | NotNullable      | no       |
/// | no              | n/a        | otherwise        | yes      |
fn decide_nullability(
    c: &describe::DescribedColumn,
    attnotnull: &std::collections::HashMap<(u32, i16), bool>,
    explain: nullability::NullVerdict,
) -> bool {
    use nullability::NullVerdict::*;
    if c.table_oid != 0 && c.attnum > 0 {
        let base_not_null = attnotnull.get(&(c.table_oid, c.attnum)).copied().unwrap_or(false);
        match (base_not_null, explain) {
            (true, Nullable) => true,
            (true, _)        => false,
            (false, _)       => true,
        }
    } else {
        !matches!(explain, NotNullable)
    }
}
