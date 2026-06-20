//! Top-level `Analyzed` builder. PARSE/DESCRIBE + EXPLAIN + SQL parse
//! tree → fully lowered `Analyzed` in one pass.

use crate::analyzed::{Analyzed, Expr, Output, Param, ResolvedCol};
use crate::describe::{self, DescribedQuery};
use crate::lowering::{self, lower};
use crate::pg_util::{norm_schema, range_var_alias, select_stmts, string_parts, walk_from_tree};
use crate::plan::{self, PlanWalk};
use crate::query::TableColRef;
use crate::scope::{DerivedColumn, Scope};
use crate::{column_pairs, resolve_column_meta};
use anyhow::Result;
use pg_query::protobuf::{node::Node as NB, Node, SelectStmt};
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use tokio_postgres::Client;

const SETOP_NONE: i32 = pg_query::protobuf::SetOperation::SetopNone as i32;

/// `(table_oid, attnum) → resolved base column`. Pre-fetched by the
/// caller from `pg_attribute` in one round trip.
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
    visited: &HashSet<u32>,
) -> Result<Analyzed> {
    let mut scope = Scope::build(
        client,
        plan.alias_to_table.clone(),
        plan.nullable_aliases.clone(),
        plan.non_null_aliases.clone(),
        unsafe_casts.clone(),
        typname_to_oid.clone(),
    )
    .await?;

    // Attach view-derived columns first so CTE / RangeSubselect
    // lowering can resolve view refs.
    let mut derived =
        analyze_view_refs(client, sql, &unsafe_casts, &typname_to_oid, visited).await?;
    scope.attach_derived(derived.clone());
    for (k, v) in collect_derived(sql, &scope) {
        derived.entry(k).or_insert(v);
    }
    scope.attach_derived(derived);

    let target_source = collect_target_source(sql);

    let outputs = described
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| Output {
            name: col.name.clone(),
            expr: lower_output(i, col, &target_source, &scope, column_meta),
        })
        .collect();

    let params = described
        .params
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let binding = param_bindings.get(&(i + 1)).cloned().map(|table_ref| {
                // INSERT/UPDATE targets aren't aliased in the plan walk —
                // look them up via `find_alias` and fall back to the
                // param_nullability tighten otherwise.
                let not_null = scope
                    .find_alias(&table_ref.schema, &table_ref.table)
                    .and_then(|a| scope.resolve_alias(a))
                    .and_then(|t| t.col_not_null(&table_ref.column))
                    .unwrap_or(false);
                ResolvedCol {
                    table_ref,
                    alias: String::new(),
                    not_null,
                    typoid: 0,
                }
            });
            Param {
                binding,
                pg_type: t.clone(),
            }
        })
        .collect();

    Ok(Analyzed { outputs, params })
}

fn lower_output(
    i: usize,
    col: &describe::DescribedColumn,
    target_source: &TargetSource,
    scope: &Scope,
    column_meta: &ColumnMeta,
) -> Expr {
    let from_target = match target_source {
        TargetSource::Plain(targets) => targets.get(i).map(|n| lower(n, scope)),
        TargetSource::SetOp(branches) => Some(Expr::SetOp(
            branches
                .iter()
                .map(|b| b.get(i).map(|n| lower(n, scope)).unwrap_or(Expr::Unknown))
                .collect(),
        )),
        TargetSource::OpaqueStar | TargetSource::Unknown => None,
    };
    if let Some(e) = from_target.filter(|e| !matches!(e, Expr::Unknown)) {
        return e;
    }
    // Fallback: star expansion / shape we didn't recognise. Synthesise
    // `Expr::Column` from RowDescription's (table_oid, attnum) plus
    // the scope's pre-fetched attnotnull, applying outer-join widening.
    if let Some(meta) = column_meta.get(&(col.table_oid, col.attnum)) {
        let alias = scope
            .find_alias(&meta.table_ref.schema, &meta.table_ref.table)
            .unwrap_or("")
            .to_string();
        let widened = !alias.is_empty() && scope.is_nullable_alias(&alias);
        let force_nn = !alias.is_empty() && scope.is_non_null_alias(&alias);
        let typoid = scope
            .resolve_alias(&alias)
            .and_then(|t| t.col_typoid(&meta.table_ref.column))
            .unwrap_or(0);
        return Expr::Column(ResolvedCol {
            table_ref: meta.table_ref.clone(),
            alias,
            not_null: (meta.not_null || force_nn) && !widened,
            typoid,
        });
    }
    Expr::Unknown
}

enum TargetSource {
    Plain(Vec<Node>),
    /// Per-branch target lists in EXPLAIN-flatten order.
    SetOp(Vec<Vec<Node>>),
    /// Target list contains a star — output ordering doesn't match 1:1.
    OpaqueStar,
    Unknown,
}

fn collect_target_source(sql: &str) -> TargetSource {
    let body = pg_query::parse(sql)
        .ok()
        .and_then(|p| p.protobuf.stmts.into_iter().next())
        .and_then(|raw| raw.stmt)
        .and_then(|boxed| boxed.node);
    let Some(body) = body else {
        return TargetSource::Unknown;
    };
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
    if targets.iter().any(target_contains_star) {
        return TargetSource::OpaqueStar;
    }
    TargetSource::Plain(targets.iter().filter_map(res_target_val).collect())
}

fn collect_setop_branch(s: &SelectStmt, out: &mut Vec<Vec<Node>>) {
    if s.op == SETOP_NONE {
        // Star inside a branch — empty branch defaults to Unknown.
        if s.target_list.iter().any(target_contains_star) {
            out.push(Vec::new());
        } else {
            out.push(s.target_list.iter().filter_map(res_target_val).collect());
        }
        return;
    }
    if let Some(l) = s.larg.as_deref() {
        collect_setop_branch(l, out);
    }
    if let Some(r) = s.rarg.as_deref() {
        collect_setop_branch(r, out);
    }
}

fn res_target_val(n: &Node) -> Option<Node> {
    match n.node.as_ref()? {
        NB::ResTarget(rt) => rt.val.as_deref().cloned(),
        _ => None,
    }
}

#[rustfmt::skip]
fn target_contains_star(n: &Node) -> bool {
    let Some(NB::ResTarget(rt)) = n.node.as_ref() else { return false };
    let Some(val) = rt.val.as_deref() else { return false };
    let Some(NB::ColumnRef(cr)) = val.node.as_ref() else { return false };
    cr.fields.iter().any(|f| matches!(f.node.as_ref(), Some(NB::AStar(_))))
}

// ---------- View recursion ----------

/// View RangeVars get their `pg_get_viewdef` recursively analysed.
/// Cycles short-circuit via the `visited` OID set — a view currently
/// on the analysis stack resolves to no derived columns, letting the
/// lookup fall through to `attnotnull`. Boxed for async recursion.
fn analyze_view_refs<'a>(
    client: &'a Client,
    sql: &'a str,
    unsafe_casts: &'a HashSet<(u32, u32)>,
    typname_to_oid: &'a HashMap<String, u32>,
    visited: &'a HashSet<u32>,
) -> Pin<
    Box<dyn std::future::Future<Output = Result<HashMap<String, Vec<DerivedColumn>>>> + Send + 'a>,
> {
    Box::pin(async move {
        let candidates = find_rangevar_aliases(sql);
        if candidates.is_empty() {
            return Ok(HashMap::new());
        }
        let mut out = HashMap::new();
        for (alias, oid) in fetch_view_oids(client, &candidates).await {
            if visited.contains(&oid) {
                continue;
            }
            let Some(view_sql) = fetch_view_def(client, oid).await else {
                continue;
            };
            let described = match describe::describe(client, &view_sql).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::debug!("describe view {oid}: {e}");
                    continue;
                }
            };
            let plan_walk = plan::explain(client, &view_sql).await.unwrap_or_default();
            let column_meta = resolve_column_meta(client, &column_pairs(&described)).await;
            let mut next_visited = visited.clone();
            next_visited.insert(oid);
            let analyzed = build(
                client,
                &view_sql,
                &described,
                plan_walk,
                &column_meta,
                &HashMap::new(),
                unsafe_casts.clone(),
                typname_to_oid.clone(),
                &next_visited,
            )
            .await?;
            let cols = analyzed
                .outputs
                .into_iter()
                .map(|o| DerivedColumn {
                    name: o.name,
                    expr: o.expr,
                })
                .collect();
            out.insert(alias, cols);
        }
        Ok(out)
    })
}

fn find_rangevar_aliases(sql: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let Ok(parsed) = pg_query::parse(sql) else {
        return out;
    };
    for select in select_stmts(&parsed.protobuf) {
        for from in &select.from_clause {
            walk_from_tree(from, &mut |n| {
                if let Some(NB::RangeVar(rv)) = n.node.as_ref() {
                    out.push((
                        range_var_alias(rv),
                        rv.schemaname.clone(),
                        rv.relname.clone(),
                    ));
                }
            });
        }
    }
    out
}

/// Filter `RangeVar` candidates by `pg_class.relkind = 'v'`. Returns
/// alias → view OID pairs for matches.
async fn fetch_view_oids(
    client: &Client,
    candidates: &[(String, String, String)],
) -> Vec<(String, u32)> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let schemas: Vec<&str> = candidates.iter().map(|(_, s, _)| norm_schema(s)).collect();
    let names: Vec<&str> = candidates.iter().map(|(_, _, n)| n.as_str()).collect();
    let rows = match client
        .query(
            r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT ask.schema, ask.name, c.oid::oid
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
        WHERE c.relkind = 'v'
        "#,
            &[&schemas, &names],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("fetch_view_oids: {e}");
            return Vec::new();
        }
    };
    let by_name: HashMap<(String, String), u32> = rows
        .iter()
        .map(|r| ((r.get(0), r.get(1)), r.get(2)))
        .collect();
    candidates
        .iter()
        .filter_map(|(alias, schema, name)| {
            by_name
                .get(&(norm_schema(schema).to_string(), name.clone()))
                .map(|oid| (alias.clone(), *oid))
        })
        .collect()
}

async fn fetch_view_def(client: &Client, oid: u32) -> Option<String> {
    client
        .query_one("SELECT pg_get_viewdef($1::oid)", &[&oid])
        .await
        .ok()
        .map(|row| row.get::<_, String>(0))
}

// ---------- Derived tables / CTEs ----------

fn collect_derived(sql: &str, scope: &Scope) -> HashMap<String, Vec<DerivedColumn>> {
    let mut out = HashMap::new();
    let Ok(parsed) = pg_query::parse(sql) else {
        return out;
    };
    for select in select_stmts(&parsed.protobuf) {
        if let Some(wc) = &select.with_clause {
            for cte_node in &wc.ctes {
                let Some(NB::CommonTableExpr(cte)) = cte_node.node.as_ref() else {
                    continue;
                };
                if cte.ctename.is_empty() {
                    continue;
                }
                if let Some(cols) =
                    lower_subquery_columns(cte.ctequery.as_deref(), &cte.aliascolnames, scope)
                {
                    out.insert(cte.ctename.clone(), cols);
                }
            }
        }
        for from in &select.from_clause {
            walk_from_tree(from, &mut |n| {
                let Some(NB::RangeSubselect(rs)) = n.node.as_ref() else {
                    return;
                };
                let alias = match rs.alias.as_ref() {
                    Some(a) if !a.aliasname.is_empty() => a,
                    _ => return,
                };
                if let Some(cols) =
                    lower_subquery_columns(rs.subquery.as_deref(), &alias.colnames, scope)
                {
                    out.insert(alias.aliasname.clone(), cols);
                }
            });
        }
    }
    out
}

fn lower_subquery_columns(
    subquery: Option<&Node>,
    aliascolnames: &[Node],
    scope: &Scope,
) -> Option<Vec<DerivedColumn>> {
    let NB::SelectStmt(select) = subquery?.node.as_ref()? else {
        return None;
    };
    lower_select_columns(select, aliascolnames, scope)
}

fn lower_select_columns(
    select: &SelectStmt,
    aliascolnames: &[Node],
    scope: &Scope,
) -> Option<Vec<DerivedColumn>> {
    let alias_names = string_parts(aliascolnames);
    let alias_at = |i: usize| alias_names.get(i).cloned().unwrap_or_default();
    // Set-op or recursive CTE: base case (`larg`) sets the floor.
    if select.op != SETOP_NONE {
        return select
            .larg
            .as_deref()
            .and_then(|s| lower_select_columns(s, aliascolnames, scope));
    }
    // VALUES — first row's per-column expr.
    if !select.values_lists.is_empty() {
        let first = select.values_lists.first()?;
        let NB::List(l) = first.node.as_ref()? else {
            return None;
        };
        return Some(
            l.items
                .iter()
                .enumerate()
                .map(|(i, expr)| DerivedColumn {
                    name: alias_at(i),
                    expr: lower(expr, scope),
                })
                .collect(),
        );
    }
    Some(
        select
            .target_list
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let NB::ResTarget(rt) = t.node.as_ref()? else {
                    return None;
                };
                let val = rt.val.as_deref()?;
                let name = alias_names
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| rt.name.clone());
                Some(DerivedColumn {
                    name,
                    expr: lower(val, scope),
                })
            })
            .collect(),
    )
}

pub fn verdict(expr: &Expr) -> Verdict {
    if lowering::is_nullable(expr) {
        Verdict::Nullable
    } else if lowering::is_non_null(expr) {
        Verdict::NotNullable
    } else {
        Verdict::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Nullable,
    NotNullable,
    Unknown,
}
