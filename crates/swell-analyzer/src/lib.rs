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
pub use ts_types::TypeCatalog;

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
    pub type_overrides: BTreeMap<String, String>,
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
        let attnotnull = catalog::fetch_attnotnull(&self.client, &pairs).await?;

        let null_hints = nullability::explain_nullability(
            &self.client, sql, &described.params, described.columns.len(),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::debug!("EXPLAIN failed for `{sql}`: {e}");
            nullability::NullabilityHints::unknown(described.columns.len())
        });

        let json_shapes = json_shape::infer_shapes(
            &self.client, &self.catalog, sql, described.columns.len(),
        ).await;

        let param_info = param_nullability::infer(
            &self.client, sql, described.params.len(),
        ).await;

        // Resolve column (table_oid, attnum) → (schema, table, column).
        // One round-trip for every referenced base-table column. Empty
        // when the query has no direct base-table refs (pure expressions).
        let col_table_refs = resolve_column_refs(&self.client, &pairs).await;

        let params = described.params.iter().enumerate()
            .map(|(i, t)| {
                let info = param_info.get(i).cloned().unwrap_or_default();
                InferredParam {
                    oid: t.oid(),
                    ts_type: catalog::render_for_oid(&self.catalog, t.oid(), t),
                    nullable: info.nullable,
                    table_ref: info.table_ref,
                }
            })
            .collect();

        let columns = described.columns.iter().enumerate()
            .map(|(i, c)| {
                let base_not_null = c.table_oid != 0 && c.attnum > 0 &&
                    attnotnull.get(&(c.table_oid, c.attnum)).copied().unwrap_or(false);
                let inferred_nullable = decide_nullability(
                    c, &attnotnull, null_hints.by_column.get(i).copied()
                        .unwrap_or(nullability::NullVerdict::Unknown),
                );
                let oid_ts = catalog::render_for_oid(&self.catalog, c.type_.oid(), &c.type_);
                let json_ts = json_shapes.by_target.get(i).cloned().flatten();
                let inferred_ts = json_ts.unwrap_or(oid_ts);

                let ov = overrides::parse(&c.name);
                let table_ref = col_table_refs.get(&(c.table_oid, c.attnum)).cloned();
                InferredColumn {
                    name: ov.clean_name,
                    oid: c.type_.oid(),
                    nullable: ov.force_nullable.unwrap_or(inferred_nullable),
                    ts_type: ov.force_ts_type.unwrap_or(inferred_ts),
                    table_ref,
                    base_not_null,
                }
            })
            .collect();

        Ok(InferredQuery { sql: sql.to_string(), params, columns })
    }

    /// Fetch the full column list of a single base table, used by
    /// codegen to emit a reusable `interface SchemaTable { … }`. Returns
    /// `None` if the table can't be resolved (dropped, schema lookup
    /// failed, etc.) — the caller falls back to inlining types.
    pub async fn table_schema(
        &self, schema: &str, table: &str,
    ) -> Result<Option<TableSchema>> {
        let rows = self.client.query(
            r#"
            SELECT a.attname, a.atttypid::bigint, t.typname, a.attnotnull
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_attribute a ON a.attrelid = c.oid
            JOIN pg_type t ON t.oid = a.atttypid
            WHERE n.nspname = $1 AND c.relname = $2
              AND a.attnum > 0 AND NOT a.attisdropped
            ORDER BY a.attnum
            "#,
            &[&schema, &table],
        ).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let columns: Vec<TableSchemaColumn> = rows.iter().map(|row| {
            let name: String = row.get(0);
            let oid: i64 = row.get(1);
            let typname: String = row.get(2);
            let not_null: bool = row.get(3);
            TableSchemaColumn {
                name,
                oid: oid as u32,
                ts_type: self.catalog.render_oid(oid as u32, &typname),
                not_null,
            }
        }).collect();
        Ok(Some(TableSchema {
            schema: schema.into(),
            table: table.into(),
            columns,
        }))
    }
}

/// Resolve `(table_oid, attnum)` → `TableColRef { schema, table, column }`
/// for every base-table-backed column in one round trip.
async fn resolve_column_refs(
    client: &Client,
    pairs: &[(u32, i16)],
) -> HashMap<(u32, i16), TableColRef> {
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
        SELECT n.nspname, c.relname, att.attname, ask.t, ask.a
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
            tracing::debug!("resolve_column_refs: {e}");
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
        out.insert((t as u32, a as i16), TableColRef {
            schema, table, column,
        });
    }
    out
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
