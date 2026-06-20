//! Query analysis pipeline.
//!
//! `Analyzer` owns one `tokio_postgres::Client` plus the per-database
//! catalog state (`TypeCatalog`, unsafe-cast pairs, `typname → oid`).
//! Each `analyze(sql)` runs PARSE/DESCRIBE + EXPLAIN, builds an
//! `Analyzed` via `build::build`, and turns it into an `InferredQuery`.

pub mod analyzed;
pub mod build;
pub mod catalog;
pub mod describe;
pub mod json_shape;
pub mod lowering;
pub mod param_nullability;
pub mod plan;
pub mod query;
pub mod scope;
pub mod ts_types;

pub use query::{
    InferredColumn, InferredParam, InferredQuery, RowVariant, TableColRef, TableSchema,
    TableSchemaColumn,
};
pub use ts_types::{Direction, TypeCatalog, TypeOverride};

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio_postgres::{Client, Config, NoTls};

pub struct Analyzer {
    pub client: Client,
    pub catalog: TypeCatalog,
    /// User-defined `castmethod='f'` `(source, target)` pairs for the
    /// per-Cast `is_unsafe` check at lowering time.
    pub unsafe_casts: HashSet<(u32, u32)>,
    /// `pg_type.typname → oid` for resolving `TypeCast` targets.
    pub typname_to_oid: HashMap<String, u32>,
}

pub struct AnalyzerOptions {
    pub database_url: String,
    pub schemas: Vec<String>,
    pub type_overrides: BTreeMap<String, ts_types::TypeOverride>,
}

impl Analyzer {
    /// Connect, load `pg_catalog` info, and probe `pg_cast`. Pins
    /// `plan_cache_mode = force_generic_plan` so EXPLAIN plans match
    /// what PARSE/DESCRIBE produces.
    pub async fn connect(opts: AnalyzerOptions) -> Result<Self> {
        let mut cfg: Config = opts
            .database_url
            .parse()
            .with_context(|| format!("invalid DATABASE_URL: {}", opts.database_url))?;
        cfg.options("-c plan_cache_mode=force_generic_plan");

        let (client, connection) = cfg.connect(NoTls).await.context("connecting to Postgres")?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!("postgres connection error: {e}");
            }
        });

        let mut catalog = catalog::load_type_catalog(&client, &opts.schemas)
            .await
            .context("loading pg_catalog")?;
        catalog.by_name = opts.type_overrides;

        let unsafe_casts = catalog::fetch_unsafe_casts(&client).await;
        let typname_to_oid = catalog::fetch_typname_to_oid(&client).await;

        Ok(Self {
            client,
            catalog,
            unsafe_casts,
            typname_to_oid,
        })
    }

    pub async fn schema_fingerprint(&self, schemas: &[String]) -> Result<String> {
        catalog::schema_fingerprint(&self.client, schemas).await
    }

    pub async fn analyze(&self, sql: &str) -> Result<InferredQuery> {
        let described = describe::describe(&self.client, sql).await?;

        let pairs: Vec<(u32, i16)> = described
            .columns
            .iter()
            .filter(|c| c.table_oid != 0 && c.attnum > 0)
            .map(|c| (c.table_oid, c.attnum))
            .collect();
        let column_meta = resolve_column_meta(&self.client, &pairs).await;

        let plan_walk = plan::explain(&self.client, sql).await.unwrap_or_else(|e| {
            tracing::debug!("EXPLAIN failed for `{sql}`: {e}");
            plan::PlanWalk::default()
        });

        let param_info = param_nullability::infer(&self.client, sql, described.params.len()).await;
        let param_bindings: HashMap<usize, TableColRef> = param_info
            .iter()
            .enumerate()
            .filter_map(|(i, info)| info.table_ref.clone().map(|tr| (i + 1, tr)))
            .collect();

        let analyzed = build::build(
            &self.client,
            sql,
            &described,
            plan_walk.clone(),
            &column_meta,
            &param_bindings,
            self.unsafe_casts.clone(),
            self.typname_to_oid.clone(),
        )
        .await?;

        let json_shapes =
            json_shape::infer_shapes(&self.client, &self.catalog, sql, described.columns.len())
                .await;

        let params: Vec<InferredParam> = described
            .params
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let info = param_info.get(i).cloned().unwrap_or_default();
                InferredParam {
                    ts_type: catalog::render_for_oid(&self.catalog, t.oid(), t, Direction::Write),
                    nullable: info.nullable,
                    table_ref: info.table_ref,
                }
            })
            .collect();

        let columns: Vec<InferredColumn> = described
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let expr = analyzed
                    .outputs
                    .get(i)
                    .map(|o| &o.expr)
                    .unwrap_or(&analyzed::Expr::Unknown);
                let inferred_nullable = decide_nullability(c, &column_meta, build::verdict(expr));
                let oid_ts = catalog::render_for_oid(
                    &self.catalog,
                    c.type_.oid(),
                    &c.type_,
                    Direction::Read,
                );
                let json_ts = json_shapes.by_target.get(i).cloned().flatten();
                let setop_lit_ts = infer_setop_literal_union(expr);
                let inferred_ts = setop_lit_ts.or(json_ts).unwrap_or(oid_ts);

                // SQLx-style override markers in the alias: `col!`
                // forces NOT NULL, `col?` forces nullable. The marker
                // stays on the column name end-to-end.
                let force_nullable = match c.name.chars().last() {
                    Some('!') => Some(false),
                    Some('?') => Some(true),
                    _ => None,
                };
                let table_ref = column_meta
                    .get(&(c.table_oid, c.attnum))
                    .map(|m| m.table_ref.clone());
                InferredColumn {
                    name: c.name.clone(),
                    nullable: force_nullable.unwrap_or(inferred_nullable),
                    ts_type: inferred_ts,
                    table_ref,
                }
            })
            .collect();

        let row_variants = build_row_variants(sql, &analyzed, &plan_walk, &columns);
        Ok(InferredQuery {
            sql: sql.to_string(),
            params,
            columns,
            row_variants,
        })
    }

    /// Per-table column list for every requested `(schema, table)` in
    /// one round trip. Dropped or missing tables are skipped silently.
    pub async fn table_schemas(&self, pairs: &[(String, String)]) -> Result<Vec<TableSchema>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let schemas: Vec<&str> = pairs.iter().map(|(s, _)| s.as_str()).collect();
        let tables: Vec<&str> = pairs.iter().map(|(_, t)| t.as_str()).collect();
        let rows = self
            .client
            .query(
                r#"
            WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
            SELECT n.nspname, c.relname, a.attname, a.atttypid::bigint, t.typname, a.attnotnull
            FROM ask
            JOIN pg_namespace n ON n.nspname = ask.schema
            JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
            JOIN pg_attribute a ON a.attrelid = c.oid
            JOIN pg_type t      ON t.oid = a.atttypid
            WHERE a.attnum > 0 AND NOT a.attisdropped
            ORDER BY n.nspname, c.relname, a.attnum
            "#,
                &[&schemas, &tables],
            )
            .await?;
        let mut grouped: BTreeMap<(String, String), Vec<TableSchemaColumn>> = BTreeMap::new();
        for row in &rows {
            let schema: String = row.get(0);
            let table: String = row.get(1);
            let name: String = row.get(2);
            let oid: i64 = row.get(3);
            let typname: String = row.get(4);
            let not_null: bool = row.get(5);
            grouped
                .entry((schema, table))
                .or_default()
                .push(TableSchemaColumn {
                    name,
                    ts_type: self
                        .catalog
                        .render_oid(oid as u32, &typname, Direction::Read),
                    not_null,
                });
        }
        Ok(grouped
            .into_iter()
            .map(|((schema, table), columns)| TableSchema {
                schema,
                table,
                columns,
            })
            .collect())
    }
}

/// `(table_oid, attnum) → ResolvedBaseCol` in one round trip. Public
/// so `build`'s view-recursion can reuse it for the view's own SQL.
pub async fn resolve_column_meta(client: &Client, pairs: &[(u32, i16)]) -> build::ColumnMeta {
    use build::ResolvedBaseCol;
    if pairs.is_empty() {
        return HashMap::new();
    }
    let unique: HashSet<(u32, i16)> = pairs.iter().copied().collect();
    let tables: Vec<i64> = unique.iter().map(|(t, _)| *t as i64).collect();
    let attnums: Vec<i32> = unique.iter().map(|(_, a)| *a as i32).collect();
    let rows = match client
        .query(
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
        )
        .await
    {
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
        out.insert(
            (t as u32, a as i16),
            ResolvedBaseCol {
                table_ref: TableColRef {
                    schema,
                    table,
                    column,
                },
                not_null,
            },
        );
    }
    out
}

/// Set-op of bare literals → deduped TS literal union (`"paid" |
/// "open"`). Returns `None` for non-SetOp or non-literal branches.
fn infer_setop_literal_union(expr: &analyzed::Expr) -> Option<String> {
    let analyzed::Expr::SetOp(branches) = expr else {
        return None;
    };
    if branches.len() < 2 {
        return None;
    }
    let mut unique: Vec<String> = Vec::new();
    for b in branches {
        let lit = lowering::as_literal(b)?.to_ts_literal();
        if !unique.contains(&lit) {
            unique.push(lit);
        }
    }
    (!unique.is_empty()).then(|| unique.join(" | "))
}

/// Row-variant union for FULL OUTER JOIN (three variants: left-only,
/// right-only, both) and GROUPING SETS (one variant per set, with
/// un-grouped GROUP BY columns set to literal `null`).
fn build_row_variants(
    sql: &str,
    analyzed: &analyzed::Analyzed,
    plan_walk: &plan::PlanWalk,
    columns: &[InferredColumn],
) -> Vec<RowVariant> {
    if let Some(v) = build_full_join_variants(analyzed, plan_walk, columns) {
        return v;
    }
    if let Some(v) = build_grouping_sets_variants(sql, columns) {
        return v;
    }
    Vec::new()
}

fn build_full_join_variants(
    analyzed: &analyzed::Analyzed,
    plan_walk: &plan::PlanWalk,
    columns: &[InferredColumn],
) -> Option<Vec<RowVariant>> {
    let (left, right) = plan_walk.root_full_join.as_ref()?;
    let strip_suffix = |s: &str| {
        let t = s.trim_end_matches(|c: char| c.is_ascii_digit());
        t.trim_end_matches('_').to_string()
    };
    let side_of = |a: &str| {
        if left.contains(a) {
            Some(true)
        } else if right.contains(a) {
            Some(false)
        } else {
            None
        }
    };
    let col_side: Vec<Option<bool>> = (0..columns.len())
        .map(|i| {
            let alias = analyzed
                .outputs
                .get(i)
                .and_then(|o| lowering::as_column(&o.expr))
                .map(|c| c.alias.clone())?;
            side_of(&alias).or_else(|| side_of(&strip_suffix(&alias)))
        })
        .collect();
    let mk = |on_left_null: bool, on_right_null: bool| -> RowVariant {
        let overrides = columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| match col_side[i] {
                Some(true) if on_left_null => Some((c.name.clone(), "null".into())),
                Some(false) if on_right_null => Some((c.name.clone(), "null".into())),
                _ => None,
            })
            .collect();
        RowVariant { overrides }
    };
    Some(vec![mk(false, true), mk(true, false), mk(false, false)])
}

fn column_ref_name_in_grouping(node: &pg_query::protobuf::Node) -> Option<String> {
    use pg_query::protobuf::node::Node as NB;
    let NB::ColumnRef(cr) = node.node.as_ref()? else {
        return None;
    };
    let last = cr.fields.last()?;
    match last.node.as_ref()? {
        NB::String(s) => Some(s.sval.clone()),
        _ => None,
    }
}

fn build_grouping_sets_variants(sql: &str, columns: &[InferredColumn]) -> Option<Vec<RowVariant>> {
    use pg_query::protobuf::{self, node::Node as NB};
    let parsed = pg_query::parse(sql).ok()?;
    let raw = parsed.protobuf.stmts.first()?;
    let NB::SelectStmt(select) = raw.stmt.as_ref()?.node.as_ref()? else {
        return None;
    };
    // Find a GroupingSet of kind Sets in the group clause.
    let sets = select
        .group_clause
        .iter()
        .find_map(|n| match n.node.as_ref()? {
            NB::GroupingSet(gs) if gs.kind == protobuf::GroupingSetKind::GroupingSetSets as i32 => {
                Some(gs.content.clone())
            }
            _ => None,
        })?;
    // Each entry is one grouping set. PG flattens single-column `(col)`
    // to a bare ColumnRef; empty `()` becomes a nested GroupingSet of
    // kind Empty; multi-column `(a, b)` is a List of ColumnRefs.
    let mut variants_keys: Vec<std::collections::HashSet<String>> = Vec::new();
    for entry in &sets {
        let mut keys = std::collections::HashSet::new();
        match entry.node.as_ref()? {
            NB::List(l) => {
                for item in &l.items {
                    if let Some(name) = column_ref_name_in_grouping(item) {
                        keys.insert(name);
                    }
                }
            }
            NB::ColumnRef(_) => {
                if let Some(name) = column_ref_name_in_grouping(entry) {
                    keys.insert(name);
                }
            }
            NB::GroupingSet(_) => {} // empty set
            _ => continue,
        }
        variants_keys.push(keys);
    }
    if variants_keys.is_empty() {
        return None;
    }
    let all_keys: std::collections::HashSet<String> =
        variants_keys.iter().flatten().cloned().collect();
    let variants = variants_keys
        .iter()
        .map(|keys| {
            let mut ov = BTreeMap::new();
            for c in columns {
                if all_keys.contains(&c.name) && !keys.contains(&c.name) {
                    ov.insert(c.name.clone(), "null".to_string());
                }
            }
            RowVariant { overrides: ov }
        })
        .collect();
    Some(variants)
}

/// Combine RowDescription's `attnotnull` with the lowered-Expr verdict
/// into a final nullable bool.
fn decide_nullability(
    c: &describe::DescribedColumn,
    column_meta: &build::ColumnMeta,
    verdict: build::Verdict,
) -> bool {
    use build::Verdict::*;
    if c.table_oid != 0 && c.attnum > 0 {
        let base_nn = column_meta
            .get(&(c.table_oid, c.attnum))
            .map(|m| m.not_null)
            .unwrap_or(false);
        // (true, Nullable) → nullable (outer-join trumps attnotnull).
        // (true, _)        → not nullable.
        // (false, _)       → nullable.
        !base_nn || matches!(verdict, Nullable)
    } else {
        !matches!(verdict, NotNullable)
    }
}
