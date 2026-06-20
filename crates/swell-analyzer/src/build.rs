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
use std::collections::HashMap;
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
) -> Result<Analyzed> {
    // Scope is the alias-resolution + nullability environment shared
    // across the whole lowering pass for this statement.
    let mut scope = Scope::build(
        client,
        plan.alias_to_table.clone(),
        plan.nullable_aliases.clone(),
        plan.non_null_aliases.clone(),
    ).await?;

    // Derived tables (RangeSubselect in FROM) and CTEs lowered into
    // per-column `Expr`s so `ColumnRef("<derived>", "col")` resolves
    // structurally instead of bailing to `Unknown`.
    scope.attach_derived(collect_derived(sql, &scope));

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
            ResolvedCol { table_ref, alias: String::new(), not_null }
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
        return Expr::Column(ResolvedCol {
            table_ref: meta.table_ref.clone(),
            alias,
            not_null: (meta.not_null || force_non_null) && !widened,
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
