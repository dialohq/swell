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

        // Extra verdict refinement: a `COALESCE(...)` whose args include any
        // NOT-NULL base column is non-null. `classify` already handles the
        // trailing-literal case; this handles `COALESCE(nullable_col,
        // not_null_col)` by looking up attnotnull for each `<alias>.<col>`
        // arg using the plan's scan map.
        let coalesce_refined = refine_coalesce_non_null_with(
            &null_hints,
            &scan_attnotnull,
        );

        // For each column, see if every branch's expression resolves to
        // a NOT NULL base-column reference; if so, upgrade to
        // NotNullable. Handles UNION/INTERSECT/EXCEPT non-null inference
        // and plain `<alias>.<col>` references that classify can't
        // resolve on its own.
        let column_ref_refined = refine_via_attnotnull(
            &null_hints,
            &scan_attnotnull,
        );

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

        let columns: Vec<InferredColumn> = described.columns.iter().enumerate()
            .map(|(i, c)| {
                let raw = null_hints.by_column.get(i).copied()
                    .unwrap_or(nullability::NullVerdict::Unknown);
                // Only upgrade an `Unknown` verdict — never override a
                // Nullable verdict (e.g. an outer-join's RHS column).
                let upgrade = matches!(raw, nullability::NullVerdict::Unknown) && (
                    coalesce_refined.get(i).copied().unwrap_or(false)
                    || column_ref_refined.get(i).copied().unwrap_or(false)
                );
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

/// For each output column whose EXPLAIN expression is `COALESCE(...)`,
/// check whether any arg is a NOT NULL base column reference and return
/// `true` for that column index. The caller upgrades the column's
/// verdict to `NotNullable` when this returns true.
///
/// `classify` already handles the trailing-literal case (e.g.
/// `coalesce(x, 'lit')`); this picks up the cases where the non-null
/// guarantor is a NOT NULL column.
fn refine_coalesce_non_null_with(
    hints: &nullability::NullabilityHints,
    attnotnull: &AttnotnullMap,
) -> Vec<bool> {
    let mut out = vec![false; hints.exprs.len()];
    for (i, expr) in hints.exprs.iter().enumerate() {
        let args = match coalesce_args(expr) {
            Some(args) => args,
            None => continue,
        };
        for arg in &args {
            if is_literal_token(arg) {
                out[i] = true;
                break;
            }
            if let Some(k) = resolve_arg(arg, &hints.alias_to_table, attnotnull) {
                if attnotnull.get(&k).copied().unwrap_or(false) {
                    out[i] = true;
                    break;
                }
            }
        }
    }
    out
}

/// For each output column, if every branch's expression resolves to a
/// NOT NULL base-column reference, mark the column NotNullable. This
/// is what makes `SELECT id FROM users INTERSECT SELECT user_id …`
/// produce `result: { id: string }` instead of `string | null` — the
/// classifier alone can't see through an outer SetOp/Append to the
/// underlying base columns.
fn refine_via_attnotnull(
    hints: &nullability::NullabilityHints,
    attnotnull: &AttnotnullMap,
) -> Vec<bool> {
    let mut out = vec![false; hints.branches.len()];
    for (i, branches) in hints.branches.iter().enumerate() {
        if branches.is_empty() { continue; }
        let all_non_null = branches.iter().all(|expr| {
            // The refinement is for bare column references that
            // classify can't resolve through a SetOp/Append wrapper.
            // Casts (and other expression wrappers) are intentionally
            // excluded — the analyzer treats `id::text` as nullable
            // because the cast may not preserve NOT NULL semantics
            // for user-defined types.
            if expr.contains("::") { return false; }
            resolve_arg(expr, &hints.alias_to_table, attnotnull)
                .map(|k| attnotnull.get(&k).copied().unwrap_or(false))
                .unwrap_or(false)
        });
        if all_non_null {
            out[i] = true;
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
    match parse_column_ref(arg) {
        ColumnRefShape::Qualified(alias, col) => {
            // Strip suffix digits from the alias — PG renames duplicate
            // scan aliases as `users_1`, `users_2` in plans (e.g. for
            // FULL JOIN). Try the literal alias first; fall back to
            // the de-numbered form so the user's alias_to_table entry
            // (which uses the literal alias) still resolves.
            let key = alias_to_table.get(&alias)
                .or_else(|| alias_to_table.get(&strip_suffix_digits(&alias)));
            key.map(|(s, t)| (s.clone(), t.clone(), col))
        }
        ColumnRefShape::Bare(col) => {
            let mut hit: Option<(String, String, String)> = None;
            let mut ambiguous = false;
            for (schema, table) in alias_to_table.values() {
                let k = (schema.clone(), table.clone(), col.clone());
                if attnotnull.contains_key(&k) {
                    if hit.is_some() { ambiguous = true; break; }
                    hit = Some(k);
                }
            }
            if ambiguous { None } else { hit }
        }
        ColumnRefShape::None => None,
    }
}

/// Across set-op branches, if every branch's expression is a bare
/// literal, render the column type as a deduped TS literal union.
fn infer_setop_literal_union(branches: &[String]) -> Option<String> {
    if branches.len() < 2 { return None; }
    let mut unique: Vec<String> = Vec::new();
    for b in branches {
        let lit = extract_ts_literal(b)?;
        if !unique.contains(&lit) { unique.push(lit); }
    }
    if unique.is_empty() { return None; }
    Some(unique.join(" | "))
}

/// If `expr` is a literal token (string / numeric / boolean), with an
/// optional `::cast` and outer parens, return its TS-literal form.
fn extract_ts_literal(expr: &str) -> Option<String> {
    let mut s = expr.trim();
    // Peel a single layer of balanced parens.
    while s.starts_with('(') && s.ends_with(')') && is_balanced_paren_wrapper(s) {
        s = &s[1..s.len() - 1].trim();
    }
    // Drop a `::cast` suffix.
    let value = match s.split_once("::") {
        Some((v, _)) => v.trim(),
        None => s,
    };
    if value.is_empty() { return None; }
    // String literal.
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        let inner = value[1..value.len() - 1].replace("''", "'");
        return Some(format!("\"{}\"", inner.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    // Boolean literal.
    let lower = value.to_ascii_lowercase();
    if lower == "true" || lower == "false" {
        return Some(lower);
    }
    // Numeric literal.
    if value.parse::<f64>().is_ok() {
        return Some(value.to_string());
    }
    None
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
    // leading alias. Columns whose source can't be determined are
    // assumed present in all variants (no override).
    use std::collections::BTreeMap;
    let mut col_side: Vec<Option<bool>> = Vec::with_capacity(columns.len()); // Some(true) = left, Some(false) = right
    for (i, c) in columns.iter().enumerate() {
        let expr = hints.exprs.get(i).map(String::as_str).unwrap_or("");
        let alias = expr_leading_alias(expr);
        let stripped = alias.as_deref().map(strip_suffix_digits);
        let side = match alias.as_deref() {
            Some(a) if left.contains(a) => Some(true),
            Some(a) if right.contains(a) => Some(false),
            _ => match stripped.as_deref() {
                Some(a) if left.contains(a) => Some(true),
                Some(a) if right.contains(a) => Some(false),
                _ => None,
            },
        };
        let _ = c; // column itself isn't needed; side is from expr
        col_side.push(side);
    }
    // Three variants: only-left (right→null), only-right (left→null),
    // both (no overrides; falls back to base columns).
    let mk = |on_left_null: bool, on_right_null: bool| -> RowVariant {
        let mut ov: BTreeMap<String, String> = BTreeMap::new();
        for (i, c) in columns.iter().enumerate() {
            match col_side[i] {
                Some(true)  if on_left_null  => { ov.insert(c.name.clone(), "null".into()); }
                Some(false) if on_right_null => { ov.insert(c.name.clone(), "null".into()); }
                _ => {}
            }
        }
        RowVariant { overrides: ov }
    };
    let only_left  = mk(false, true);
    let only_right = mk(true,  false);
    let both       = mk(false, false);
    Some(vec![only_left, only_right, both])
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

/// Extract `alias` from `expr`'s leading `<alias>.<col>` reference.
fn expr_leading_alias(expr: &str) -> Option<String> {
    let trimmed = expr.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let dot = trimmed.find('.')?;
    let prefix = &trimmed[..dot];
    let s = if prefix.starts_with('"') && prefix.ends_with('"') && prefix.len() >= 2 {
        &prefix[1..prefix.len() - 1]
    } else {
        prefix
    };
    if s.is_empty() { return None; }
    Some(s.to_string())
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

fn strip_suffix_digits(s: &str) -> String {
    // `users_1` → `users`. PG appends `_N` to disambiguate duplicate
    // scan aliases within a plan.
    let trimmed = s.trim_end_matches(|c: char| c.is_ascii_digit());
    trimmed.trim_end_matches('_').to_string()
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
