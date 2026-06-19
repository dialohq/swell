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

        // Resolve `attnotnull` for any base-column refs the plan walker
        // recovered from strict expressions (e.g. `(<col>)::<type>`
        // casts) where `RowDescription` dropped (table_oid, attnum).
        // Without this lookup, casts over NOT NULL columns get inferred
        // as nullable (issue #21).
        let base_attnotnull = resolve_base_ref_attnotnull(
            &self.client,
            &null_hints.base_refs,
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
                let base_ref_not_null = null_hints.base_refs
                    .get(i)
                    .and_then(|r| r.as_ref())
                    .and_then(|r| base_attnotnull.get(r).copied())
                    .unwrap_or(false);
                let inferred_nullable = decide_nullability(
                    c, &attnotnull, null_hints.by_column.get(i).copied()
                        .unwrap_or(nullability::NullVerdict::Unknown),
                    base_ref_not_null,
                );
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
                    ts_type: ov.force_ts_type.unwrap_or(inferred_ts),
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

/// Combine attnotnull and EXPLAIN evidence into a final nullable verdict.
///
/// | base table col? | attnotnull | EXPLAIN     | base ref NN | nullable |
/// |-----------------|------------|-------------|-------------|----------|
/// | yes             | NOT NULL   | Nullable    | *           | yes (outer-join trumps) |
/// | yes             | NOT NULL   | otherwise   | *           | no       |
/// | yes             | nullable   | *           | *           | yes      |
/// | no              | n/a        | NotNullable | *           | no       |
/// | no              | n/a        | Nullable    | *           | yes      |
/// | no              | n/a        | Unknown     | true        | no       |
/// | no              | n/a        | Unknown     | false       | yes      |
///
/// `base_ref_not_null` carries `attnotnull` for a base column recovered
/// from a strict expression (e.g. `(<col>)::<type>` cast) when
/// `RowDescription` dropped the direct (table_oid, attnum) link.
fn decide_nullability(
    c: &describe::DescribedColumn,
    attnotnull: &std::collections::HashMap<(u32, i16), bool>,
    explain: nullability::NullVerdict,
    base_ref_not_null: bool,
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
        match explain {
            NotNullable => false,
            Nullable => true,
            Unknown => !base_ref_not_null,
        }
    }
}

/// Resolve `attnotnull` for every (schema, relation, attname) we want a
/// nullability lookup for. Returns `false` for keys we couldn't resolve
/// (missing relation, dropped column, etc.) — the analyzer treats absence
/// as "no evidence", same as missing pg_attribute entries on the direct
/// (table_oid, attnum) path.
async fn resolve_base_ref_attnotnull(
    client: &Client,
    refs: &[Option<nullability::BaseColumnRef>],
) -> std::collections::HashMap<nullability::BaseColumnRef, bool> {
    let mut out = std::collections::HashMap::new();
    let unique: std::collections::HashSet<&nullability::BaseColumnRef> =
        refs.iter().filter_map(|r| r.as_ref()).collect();
    if unique.is_empty() {
        return out;
    }
    let mut schemas: Vec<&str> = Vec::with_capacity(unique.len());
    let mut relations: Vec<&str> = Vec::with_capacity(unique.len());
    let mut attnames: Vec<&str> = Vec::with_capacity(unique.len());
    for r in &unique {
        schemas.push(&r.schema);
        relations.push(&r.relation);
        attnames.push(&r.attname);
    }
    let rows = match client.query(
        r#"
        WITH ask(schema, rel, att) AS (
            SELECT * FROM unnest($1::text[], $2::text[], $3::text[])
        )
        SELECT ask.schema, ask.rel, ask.att, a.attnotnull
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema OR (ask.schema = '' AND n.nspname = ANY(current_schemas(false)))
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.rel
        JOIN pg_attribute a ON a.attrelid = c.oid AND a.attname = ask.att
        WHERE a.attnum > 0 AND NOT a.attisdropped
        "#,
        &[&schemas, &relations, &attnames],
    ).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("resolve_base_ref_attnotnull: {e}");
            return out;
        }
    };
    for row in &rows {
        let schema: String = row.get(0);
        let relation: String = row.get(1);
        let attname: String = row.get(2);
        let not_null: bool = row.get(3);
        // For unqualified refs we passed an empty schema in `ask.schema`;
        // the row reports it back the same way. The map key matches by
        // value equality on (schema, relation, attname).
        out.insert(
            nullability::BaseColumnRef { schema, relation, attname },
            not_null,
        );
    }
    out
}
