//! Query analysis pipeline.
//!
//! `Analyzer` owns one `tokio_postgres::Client` and a cached `TypeCatalog`.
//! Each call to `analyze` runs PARSE + DESCRIBE for the supplied SQL, then
//! enriches the result with `pg_attribute.attnotnull`, `EXPLAIN VERBOSE`
//! join nullability, and JSON shape inference.

pub mod describe;
pub mod catalog;
pub mod analyzed;
pub mod plan;
pub mod scope;
pub mod lowering;
pub mod build;
pub mod param_nullability;
pub mod json_shape;
pub mod ts_types;
pub mod query;

pub use query::{
    InferredColumn, InferredParam, InferredQuery, RowVariant, TableColRef, TableSchema,
    TableSchemaColumn,
};
pub use ts_types::{Direction, TypeCatalog, TypeOverride};

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use tokio_postgres::{Client, Config, NoTls};

pub struct Analyzer {
    pub client: Client,
    pub catalog: TypeCatalog,
    /// `(source_typoid, target_typoid)` pairs that have a user-defined
    /// `castmethod='f'` cast in this database. The lowering pass
    /// checks the *specific* pair for each `Expr::Cast`; an
    /// unrelated unsafe cast doesn't widen unrelated casts.
    pub unsafe_casts: std::collections::HashSet<(u32, u32)>,
    /// `pg_type.typname → oid` map used by `Cast` lowering to resolve
    /// a target type name to its OID.
    pub typname_to_oid: std::collections::HashMap<String, u32>,
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

        let unsafe_casts = catalog::fetch_unsafe_casts(&client).await;
        let typname_to_oid = catalog::fetch_typname_to_oid(&client).await;

        Ok(Self { client, catalog, unsafe_casts, typname_to_oid })
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
        // Per-(table_oid, attnum) → (schema, table, column, attnotnull)
        // in one round trip. Used both to populate
        // `InferredColumn.table_ref` and as the fallback source of
        // `Expr::Column` for star-expansion outputs.
        let column_meta = resolve_column_meta(&self.client, &pairs).await;

        // Walk the EXPLAIN plan tree for structural facts only —
        // scan aliases, outer-join widening, function-scan non-null
        // sources, root FULL JOIN. No EXPLAIN-text expression
        // strings are read from this point on.
        let plan_walk = plan::explain(&self.client, sql).await
            .unwrap_or_else(|e| {
                tracing::debug!("EXPLAIN failed for `{sql}`: {e}");
                plan::PlanWalk::default()
            });

        let param_info = param_nullability::infer(
            &self.client, sql, described.params.len(),
        ).await;
        let param_bindings: std::collections::HashMap<usize, TableColRef> = param_info.iter()
            .enumerate()
            .filter_map(|(i, info)| info.table_ref.clone().map(|tr| (i + 1, tr)))
            .collect();

        // Single-pass lowering: SQL AST + plan-derived scope + the
        // pre-fetched column_meta → `Analyzed`. Every downstream
        // verdict / TS-type / row-variant decision walks the `Expr`
        // tree this produces.
        let analyzed = build::build(
            &self.client, sql, &described, plan_walk.clone(),
            &column_meta, &param_bindings,
            self.unsafe_casts.clone(), self.typname_to_oid.clone(),
        ).await?;

        let json_shapes = json_shape::infer_shapes(
            &self.client, &self.catalog, sql, described.columns.len(),
        ).await;

        let params: Vec<InferredParam> = described.params.iter().enumerate()
            .map(|(i, t)| {
                let info = param_info.get(i).cloned().unwrap_or_default();
                InferredParam {
                    ts_type: catalog::render_for_oid(&self.catalog, t.oid(), t, Direction::Write),
                    nullable: info.nullable,
                    table_ref: info.table_ref,
                }
            })
            .collect();

        let columns: Vec<InferredColumn> = described.columns.iter().enumerate()
            .map(|(i, c)| {
                let expr = analyzed.outputs.get(i)
                    .map(|o| &o.expr)
                    .unwrap_or(&analyzed::Expr::Unknown);
                let verdict = build::verdict(expr);
                let inferred_nullable = decide_nullability(c, &column_meta, verdict);
                let oid_ts = catalog::render_for_oid(&self.catalog, c.type_.oid(), &c.type_, Direction::Read);
                let json_ts = json_shapes.by_target.get(i).cloned().flatten();
                let setop_lit_ts = infer_setop_literal_union(expr);
                let inferred_ts = setop_lit_ts.or(json_ts).unwrap_or(oid_ts);

                // SQLx-style alias suffix overrides: `as "col!"` /
                // `as "col?"`. Postgres surfaces the marker in the
                // RowDescription name; the marker stays on the column
                // name end-to-end so the row type matches the SQL the
                // user wrote.
                let force_nullable = match c.name.chars().last() {
                    Some('!') => Some(false),
                    Some('?') => Some(true),
                    _ => None,
                };
                let table_ref = column_meta.get(&(c.table_oid, c.attnum))
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
        Ok(InferredQuery { sql: sql.to_string(), params, columns, row_variants })
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
                   a.attnotnull
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
                ts_type: self.catalog.render_oid(oid as u32, &typname, Direction::Read),
                not_null,
            });
        }
        Ok(grouped.into_iter()
            .map(|((schema, table), columns)| TableSchema { schema, table, columns })
            .collect())
    }
}

/// Resolve `(table_oid, attnum)` → `ResolvedBaseCol` in one round
/// trip — populates `InferredColumn.table_ref` *and* provides the
/// star-expansion fallback for `build::build`. Public so that
/// `build`'s view-recursion path can reuse it for the view's own
/// SQL.
pub async fn resolve_column_meta(
    client: &Client,
    pairs: &[(u32, i16)],
) -> build::ColumnMeta {
    use build::ResolvedBaseCol;
    if pairs.is_empty() { return HashMap::new(); }
    let unique: std::collections::HashSet<(u32, i16)> = pairs.iter().copied().collect();
    let tables:  Vec<i64> = unique.iter().map(|(t, _)| *t as i64).collect();
    let attnums: Vec<i32> = unique.iter().map(|(_, a)| *a as i32).collect();
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
        Err(e) => { tracing::debug!("resolve_column_meta: {e}"); return HashMap::new(); }
    };
    let mut out = HashMap::with_capacity(rows.len());
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let column: String = row.get(2);
        let t: i64 = row.get(3);
        let a: i32 = row.get(4);
        let not_null: bool = row.get(5);
        out.insert((t as u32, a as i16), ResolvedBaseCol {
            table_ref: TableColRef { schema, table, column },
            not_null,
        });
    }
    out
}

/// Across set-op branches, if every branch's expression is a bare
/// literal, render the column type as a deduped TS literal union.
/// Walks the lowered `Expr::SetOp` — each branch is a structured
/// `Expr`, no EXPLAIN-text parsing.
fn infer_setop_literal_union(expr: &analyzed::Expr) -> Option<String> {
    let branches = match expr {
        analyzed::Expr::SetOp(b) => b,
        _ => return None,
    };
    if branches.len() < 2 { return None; }
    let mut unique: Vec<String> = Vec::new();
    for b in branches {
        let lit = lowering::as_literal(b)?.to_ts_literal();
        if !unique.contains(&lit) { unique.push(lit); }
    }
    (!unique.is_empty()).then(|| unique.join(" | "))
}

/// Build row-level variants for queries that produce a discriminated
/// union. Two cases:
///
///   - FULL OUTER JOIN: three variants — left-only, right-only, both.
///     A column's side is inferred from `Expr::Column.alias` on the
///     lowered output expression. The "absent" side's columns become
///     literal `null`; the present side keeps its rendered TS type.
///
///   - GROUPING SETS (a, b, c, …): one variant per grouping set.
///     Columns whose names appear in the set keep their type;
///     un-grouped GROUP BY columns become literal `null`. Aggregates
///     (count, sum, …) are untouched.
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
    // Decide each column's source side from its lowered output expr.
    // `Expr::Column` (or `Cast(Column)`) gives us the alias straight
    // off the resolved base ref — no string parsing on the EXPLAIN
    // deparse text.
    let strip_suffix = |s: &str| {
        let t = s.trim_end_matches(|c: char| c.is_ascii_digit());
        t.trim_end_matches('_').to_string()
    };
    let side_of = |a: &str| {
        if left.contains(a) { Some(true) }
        else if right.contains(a) { Some(false) }
        else { None }
    };
    let col_side: Vec<Option<bool>> = (0..columns.len()).map(|i| {
        let alias = analyzed.outputs.get(i)
            .and_then(|o| lowering::as_column(&o.expr))
            .map(|c| c.alias.clone())?;
        side_of(&alias).or_else(|| side_of(&strip_suffix(&alias)))
    }).collect();
    let mk = |on_left_null: bool, on_right_null: bool| -> RowVariant {
        let overrides = columns.iter().enumerate()
            .filter_map(|(i, c)| match col_side[i] {
                Some(true)  if on_left_null  => Some((c.name.clone(), "null".into())),
                Some(false) if on_right_null => Some((c.name.clone(), "null".into())),
                _ => None,
            })
            .collect();
        RowVariant { overrides }
    };
    Some(vec![mk(false, true), mk(true, false), mk(false, false)])
}

/// Pick the column name out of a `ColumnRef` node inside a GROUPING
/// SETS entry. Multi-segment refs (`t.col`) take the last segment.
fn column_ref_name_in_grouping(node: &pg_query::protobuf::Node) -> Option<String> {
    use pg_query::protobuf::node::Node as NodeBody;
    let cr = match node.node.as_ref()? {
        NodeBody::ColumnRef(c) => c,
        _ => return None,
    };
    let last = cr.fields.last()?;
    match last.node.as_ref()? {
        NodeBody::String(s) => Some(s.sval.clone()),
        _ => None,
    }
}

/// Detect `GROUP BY GROUPING SETS (...)` in the SQL and build one
/// variant per set: columns named in the set keep their type, others
/// (GROUP BY keys not in this set) become literal `null`. The
/// detection uses pg_query's AST — `GroupingSet` nodes with
/// `kind = GroupingSetSets`.
fn build_grouping_sets_variants(
    sql: &str,
    columns: &[InferredColumn],
) -> Option<Vec<RowVariant>> {
    use pg_query::protobuf::{self, node::Node as NodeBody};
    let parsed = pg_query::parse(sql).ok()?;
    let raw = parsed.protobuf.stmts.first()?;
    let stmt = raw.stmt.as_ref()?.node.as_ref()?;
    let select = match stmt {
        NodeBody::SelectStmt(s) => s,
        _ => return None,
    };
    // Walk group_clause looking for a GroupingSet of kind Sets.
    let sets = select.group_clause.iter()
        .find_map(|n| match n.node.as_ref()? {
            NodeBody::GroupingSet(gs) => {
                if gs.kind == protobuf::GroupingSetKind::GroupingSetSets as i32 {
                    Some(gs.content.clone())
                } else {
                    None
                }
            }
            _ => None,
        })?;
    // Each entry in `content` is one grouping set. PG flattens the
    // single-column parenthesised form `(col)` into a bare ColumnRef,
    // and represents the empty set `()` as a nested GroupingSet of
    // kind `GroupingSetEmpty`. Multi-column sets `(a, b)` come
    // through as a List of ColumnRefs.
    let mut variants_keys: Vec<std::collections::HashSet<String>> = Vec::new();
    for entry in &sets {
        let mut keys = std::collections::HashSet::new();
        match entry.node.as_ref()? {
            NodeBody::List(l) => {
                for item in &l.items {
                    if let Some(name) = column_ref_name_in_grouping(item) {
                        keys.insert(name);
                    }
                }
            }
            NodeBody::ColumnRef(_) => {
                if let Some(name) = column_ref_name_in_grouping(entry) {
                    keys.insert(name);
                }
            }
            NodeBody::GroupingSet(_) => {
                // Empty grouping set `()` — keys stays empty (no
                // grouping columns retained).
            }
            _ => continue,
        }
        variants_keys.push(keys);
    }
    if variants_keys.is_empty() { return None; }
    // Union of all keys across all grouping sets — these are the
    // "grouping columns" that may be NULL in some variant. Other
    // columns (aggregates) stay unchanged.
    let all_keys: std::collections::HashSet<String> = variants_keys.iter().flatten().cloned().collect();
    let mut variants = Vec::with_capacity(variants_keys.len());
    for keys in &variants_keys {
        let mut ov = std::collections::BTreeMap::new();
        for c in columns {
            if all_keys.contains(&c.name) && !keys.contains(&c.name) {
                ov.insert(c.name.clone(), "null".to_string());
            }
        }
        variants.push(RowVariant { overrides: ov });
    }
    Some(variants)
}

/// Combine RowDescription's `attnotnull` with the lowered-Expr verdict
/// into a final nullable bool.
///
/// | base table col? | attnotnull | verdict     | nullable |
/// |-----------------|------------|-------------|----------|
/// | yes             | NOT NULL   | Nullable    | yes (outer-join trumps) |
/// | yes             | NOT NULL   | otherwise   | no       |
/// | yes             | nullable   | *           | yes      |
/// | no              | n/a        | NotNullable | no       |
/// | no              | n/a        | otherwise   | yes      |
fn decide_nullability(
    c: &describe::DescribedColumn,
    column_meta: &build::ColumnMeta,
    verdict: build::Verdict,
) -> bool {
    use build::Verdict::*;
    if c.table_oid != 0 && c.attnum > 0 {
        let base_not_null = column_meta.get(&(c.table_oid, c.attnum))
            .map(|m| m.not_null).unwrap_or(false);
        match (base_not_null, verdict) {
            (true, Nullable) => true,
            (true, _)        => false,
            (false, _)       => true,
        }
    } else {
        !matches!(verdict, NotNullable)
    }
}
