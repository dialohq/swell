//! Top-level `Analyzed` builder.
//!
//! Threads PARSE/DESCRIBE + EXPLAIN + the SQL parse tree into one pass
//! that produces a fully lowered `Analyzed`. No EXPLAIN-text reading
//! downstream of this point.

use crate::analyzed::{Analyzed, Expr, Output, Param, ResolvedCol};
use crate::describe::DescribedQuery;
use crate::lowering::{self, lower};
use crate::plan::PlanWalk;
use crate::query::TableColRef;
use crate::scope::{DerivedColumn, Scope};
use anyhow::Result;
use pg_query::protobuf::{node::Node as NB, Node, SelectStmt};
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use tokio_postgres::Client;

const SETOP_NONE: i32 = pg_query::protobuf::SetOperation::SetopNone as i32;

/// `(table_oid, attnum)` → resolved `(schema, table, column, attnotnull)`.
/// Pre-fetched by the caller from `pg_attribute` in one round trip.
pub type ColumnMeta = HashMap<(u32, i16), ResolvedBaseCol>;

#[derive(Debug, Clone)]
pub struct ResolvedBaseCol {
    pub table_ref: TableColRef,
    pub not_null: bool,
}

pub async fn build(
    client: &Client,
    sql: &str,
    described: &DescribedQuery,
    plan: PlanWalk,
    column_meta: &ColumnMeta,
    param_bindings: &HashMap<usize, TableColRef>,
    unsafe_casts: HashSet<(u32, u32)>,
    typname_to_oid: HashMap<String, u32>,
) -> Result<Analyzed> {
    build_inner(
        client, sql, described, plan, column_meta, param_bindings,
        unsafe_casts, typname_to_oid, &HashSet::new(),
    ).await
}

async fn build_inner(
    client: &Client,
    sql: &str,
    described: &DescribedQuery,
    plan: PlanWalk,
    column_meta: &ColumnMeta,
    param_bindings: &HashMap<usize, TableColRef>,
    unsafe_casts: HashSet<(u32, u32)>,
    typname_to_oid: HashMap<String, u32>,
    visited: &HashSet<u32>,
) -> Result<Analyzed> {
    // Scope is the alias-resolution + nullability environment shared
    // across the whole lowering pass for this statement.
    let mut scope = Scope::build(
        client,
        plan.alias_to_table.clone(),
        plan.nullable_aliases.clone(),
        plan.non_null_aliases.clone(),
        unsafe_casts.clone(),
        typname_to_oid.clone(),
    ).await?;

    // View references: each `RangeVar` whose underlying `pg_class`
    // row has `relkind = 'v'` gets its definition recursively
    // analysed, with the resulting per-column `Expr`s slotted in as a
    // derived alias.
    let view_derived = analyze_view_refs(
        client, sql, &unsafe_casts, &typname_to_oid, visited,
    ).await?;
    let mut derived: HashMap<String, Vec<DerivedColumn>> = view_derived;
    // Attach views first so the SQL-AST derived collection can resolve
    // CTE / subselect refs to views.
    scope.attach_derived(derived.clone());
    for (k, v) in collect_derived(sql, &scope) {
        derived.entry(k).or_insert(v);
    }
    scope.attach_derived(derived);

    // Pull per-output AST nodes from the SQL once.
    let target_source = collect_target_source(sql);

    let outputs = described.columns.iter().enumerate().map(|(i, col)| {
        let expr = lower_output(i, col, &target_source, &scope, column_meta);
        Output { name: col.name.clone(), expr }
    }).collect();

    let params = described.params.iter().enumerate().map(|(i, t)| {
        let binding = param_bindings.get(&(i + 1)).cloned().map(|table_ref| {
            // The Scope's `aliases` is keyed by the EXPLAIN plan's scan
            // aliases — INSERT/UPDATE target relations aren't aliased
            // there, so we look up the table by name directly through
            // `find_alias` (which finds the matching plan alias, if
            // any) and fall back to the param_nullability tighten.
            let not_null = scope.find_alias(&table_ref.schema, &table_ref.table)
                .and_then(|a| scope.resolve_alias(a))
                .and_then(|t| t.col_not_null(&table_ref.column))
                .unwrap_or(false);
            ResolvedCol { table_ref, alias: String::new(), not_null, typoid: 0 }
        });
        Param { binding, pg_type: t.clone() }
    }).collect();

    Ok(Analyzed { outputs, params })
}

fn lower_output(
    i: usize, col: &crate::describe::DescribedColumn,
    target_source: &TargetSource, scope: &Scope, column_meta: &ColumnMeta,
) -> Expr {
    // First choice: lower the SQL target node 1:1 by position.
    let from_target = match target_source {
        TargetSource::Plain(targets) =>
            targets.get(i).map(|n| lower(n, scope)),
        TargetSource::SetOp(branches) => Some(Expr::SetOp(branches.iter()
            .map(|b| b.get(i).map(|n| lower(n, scope)).unwrap_or(Expr::Unknown))
            .collect())),
        TargetSource::OpaqueStar | TargetSource::Unknown => None,
    };
    if let Some(e) = from_target.filter(|e| !matches!(e, Expr::Unknown)) {
        return e;
    }
    // Fallback: the SQL target didn't slot 1:1 (star expansion, or a
    // shape the lowering didn't recognise). When RowDescription gives
    // us a direct `(table_oid, attnum)`, synthesise `Expr::Column`
    // from the pg_attribute lookup the caller pre-fetched, applying
    // the scope's outer-join widening for the matching alias.
    if let Some(meta) = column_meta.get(&(col.table_oid, col.attnum)) {
        let alias = scope.find_alias(&meta.table_ref.schema, &meta.table_ref.table)
            .unwrap_or("").to_string();
        let widened = !alias.is_empty() && scope.is_nullable_alias(&alias);
        let force_non_null = !alias.is_empty() && scope.is_non_null_alias(&alias);
        let typoid = scope.resolve_alias(&alias)
            .and_then(|t| t.col_typoid(&meta.table_ref.column))
            .unwrap_or(0);
        return Expr::Column(ResolvedCol {
            table_ref: meta.table_ref.clone(),
            alias,
            not_null: (meta.not_null || force_non_null) && !widened,
            typoid,
        });
    }
    Expr::Unknown
}

enum TargetSource {
    /// Plain SELECT / INSERT/UPDATE/DELETE RETURNING.
    Plain(Vec<Node>),
    /// Set-op (UNION / INTERSECT / EXCEPT). Per-branch target lists in
    /// flatten order (matches EXPLAIN's Append children).
    SetOp(Vec<Vec<Node>>),
    /// The target list contains a star — output ordering doesn't match
    /// target_list 1:1.
    OpaqueStar,
    /// Something else — DDL, set-returning function at top level, …
    Unknown,
}

fn collect_target_source(sql: &str) -> TargetSource {
    let Ok(parsed) = pg_query::parse(sql) else { return TargetSource::Unknown };
    let Some(raw) = parsed.protobuf.stmts.into_iter().next() else { return TargetSource::Unknown };
    let Some(boxed) = raw.stmt else { return TargetSource::Unknown };
    let Some(body) = (*boxed).node else { return TargetSource::Unknown };
    let targets = match body {
        NB::SelectStmt(s) if s.op != SETOP_NONE => {
            let mut branches = Vec::new();
            collect_setop_branch(&s, &mut branches);
            return TargetSource::SetOp(branches);
        }
        NB::SelectStmt(s) => s.target_list,
        NB::InsertStmt(ins) => ins.returning_list,
        NB::UpdateStmt(upd) => upd.returning_list,
        NB::DeleteStmt(del) => del.returning_list,
        _ => return TargetSource::Unknown,
    };
    if targets.iter().any(target_contains_star) { return TargetSource::OpaqueStar; }
    TargetSource::Plain(targets.iter().filter_map(res_target_val).collect())
}

fn collect_setop_branch(s: &SelectStmt, out: &mut Vec<Vec<Node>>) {
    if s.op == SETOP_NONE {
        if s.target_list.iter().any(target_contains_star) {
            // Star inside a branch — mark the branch as empty so the
            // per-column lookup defaults to Unknown.
            out.push(Vec::new());
            return;
        }
        out.push(s.target_list.iter().filter_map(res_target_val).collect());
        return;
    }
    if let Some(l) = s.larg.as_deref() { collect_setop_branch(l, out); }
    if let Some(r) = s.rarg.as_deref() { collect_setop_branch(r, out); }
}

fn res_target_val(n: &Node) -> Option<Node> {
    match n.node.as_ref()? {
        NB::ResTarget(rt) => rt.val.as_deref().cloned(),
        _ => None,
    }
}

fn target_contains_star(n: &Node) -> bool {
    let Some(NB::ResTarget(rt)) = n.node.as_ref() else { return false };
    let Some(val) = rt.val.as_deref() else { return false };
    let Some(NB::ColumnRef(cr)) = val.node.as_ref() else { return false };
    cr.fields.iter().any(|f| matches!(f.node.as_ref(), Some(NB::AStar(_))))
}

// ---------- View recursion ----------

/// Find every `RangeVar` in the SQL, check `pg_class` for which are
/// views (`relkind = 'v'`), fetch each view's definition, and
/// recursively analyse it — returning per-view alias → per-column
/// `DerivedColumn`s. Cycles (a view that references itself or sits
/// in a cyclic dependency) are detected via the `visited` OID set and
/// short-circuited: any view OID currently on the analysis stack
/// resolves to no derived columns, leaving the lookup to fall through
/// to `attnotnull` like for an opaque table. Boxed because of async
/// recursion.
fn analyze_view_refs<'a>(
    client: &'a Client,
    sql: &'a str,
    unsafe_casts: &'a HashSet<(u32, u32)>,
    typname_to_oid: &'a HashMap<String, u32>,
    visited: &'a HashSet<u32>,
) -> Pin<Box<dyn std::future::Future<Output = Result<HashMap<String, Vec<DerivedColumn>>>> + Send + 'a>> {
    Box::pin(async move {
        let candidates = find_rangevar_aliases(sql);
        if candidates.is_empty() { return Ok(HashMap::new()); }
        let view_oids = fetch_view_oids(client, &candidates).await;
        if view_oids.is_empty() { return Ok(HashMap::new()); }
        let mut out = HashMap::new();
        for (alias, oid) in view_oids {
            if visited.contains(&oid) { continue; }
            let view_sql = match fetch_view_def(client, oid).await {
                Some(s) => s,
                None => continue,
            };
            let described = match crate::describe::describe(client, &view_sql).await {
                Ok(d) => d,
                Err(e) => { tracing::debug!("describe view {oid}: {e}"); continue; }
            };
            let plan_walk = crate::plan::explain(client, &view_sql).await
                .unwrap_or_default();
            let pairs: Vec<(u32, i16)> = described.columns.iter()
                .filter(|c| c.table_oid != 0 && c.attnum > 0)
                .map(|c| (c.table_oid, c.attnum))
                .collect();
            let column_meta = crate::resolve_column_meta(client, &pairs).await;
            let mut next_visited = visited.clone();
            next_visited.insert(oid);
            let analyzed = build_inner(
                client, &view_sql, &described, plan_walk,
                &column_meta, &HashMap::new(),
                unsafe_casts.clone(), typname_to_oid.clone(),
                &next_visited,
            ).await?;
            let cols: Vec<DerivedColumn> = analyzed.outputs.into_iter()
                .map(|o| DerivedColumn { name: o.name, expr: o.expr })
                .collect();
            out.insert(alias, cols);
        }
        Ok(out)
    })
}

/// `(alias_or_relname, schema, relname)` for every `RangeVar` in the
/// SQL's top-level `from_clause`. We don't yet know which are views;
/// `fetch_view_oids` filters via `pg_class`.
fn find_rangevar_aliases(sql: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let Ok(parsed) = pg_query::parse(sql) else { return out };
    for raw in &parsed.protobuf.stmts {
        let Some(stmt) = raw.stmt.as_deref().and_then(|s| s.node.as_ref()) else { continue };
        let select = match stmt {
            NB::SelectStmt(s) => s,
            _ => continue,
        };
        for from in &select.from_clause {
            walk_rangevars(from, &mut out);
        }
    }
    out
}

fn walk_rangevars(n: &Node, out: &mut Vec<(String, String, String)>) {
    match n.node.as_ref() {
        Some(NB::RangeVar(rv)) => {
            let alias = rv.alias.as_ref().map(|a| a.aliasname.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| rv.relname.clone());
            let schema = rv.schemaname.clone();
            out.push((alias, schema, rv.relname.clone()));
        }
        Some(NB::JoinExpr(je)) => {
            if let Some(l) = je.larg.as_deref() { walk_rangevars(l, out); }
            if let Some(r) = je.rarg.as_deref() { walk_rangevars(r, out); }
        }
        _ => {}
    }
}

/// One round-trip filters the `RangeVar` candidates by
/// `pg_class.relkind = 'v'`. Returns alias → view OID for each view
/// candidate.
async fn fetch_view_oids(
    client: &Client, candidates: &[(String, String, String)],
) -> Vec<(String, u32)> {
    if candidates.is_empty() { return Vec::new() }
    let schemas: Vec<&str> = candidates.iter()
        .map(|(_, s, _)| if s.is_empty() { "public" } else { s.as_str() })
        .collect();
    let names: Vec<&str> = candidates.iter().map(|(_, _, n)| n.as_str()).collect();
    let rows = match client.query(
        r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT ask.schema, ask.name, c.oid::oid
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
        WHERE c.relkind = 'v'
        "#,
        &[&schemas, &names],
    ).await {
        Ok(r) => r,
        Err(e) => { tracing::debug!("fetch_view_oids: {e}"); return Vec::new(); }
    };
    let mut by_name: HashMap<(String, String), u32> = HashMap::new();
    for row in &rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let oid: u32 = row.get(2);
        by_name.insert((schema, name), oid);
    }
    candidates.iter().filter_map(|(alias, schema, name)| {
        let s = if schema.is_empty() { "public".to_string() } else { schema.clone() };
        by_name.get(&(s, name.clone())).map(|oid| (alias.clone(), *oid))
    }).collect()
}

async fn fetch_view_def(client: &Client, oid: u32) -> Option<String> {
    let row = client.query_one("SELECT pg_get_viewdef($1::oid)", &[&oid]).await.ok()?;
    Some(row.get::<_, String>(0))
}

// ---------- Derived tables / CTEs ----------

/// Walk the SQL once and lower every derived-table alias (RangeSubselect
/// in FROM) and every CTE name (WithClause) into per-column `Expr`s.
/// The current `scope` is used to lower expressions — derived tables
/// share the outer scope's tables for simple cases; nested aliases
/// (a derived table over another derived table) are handled by the
/// outer pass's `scope.derived` lookup chain.
fn collect_derived(sql: &str, scope: &Scope) -> HashMap<String, Vec<DerivedColumn>> {
    let mut out = HashMap::new();
    let Ok(parsed) = pg_query::parse(sql) else { return out };
    for raw in &parsed.protobuf.stmts {
        let Some(stmt) = raw.stmt.as_deref().and_then(|s| s.node.as_ref()) else { continue };
        let select = match stmt {
            NB::SelectStmt(s) => s,
            _ => continue,
        };
        // WITH clause CTEs.
        if let Some(wc) = &select.with_clause {
            for cte_node in &wc.ctes {
                let Some(NB::CommonTableExpr(cte)) = cte_node.node.as_ref() else { continue };
                let name = &cte.ctename;
                if name.is_empty() { continue; }
                if let Some(cols) = lower_subquery_columns(cte.ctequery.as_deref(), &cte.aliascolnames, scope) {
                    out.insert(name.clone(), cols);
                }
            }
        }
        // Derived tables in FROM.
        for from in &select.from_clause {
            collect_derived_from(from, scope, &mut out);
        }
    }
    out
}

fn collect_derived_from(
    node: &Node, scope: &Scope, out: &mut HashMap<String, Vec<DerivedColumn>>,
) {
    match node.node.as_ref() {
        Some(NB::RangeSubselect(rs)) => {
            let alias_name = rs.alias.as_ref().map(|a| a.aliasname.clone()).unwrap_or_default();
            if alias_name.is_empty() { return; }
            let alias_colnames: Vec<Node> = rs.alias.as_ref()
                .map(|a| a.colnames.clone()).unwrap_or_default();
            if let Some(cols) = lower_subquery_columns(rs.subquery.as_deref(), &alias_colnames, scope) {
                out.insert(alias_name, cols);
            }
        }
        Some(NB::JoinExpr(je)) => {
            if let Some(l) = je.larg.as_deref() { collect_derived_from(l, scope, out); }
            if let Some(r) = je.rarg.as_deref() { collect_derived_from(r, scope, out); }
        }
        _ => {}
    }
}

/// Lower the per-column outputs of a subquery (a SelectStmt — either
/// plain SELECT, VALUES, or a set-op tree). Returns one
/// `DerivedColumn` per output position. Column names come from
/// `aliascolnames` (the user's `(col1, col2, …)` after the alias) if
/// provided, falling back to the subquery's own target names.
fn lower_subquery_columns(
    subquery: Option<&Node>, aliascolnames: &[Node], scope: &Scope,
) -> Option<Vec<DerivedColumn>> {
    let body = subquery?.node.as_ref()?;
    let select = match body { NB::SelectStmt(s) => s, _ => return None };
    lower_select_columns(select, aliascolnames, scope)
}

fn lower_select_columns(
    select: &SelectStmt, aliascolnames: &[Node], scope: &Scope,
) -> Option<Vec<DerivedColumn>> {
    let alias_names: Vec<Option<String>> = aliascolnames.iter()
        .map(|n| match n.node.as_ref()? {
            NB::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();
    // Set-op (UNION / INTERSECT / EXCEPT) or recursive CTE: the base
    // case (`larg`) sets the floor for column nullability. The
    // recursive branch (or second set-op branch) must align on type
    // and column count, so its verdict can't be wider than the base.
    if select.op != SETOP_NONE {
        return select.larg.as_deref()
            .and_then(|s| lower_select_columns(s, aliascolnames, scope));
    }
    // VALUES (1, 'a'), (2, 'b'). Take the first row.
    if !select.values_lists.is_empty() {
        let first = select.values_lists.first()?;
        let row = match first.node.as_ref()? {
            NB::List(l) => &l.items,
            _ => return None,
        };
        let cols = row.iter().enumerate().map(|(i, expr)| DerivedColumn {
            name: alias_names.get(i).cloned().flatten().unwrap_or_default(),
            expr: lower(expr, scope),
        }).collect();
        return Some(cols);
    }
    // Regular SELECT — pull from target_list.
    let cols = select.target_list.iter().enumerate().filter_map(|(i, t)| {
        let rt = match t.node.as_ref()? { NB::ResTarget(rt) => rt, _ => return None };
        let val = rt.val.as_deref()?;
        let name = alias_names.get(i).cloned().flatten()
            .unwrap_or_else(|| rt.name.clone());
        Some(DerivedColumn { name, expr: lower(val, scope) })
    }).collect();
    Some(cols)
}

/// Three-state verdict from a lowered `Expr`. The downstream
/// `decide_nullability` function combines this with `attnotnull` from
/// RowDescription's base-column refs.
pub fn verdict(expr: &Expr) -> Verdict {
    if lowering::is_nullable(expr) { Verdict::Nullable }
    else if lowering::is_non_null(expr) { Verdict::NotNullable }
    else { Verdict::Unknown }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict { Nullable, NotNullable, Unknown }
