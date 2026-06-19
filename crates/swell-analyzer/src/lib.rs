//! Query analysis pipeline.
//!
//! `Analyzer` owns one `tokio_postgres::Client` and a cached `TypeCatalog`.
//! Each call to `analyze` runs PARSE + DESCRIBE for the supplied SQL, then
//! enriches the result with `pg_attribute.attnotnull`, `EXPLAIN VERBOSE`
//! join nullability, and JSON shape inference.

pub mod describe;
pub mod catalog;
pub mod checks;
pub mod nullability;
pub mod param_nullability;
pub mod json_shape;
pub mod overrides;
pub mod ts_types;
pub mod query;

pub use query::{
    InferredColumn, InferredParam, InferredQuery, RowCheck, TableColRef, TableRowVariant,
    TableSchema, TableSchemaColumn,
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

        let json_shapes = json_shape::infer_shapes(
            &self.client, &self.catalog, sql, described.columns.len(),
        ).await;

        // CHECK-constraint refinements (issue #22, Tiers 1-3): literal
        // unions, JSON object shapes, JSON discriminated unions.
        let referenced_tables: Vec<(String, String)> = column_meta.values()
            .map(|m| (m.table_ref.schema.clone(), m.table_ref.table.clone()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let check_refinements = checks::fetch_refinements(&self.client, &referenced_tables).await;

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
                let inferred_nullable = decide_nullability(
                    c, &attnotnull, null_hints.by_column.get(i).copied()
                        .unwrap_or(nullability::NullVerdict::Unknown),
                );
                let oid_ts = catalog::render_for_oid(&self.catalog, c.type_.oid(), &c.type_, Direction::Read);
                let json_ts = json_shapes.by_target.get(i).cloned().flatten();
                let mut inferred_ts = json_ts.unwrap_or(oid_ts);

                let table_ref = column_meta.get(&(c.table_oid, c.attnum))
                    .map(|m| m.table_ref.clone());

                // Apply a CHECK-constraint refinement when the column is
                // a direct base-column reference whose base type is one
                // we'd be willing to narrow (`string` / `number` /
                // `boolean` / `Json`). Custom domain renderings (enums,
                // user types) and user-overridden types are preserved.
                if let Some(tref) = &table_ref {
                    let key = (tref.schema.clone(), tref.table.clone(), tref.column.clone());
                    if let Some(refinement) = check_refinements.get(&key) {
                        if is_narrowable_base_type(&inferred_ts) {
                            if let Some(narrowed) = refinement.render_ts() {
                                inferred_ts = narrowed;
                            }
                        }
                    }
                }

                let ov = overrides::parse(&c.name);
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
        // Per-column refinements from CHECK (issue #22 Tiers 1-3) —
        // applied to the table interface so consumers reading the table
        // type get the narrowed shape too.
        let table_pairs: Vec<(String, String)> = grouped.keys().cloned().collect();
        let col_refinements = checks::fetch_refinements(&self.client, &table_pairs).await;
        for ((schema, table), cols) in grouped.iter_mut() {
            for col in cols.iter_mut() {
                let key = (schema.clone(), table.clone(), col.name.clone());
                if let Some(refinement) = col_refinements.get(&key) {
                    if is_narrowable_base_type(&col.ts_type) {
                        if let Some(narrowed) = refinement.render_ts() {
                            col.ts_type = narrowed;
                        }
                    }
                }
            }
        }

        // Row-level refinements (Tier 3 cross-column). Build the (col →
        // base ts_type) map per table so the row reducer can synthesise
        // `non-null` variants from the column's actual base type.
        let mut col_types_per_table: std::collections::HashMap<
            (String, String),
            std::collections::HashMap<String, String>,
        > = std::collections::HashMap::new();
        for ((schema, table), cols) in &grouped {
            let mut m = std::collections::HashMap::new();
            for col in cols {
                let ts = if col.not_null {
                    col.ts_type.clone()
                } else {
                    format!("{} | null", col.ts_type)
                };
                m.insert(col.name.clone(), ts);
            }
            col_types_per_table.insert((schema.clone(), table.clone()), m);
        }
        let row_refinements = checks::fetch_row_refinements(
            &self.client, &table_pairs, &col_types_per_table,
        ).await;

        Ok(grouped.into_iter()
            .map(|((schema, table), columns)| {
                let row_checks = row_refinements
                    .get(&(schema.clone(), table.clone()))
                    .map(|refs| refs.iter().map(|r| RowCheck {
                        variants: r.variants.iter().map(|v| TableRowVariant {
                            columns: v.columns.clone(),
                        }).collect(),
                    }).collect())
                    .unwrap_or_default();
                TableSchema { schema, table, columns, row_checks }
            })
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

/// CHECK refinements only override types we know are wide enough that
/// narrowing is unambiguously useful. Custom domain / enum renderings
/// and user `[types.by_name]` overrides are preserved — they already
/// carry the user's intent.
fn is_narrowable_base_type(ts: &str) -> bool {
    matches!(ts, "string" | "number" | "boolean" | "Json")
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
