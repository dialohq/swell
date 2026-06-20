//! Query analysis pipeline.
//!
//! `Analyzer` owns one `tokio_postgres::Client` and a cached `TypeCatalog`.
//! Each call to `analyze` runs PARSE + DESCRIBE for the supplied SQL, then
//! enriches the result with `pg_attribute.attnotnull`, `EXPLAIN VERBOSE`
//! join nullability, and JSON shape inference.

pub mod describe;
pub mod catalog;
pub mod explain_expr;
pub mod nullability;
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

        // Pre-fetch attnotnull for every (schema, table) referenced
        // by an EXPLAIN scan. Used by the post-pass refinements:
        //   - `refine_coalesce_non_null`: coalesce arg is a NOT NULL col.
        //   - `refine_via_attnotnull`:    bare column-ref expressions
        //     (including all branches of a UNION/INTERSECT/EXCEPT)
        //     classify as Unknown but are actually NOT NULL.
        let scan_attnotnull = fetch_scan_attnotnull(
            &self.client,
            &null_hints.alias_to_table,
        ).await;

        // Per-column NOT NULL refinement, two sources combined into one
        // bitmask:
        //   - `COALESCE(...)` whose any arg is a NOT NULL base column.
        //     classify already handles `coalesce(x, 'lit')`; this picks
        //     up `coalesce(nullable, not_null_col)` via attnotnull.
        //   - SET-OP branches whose every branch is a NOT NULL base
        //     column ref. Handles UNION/INTERSECT/EXCEPT cases that the
        //     classifier can't see through.
        let refine_upgrade = refine_to_not_null(&null_hints, &scan_attnotnull);

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
                    ts_type: catalog::render_for_oid(&self.catalog, t.oid(), t, Direction::Write),
                    nullable: info.nullable,
                    table_ref: info.table_ref,
                }
            })
            .collect();

        let columns: Vec<InferredColumn> = described.columns.iter().enumerate()
            .map(|(i, c)| {
                let raw = null_hints.by_column.get(i).copied()
                    .unwrap_or(nullability::NullVerdict::Unknown);
                // Only upgrade an `Unknown` verdict — never override a
                // Nullable verdict (e.g. an outer-join's RHS column).
                let upgrade = matches!(raw, nullability::NullVerdict::Unknown)
                    && refine_upgrade.get(i).copied().unwrap_or(false);
                let verdict = if upgrade { nullability::NullVerdict::NotNullable } else { raw };
                let inferred_nullable = decide_nullability(c, &attnotnull, verdict);
                let oid_ts = catalog::render_for_oid(&self.catalog, c.type_.oid(), &c.type_, Direction::Read);
                let json_ts = json_shapes.by_target.get(i).cloned().flatten();
                // Literal-type union across set-op branches: when every
                // branch's expression is a bare literal (string / int /
                // bool), the result column type is the deduped union of
                // those literals — e.g. `UNION ALL SELECT 'paid'` and
                // `'open'` becomes `"paid" | "open"`.
                let setop_lit_ts = null_hints.branches.get(i)
                    .and_then(|b| infer_setop_literal_union(b));
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

        let row_variants = build_row_variants(sql, &null_hints, &columns);
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

type AttnotnullMap = std::collections::HashMap<(String, String, String), bool>;

/// Bulk-fetch `attnotnull` for every column of every scan table in
/// `alias_to_table`. Returned map is keyed by `(schema, table, col)`.
async fn fetch_scan_attnotnull(
    client: &Client,
    alias_to_table: &std::collections::HashMap<String, (String, String)>,
) -> AttnotnullMap {
    use std::collections::HashSet;
    let mut out: AttnotnullMap = std::collections::HashMap::new();
    if alias_to_table.is_empty() {
        return out;
    }
    let pairs: HashSet<(String, String)> = alias_to_table.values().cloned().collect();
    let schemas: Vec<String> = pairs.iter().map(|p| p.0.clone()).collect();
    let tables: Vec<String> = pairs.iter().map(|p| p.1.clone()).collect();
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
            out.insert((row.get(0), row.get(1), row.get(2)), row.get(3));
        }
    }
    out
}

/// Per-column NOT NULL refinement combining two evidence sources:
///   - `COALESCE(...)` where any arg is a NOT NULL base column or a
///     non-null literal.
///   - Every SET-OP branch is a NOT NULL base column reference.
fn refine_to_not_null(
    hints: &nullability::NullabilityHints,
    attnotnull: &AttnotnullMap,
) -> Vec<bool> {
    let n = hints.exprs.len().max(hints.branches.len());
    let mut out = vec![false; n];
    for i in 0..n {
        // Coalesce: any arg is a non-null literal or NOT NULL column.
        if let Some(expr) = hints.exprs.get(i) {
            if let Some(args) = explain_expr::parse_call_args(expr, "coalesce") {
                let non_null = args.iter().any(|a| {
                    explain_expr::is_literal_non_null(a)
                    || resolve_arg(a, &hints.alias_to_table, attnotnull)
                        .map(|k| attnotnull.get(&k).copied().unwrap_or(false))
                        .unwrap_or(false)
                });
                if non_null { out[i] = true; continue; }
            }
        }
        // Set-op branches: every branch is a bare NOT NULL column. Casts
        // are excluded — `id::text` isn't guaranteed to preserve NOT NULL
        // semantics for user-defined types.
        if let Some(branches) = hints.branches.get(i) {
            if !branches.is_empty() && branches.iter().all(|expr| {
                if expr.contains("::") { return false; }
                resolve_arg(expr, &hints.alias_to_table, attnotnull)
                    .map(|k| attnotnull.get(&k).copied().unwrap_or(false))
                    .unwrap_or(false)
            }) {
                out[i] = true;
            }
        }
    }
    out
}

/// Resolve an EXPLAIN expression (possibly with `(…)::cast` wrapping)
/// to a `(schema, table, col)` key. Handles qualified `<alias>.<col>`
/// (looks up via `alias_to_table`) and bare `<col>` (picks the unique
/// scan table that has a column by that name).
fn resolve_arg(
    arg: &str,
    alias_to_table: &std::collections::HashMap<String, (String, String)>,
    attnotnull: &AttnotnullMap,
) -> Option<(String, String, String)> {
    match explain_expr::parse_ref(arg)? {
        explain_expr::Ref::Qualified { alias, col } => {
            // PG renames duplicate scan aliases as `users_1`, `users_2`
            // in plans (e.g. for FULL JOIN). Try the literal alias
            // first; fall back to the de-numbered form so the user's
            // `alias_to_table` entry still resolves.
            let key = alias_to_table.get(alias)
                .or_else(|| alias_to_table.get(explain_expr::strip_suffix_digits(alias)));
            key.map(|(s, t)| (s.clone(), t.clone(), col.to_string()))
        }
        explain_expr::Ref::Bare(col) => {
            let mut matches = alias_to_table.values().filter_map(|(s, t)| {
                let k = (s.clone(), t.clone(), col.to_string());
                attnotnull.contains_key(&k).then_some(k)
            });
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        }
    }
}

/// Across set-op branches, if every branch's expression is a bare
/// literal, render the column type as a deduped TS literal union.
fn infer_setop_literal_union(branches: &[String]) -> Option<String> {
    if branches.len() < 2 { return None; }
    let mut unique: Vec<String> = Vec::new();
    for b in branches {
        let lit = explain_expr::parse_literal_ts(b)?;
        if !unique.contains(&lit) { unique.push(lit); }
    }
    (!unique.is_empty()).then(|| unique.join(" | "))
}

/// Build row-level variants for queries that produce a discriminated
/// union. Currently two cases are handled:
///
///   - FULL OUTER JOIN: three variants — left-only, right-only, both.
///     A column's side is inferred from its EXPLAIN expression's
///     leading alias. The "absent" side's columns become literal
///     `null`; the present side keeps the rendered TS type.
///
///   - GROUPING SETS (a, b, c, …): one variant per grouping set.
///     Columns whose names appear in the set keep their type;
///     un-grouped GROUP BY columns become literal `null`. Aggregates
///     (count, sum, …) are untouched.
fn build_row_variants(
    sql: &str,
    hints: &nullability::NullabilityHints,
    columns: &[InferredColumn],
) -> Vec<RowVariant> {
    if let Some(v) = build_full_join_variants(hints, columns) {
        return v;
    }
    if let Some(v) = build_grouping_sets_variants(sql, columns) {
        return v;
    }
    Vec::new()
}

fn build_full_join_variants(
    hints: &nullability::NullabilityHints,
    columns: &[InferredColumn],
) -> Option<Vec<RowVariant>> {
    let (left, right) = hints.root_full_join.as_ref()?;
    // Decide each column's source side via its EXPLAIN expression's
    // leading alias (Some(true) = left, Some(false) = right). Columns
    // whose source can't be determined are present in every variant.
    let side_of = |a: &str| {
        if left.contains(a) { Some(true) }
        else if right.contains(a) { Some(false) }
        else { None }
    };
    let col_side: Vec<Option<bool>> = (0..columns.len()).map(|i| {
        let expr = hints.exprs.get(i).map(String::as_str).unwrap_or("");
        let alias = explain_expr::leading_alias(expr);
        alias.and_then(side_of)
            .or_else(|| alias.and_then(|a| side_of(explain_expr::strip_suffix_digits(a))))
    }).collect();
    // Three variants: only-left (right→null), only-right (left→null),
    // both (no overrides; falls back to base columns).
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
