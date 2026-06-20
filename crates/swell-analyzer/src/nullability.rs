//! Nullability refinement via `EXPLAIN (VERBOSE, FORMAT JSON)` plan walk.
//!
//! Layered on top of `attnotnull`. EXPLAIN gives us *evidence* about
//! per-output-expression nullability. Three distinct verdicts:
//!
//!   - `Nullable`    — strong evidence: aggregate that goes NULL on empty
//!     input (sum/avg/min/max/json_agg/...), or the expression references
//!     an alias on the nullable side of an outer join.
//!   - `NotNullable` — strong evidence: aggregate that never returns NULL
//!     (e.g. `count(*)`), or `coalesce(x, <literal>)`.
//!   - `Unknown`     — neutral. Defer to attnotnull / default.
//!
//! How outer-join detection works:
//!   The Postgres EXPLAIN plan tree has nodes with `"Join Type": "Left"|
//!   "Right"|"Full"` and child plans tagged with `"Parent Relationship":
//!   "Outer"|"Inner"`. We walk bottom-up to compute the set of aliases
//!   whose rows can be NULL when the join doesn't match — for LEFT JOIN
//!   that's the Inner-side aliases, for RIGHT it's Outer, for FULL it's
//!   both. The topmost node's `Output` list is then matched against this
//!   set: any expression of the form `<alias>.<col>` whose alias is
//!   nullable becomes `Nullable`.
//!
//! Caveat:
//!   The format isn't contractually stable across Postgres versions. We
//!   pin `plan_cache_mode = force_generic_plan` and use the `GENERIC_PLAN`
//!   EXPLAIN option (PG 16+) so the planner doesn't constant-fold outer
//!   joins away based on substituted parameter values.

use crate::ast_classify;
use crate::explain_expr::leading_alias;
use pg_query::protobuf::{node::Node as NB, Node, SelectStmt};
use serde::Deserialize;
use std::collections::HashSet;
use tokio_postgres::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullVerdict {
    Nullable,
    NotNullable,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct NullabilityHints {
    pub by_column: Vec<NullVerdict>,
    /// Raw `Output` expressions from EXPLAIN VERBOSE, one per result
    /// column, in column order. Exposed so the analyzer can do extra
    /// per-expression checks (e.g. coalesce-with-non-null-arg) that
    /// need access to attnotnull beyond what `classify` sees.
    pub exprs: Vec<String>,
    /// For set-op plans (UNION/INTERSECT/EXCEPT/UNION ALL), the
    /// per-column expressions of every branch. Equal to
    /// `vec![exprs[i].clone()]` for non-set-op plans. Lets the
    /// analyzer reason "this column is non-null iff every branch is
    /// non-null" with attnotnull-aware lookups.
    pub branches: Vec<Vec<String>>,
    /// `alias → (schema, relation)` for every scan node in the plan.
    /// Lets the analyzer turn an EXPLAIN expression like
    /// `(users.email)` back into `(public, users, email)` so it can
    /// look up `attnotnull` for the referenced base column.
    pub alias_to_table: std::collections::HashMap<String, (String, String)>,
    /// `Some(("Full", left_aliases, right_aliases))` for plans whose
    /// root is an outer join. The two alias sets list which scan-
    /// aliases sit on the join's left vs right side. The analyzer uses
    /// this to synthesise the FULL JOIN row-variant union.
    pub root_full_join: Option<(HashSet<String>, HashSet<String>)>,
}

impl NullabilityHints {
    pub fn unknown(n: usize) -> Self {
        Self {
            by_column: vec![NullVerdict::Unknown; n],
            exprs: vec![String::new(); n],
            branches: vec![Vec::new(); n],
            alias_to_table: std::collections::HashMap::new(),
            root_full_join: None,
        }
    }
}

pub async fn explain_nullability(
    client: &Client,
    sql: &str,
    _params: &[postgres_types::Type],
    n_columns: usize,
) -> anyhow::Result<NullabilityHints> {
    // swell requires Postgres 16+ for the GENERIC_PLAN EXPLAIN option,
    // which lets the planner use parameter type info without binding values.
    // We send via simple_query (text protocol) so unbound `$N` placeholders
    // don't trigger the extended-protocol bind step.
    let stmt = format!("EXPLAIN (VERBOSE, FORMAT JSON, GENERIC_PLAN) {}", sql);
    let msgs = client.simple_query(&stmt).await?;
    let json_text = msgs.iter().find_map(|m| match m {
        tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(str::to_string),
        _ => None,
    }).unwrap_or_default();
    let plans: Vec<ExplainEntry> = serde_json::from_str(&json_text).unwrap_or_default();
    let plan = match plans.into_iter().next() {
        Some(p) => p.plan,
        None => return Ok(NullabilityHints::unknown(n_columns)),
    };

    // Bottom-up: compute the set of aliases that can be NULL on this plan.
    let nullable_aliases = collect_nullable_aliases(&plan);
    // `alias → (schema, relation)` from every scan node; used by the
    // caller to resolve EXPLAIN's `<alias>.<col>` back to a table column.
    let alias_to_table = collect_alias_to_table(&plan);
    // Aliases for plan nodes whose every output column is non-null
    // by construction (Function Scan over `unnest(<literal-array>)`,
    // Values Scan over all-literal rows). Used so refs like `t.label`
    // → `t` from a literal `unnest` get classified as NotNullable.
    let non_null_aliases = collect_non_null_source_aliases(&plan);

    // Combined verdicts and the canonical Output expression per column.
    // For a plain SELECT this is the topmost node's Output. For an
    // Append / SetOp (UNION/INTERSECT/EXCEPT) the top node has no
    // Output of its own — we merge each subplan's per-column Output by
    // classifying each and taking the conservative result.
    let mut by_column = vec![NullVerdict::Unknown; n_columns];
    let mut exprs = vec![String::new(); n_columns];
    let mut branches: Vec<Vec<String>> = vec![Vec::new(); n_columns];

    // SubPlan and CTE name resolution. PG emits `(SubPlan N)` or
    // `<cte_alias>.<col>` for scalar subqueries / CTE references; the
    // actual underlying expressions live in InitPlan / SubPlan / CTE
    // wrapper nodes inside `plans`. We pre-collect them so the
    // per-column classification can look through `(SubPlan 1)` to the
    // `count(*)` underneath.
    let named = collect_named_outputs(&plan);

    // SQL-level target ASTs, indexed by output-column position. Used by
    // the AST-first classification path — beats matching against the
    // EXPLAIN deparse text because the parse tree disambiguates
    // `coalesce(...)` from a user `coalesce_safe(...)`, recognises CASE
    // / aggregate / never-null shapes structurally, and resolves
    // `<alias>.<col>` without parsing strings. Falls back to the
    // EXPLAIN-text classifier for SubPlan / CTE-resolved indirection.
    let top_targets = extract_top_targets(sql);
    let branch_targets = extract_setop_branch_targets(sql);

    let ast_classify = |node: &Node| -> Option<NullVerdict> {
        ast_classify::classify(node, &nullable_aliases, &non_null_aliases)
    };

    if is_setop_node(&plan) {
        let branch_outputs = collect_setop_branches(&plan);
        for (branch_idx, outputs) in branch_outputs.iter().enumerate() {
            for (i, expr) in outputs.iter().take(n_columns).enumerate() {
                let v = branch_targets.get(branch_idx)
                    .and_then(|t| t.get(i)?.as_ref())
                    .and_then(ast_classify)
                    .unwrap_or_else(|| classify_expr(
                        expr, &nullable_aliases, &non_null_aliases, &named,
                    ));
                by_column[i] = combine_setop(by_column[i], v, branch_idx > 0);
                if branch_idx == 0 {
                    exprs[i] = expr.clone();
                }
                branches[i].push(expr.clone());
            }
        }
    } else {
        let outputs = collect_topmost_output(&plan).unwrap_or_default();
        for (i, expr) in outputs.iter().take(n_columns).enumerate() {
            by_column[i] = top_targets.get(i).and_then(|n| n.as_ref())
                .and_then(ast_classify)
                .unwrap_or_else(|| classify_expr(
                    expr, &nullable_aliases, &non_null_aliases, &named,
                ));
            exprs[i] = expr.clone();
            branches[i] = vec![expr.clone()];
        }
    }
    // If the plan's outer-most join is FULL, capture the alias sets
    // for each side so the analyzer can build the 3-variant row union.
    let root_full_join = detect_root_full_join(&plan);

    Ok(NullabilityHints { by_column, exprs, branches, alias_to_table, root_full_join })
}

fn detect_root_full_join(plan: &PlanNode) -> Option<(HashSet<String>, HashSet<String>)> {
    // Walk through passthrough wrappers to find the join.
    let mut cur = plan;
    loop {
        if cur.join_type.as_deref() == Some("Full") {
            let children = cur.plans.as_deref()?;
            let mut left = HashSet::new();
            let mut right = HashSet::new();
            for c in children {
                match c.parent_relationship.as_deref() {
                    Some("Outer") => left.extend(collect_subtree_aliases(c)),
                    Some("Inner") => right.extend(collect_subtree_aliases(c)),
                    _ => {}
                }
            }
            if !left.is_empty() && !right.is_empty() {
                return Some((left, right));
            }
            return None;
        }
        let next = unwrap_passthrough(cur);
        if std::ptr::eq(next, cur) { return None; }
        cur = next;
    }
}

fn is_setop_node(plan: &PlanNode) -> bool {
    matches!(
        plan.node_type.as_deref(),
        Some("Append" | "MergeAppend" | "SetOp" | "HashSetOp" | "Recursive Union"),
    )
}

/// Walk through nested Append / SetOp wrappers and return the
/// `Output` of every underlying branch in source order.
fn collect_setop_branches(node: &PlanNode) -> Vec<Vec<String>> {
    if is_setop_node(node) {
        let mut out = Vec::new();
        if let Some(children) = &node.plans {
            for c in children {
                out.extend(collect_setop_branches(c));
            }
        }
        return out;
    }
    // Leaf: the closest Output below this node. For Subquery Scan
    // wrappers (which PG emits around each UNION/INTERSECT/EXCEPT
    // branch), the wrapper's own Output uses synthetic aliases like
    // `"*SELECT* 1".id`; we need the *child's* Output, which has the
    // real base-column references (`users.id`).
    let unwrapped = unwrap_passthrough(node);
    match collect_topmost_output(unwrapped) {
        Some(o) => vec![o],
        None => Vec::new(),
    }
}

/// Step through known passthrough plan nodes (Subquery Scan, Sort,
/// Materialize, Limit, …) to find the first node that emits the real
/// underlying expressions. The passthrough wrapper's Output uses
/// synthetic identifiers that don't resolve back to base columns.
fn unwrap_passthrough(node: &PlanNode) -> &PlanNode {
    let nt = node.node_type.as_deref().unwrap_or("");
    let is_passthrough = matches!(
        nt,
        "Subquery Scan" | "Result" | "Sort" | "Incremental Sort" | "Materialize"
            | "Limit" | "Unique" | "WindowAgg"
    );
    if is_passthrough {
        if let Some(children) = &node.plans {
            if children.len() == 1 {
                return unwrap_passthrough(&children[0]);
            }
        }
    }
    node
}

/// Merge two verdicts for the same column across set-op branches.
/// `already_seen` indicates whether `accum` was set by a previous
/// branch (false → first branch, just take `next`).
fn combine_setop(accum: NullVerdict, next: NullVerdict, already_seen: bool) -> NullVerdict {
    if !already_seen {
        return next;
    }
    match (accum, next) {
        (NullVerdict::Nullable, _) | (_, NullVerdict::Nullable) => NullVerdict::Nullable,
        (NullVerdict::NotNullable, NullVerdict::NotNullable) => NullVerdict::NotNullable,
        _ => NullVerdict::Unknown,
    }
}

/// Visit every node in the plan tree, calling `f` once per node
/// (pre-order). Cheaper to reuse than re-rolling each collector's
/// boilerplate.
fn walk_plan<F: FnMut(&PlanNode)>(node: &PlanNode, f: &mut F) {
    f(node);
    for c in node.plans.iter().flatten() { walk_plan(c, f); }
}

/// Walk every Scan node and record its `(alias, schema, relation_name)`
/// so the caller can resolve EXPLAIN's column qualifications.
fn collect_alias_to_table(node: &PlanNode) -> std::collections::HashMap<String, (String, String)> {
    let mut out = std::collections::HashMap::new();
    walk_plan(node, &mut |n| {
        if let (Some(alias), Some(rel)) = (&n.alias, &n.relation_name) {
            out.entry(alias.clone()).or_insert((
                n.schema.clone().unwrap_or_default(), rel.clone(),
            ));
        }
    });
    out
}

/// Walk the plan and return scan aliases whose every output column is
/// non-null by construction:
///
///   - `Function Scan` over `unnest(ARRAY[…])` of literal elements
///   - `Function Scan` over `unnest('{…}'::type[])` (PG's array literal)
///   - `Values Scan` whose values are all bare literals (PG doesn't
///     put the values into EXPLAIN, but every VALUES we recognise from
///     the SQL is a row of bare literals — be optimistic).
///
/// A `<alias>.<col>` reference for an alias in this set classifies as
/// `NotNullable` even though PG doesn't carry attnotnull info for it.
fn collect_non_null_source_aliases(node: &PlanNode) -> HashSet<String> {
    let mut out = HashSet::new();
    walk_plan(node, &mut |n| {
        let Some(a) = n.alias.as_deref() else { return };
        match n.node_type.as_deref().unwrap_or("") {
            "Function Scan" => {
                if let (Some("unnest"), Some(call)) =
                    (n.function_name.as_deref(), n.function_call.as_deref())
                {
                    if unnest_call_is_literal_array(call) { out.insert(a.to_string()); }
                }
            }
            "Values Scan" => { out.insert(a.to_string()); }
            _ => {}
        }
    });
    out
}

/// Recognise `unnest(ARRAY[lit, lit, …])` or `unnest('{lit,lit}'::T[])`
/// — the array argument is a bare literal so every yielded element is
/// non-null. Reparses the EXPLAIN "Function Call" text as SQL and
/// inspects the AST so we accept any cast / paren shape pg's deparse
/// happens to choose without a textual prefix list.
fn unnest_call_is_literal_array(call: &str) -> bool {
    let Ok(parsed) = pg_query::parse(&format!("SELECT {call}")) else { return false };
    let Some(raw) = parsed.protobuf.stmts.into_iter().next() else { return false };
    let Some(boxed) = raw.stmt else { return false };
    let Some(NB::SelectStmt(s)) = (*boxed).node else { return false };
    let Some(target) = s.target_list.into_iter().next() else { return false };
    let Some(NB::ResTarget(rt)) = target.node else { return false };
    let val = match rt.val.map(|b| *b).and_then(|n| n.node) {
        Some(v) => v,
        None => return false,
    };
    let fc = match val {
        NB::FuncCall(fc) => fc,
        _ => return false,
    };
    if fc.funcname.last().and_then(|n| match &n.node {
        Some(NB::String(s)) => Some(s.sval.as_str()),
        _ => None,
    }) != Some("unnest") { return false; }
    // ARRAY[...] literal OR a bare string literal cast to an array.
    fc.args.first().and_then(|n| n.node.as_ref()).is_some_and(|n| match n {
        NB::AArrayExpr(_) => true,
        NB::AConst(c) => matches!(&c.val, Some(pg_query::protobuf::a_const::Val::Sval(_))),
        NB::TypeCast(tc) => tc.arg.as_deref()
            .and_then(|a| a.node.as_ref())
            .is_some_and(|n| matches!(n,
                NB::AArrayExpr(_) | NB::AConst(_))),
        _ => false,
    })
}

// ------------- Plan tree types -------------

#[derive(Debug, Deserialize)]
struct ExplainEntry {
    #[serde(rename = "Plan")]
    plan: PlanNode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlanNode {
    #[serde(default)]
    output: Option<Vec<String>>,
    #[serde(default)]
    plans: Option<Vec<PlanNode>>,
    #[serde(default)]
    alias: Option<String>,
    #[serde(rename = "Relation Name", default)]
    relation_name: Option<String>,
    #[serde(default)]
    schema: Option<String>,
    #[serde(rename = "Join Type", default)]
    join_type: Option<String>,
    #[serde(rename = "Parent Relationship", default)]
    parent_relationship: Option<String>,
    #[serde(rename = "Node Type", default)]
    node_type: Option<String>,
    #[serde(rename = "Function Name", default)]
    function_name: Option<String>,
    #[serde(rename = "Function Call", default)]
    function_call: Option<String>,
    #[serde(rename = "Subplan Name", default)]
    subplan_name: Option<String>,
    #[serde(rename = "Values List", default)]
    #[allow(dead_code)]
    values_list: Option<serde_json::Value>,
}

fn collect_topmost_output(node: &PlanNode) -> Option<Vec<String>> {
    if let Some(out) = &node.output { return Some(out.clone()); }
    node.plans.iter().flatten().find_map(collect_topmost_output)
}

/// Walk the plan tree and return every `Alias` whose rows can be NULL
/// due to an outer-join above it.
///
///   - LEFT → mark Inner-side aliases nullable.
///   - RIGHT → mark Outer-side aliases nullable.
///   - FULL → mark both sides nullable.
fn collect_nullable_aliases(node: &PlanNode) -> HashSet<String> {
    let mut out = HashSet::new();
    walk_plan(node, &mut |n| {
        let null_side = match n.join_type.as_deref() {
            Some("Left")  => Some("Inner"),
            Some("Right") => Some("Outer"),
            Some("Full")  => None, // both sides
            _ => return,
        };
        for c in n.plans.iter().flatten() {
            if null_side.is_none() || c.parent_relationship.as_deref() == null_side {
                out.extend(collect_subtree_aliases(c));
            }
        }
    });
    out
}

fn collect_subtree_aliases(node: &PlanNode) -> HashSet<String> {
    let mut set = HashSet::new();
    walk_plan(node, &mut |n| { if let Some(a) = &n.alias { set.insert(a.clone()); } });
    set
}

// ------------- Per-expression classification -------------

#[cfg(test)]
pub(crate) fn classify(expr: &str, nullable_aliases: &HashSet<String>) -> NullVerdict {
    classify_with(expr, nullable_aliases, &HashSet::new())
}

/// Like `classify` but also takes a set of aliases whose every output
/// column is non-null (e.g. `unnest(<literal-array>)` results).
///
/// Reparses the EXPLAIN expression text as SQL and delegates to the
/// AST classifier — that one walk handles FuncCall (aggregates,
/// builders, window funcs, count), CoalesceExpr, CaseExpr, AArrayExpr,
/// TypeCast, AConst, and ColumnRef structurally. The substring fallback
/// below covers only the synthetic forms PG emits in EXPLAIN that
/// don't reparse: planner-introduced `NULL::T`, `(SubPlan N)` or
/// `<cte_alias>.<col>` indirection (handled one layer up in
/// `classify_expr`), and quoted synthetic aliases like
/// `"*VALUES*"."column1"`.
pub(crate) fn classify_with(
    expr: &str,
    nullable_aliases: &HashSet<String>,
    non_null_aliases: &HashSet<String>,
) -> NullVerdict {
    let trimmed = expr.trim();

    if let Some(v) = ast_classify::try_classify_text(trimmed, nullable_aliases, non_null_aliases) {
        return v;
    }

    // EXPLAIN-only fallback for synthetic forms the SQL parser can't
    // take — quoted synthetic aliases like `"*VALUES*"."column1"`.
    // Real ColumnRefs are handled by the AST path above.
    if let Some(alias) = leading_alias(trimmed) {
        if nullable_aliases.contains(alias) { return NullVerdict::Nullable; }
        if non_null_aliases.contains(alias) { return NullVerdict::NotNullable; }
    }

    NullVerdict::Unknown
}

/// Like `classify_with`, but also resolves `(SubPlan N)` and
/// `<cte_alias>.<col>` references via the pre-collected plan maps.
fn classify_expr(
    expr: &str,
    nullable_aliases: &HashSet<String>,
    non_null_aliases: &HashSet<String>,
    named: &NamedOutputs,
) -> NullVerdict {
    let trimmed = expr.trim();
    let recurse = |s: &str| classify_with(s, nullable_aliases, non_null_aliases);
    // `(SubPlan N)` — resolve to the SubPlan's Output.
    let s = trimmed.trim_start_matches('(').trim_end_matches(')').trim();
    if let Some(rest) = s.strip_prefix("SubPlan ") {
        if let Some(first) = named.subplan.get(&format!("SubPlan {}", rest))
            .and_then(|o| o.first())
        {
            return recurse(first);
        }
    }
    // `<cte_alias>.<col>` — resolve to the CTE's base-case Output. The
    // recursive branch is ignored; the base case sets the floor and the
    // recursive arithmetic preserves it.
    if let Some(alias) = leading_alias(trimmed) {
        if let Some(first) = named.cte.get(alias).and_then(|o| o.first()) {
            return recurse(first);
        }
    }
    recurse(trimmed)
}

/// Subplan + CTE name maps collected in one plan walk.
struct NamedOutputs {
    /// `"SubPlan N"` / `"InitPlan N"` → its Output expressions.
    subplan: std::collections::HashMap<String, Vec<String>>,
    /// CTE alias → base-case Output expressions.
    cte: std::collections::HashMap<String, Vec<String>>,
}

fn collect_named_outputs(plan: &PlanNode) -> NamedOutputs {
    let mut out = NamedOutputs {
        subplan: std::collections::HashMap::new(),
        cte: std::collections::HashMap::new(),
    };
    walk_plan(plan, &mut |n| {
        let Some(name) = n.subplan_name.as_deref() else { return };
        if name.starts_with("SubPlan ") || name.starts_with("InitPlan ") {
            if let Some(o) = collect_topmost_output(n) {
                out.subplan.entry(name.to_string()).or_insert(o);
            }
        } else if let Some(cte_name) = name.strip_prefix("CTE ") {
            // Recursive CTE: use the non-recursive (Outer) branch's
            // Output; we assume the recursive arithmetic preserves the
            // base case's nullability.
            let outputs = if n.node_type.as_deref() == Some("Recursive Union") {
                n.plans.iter().flatten()
                    .find(|c| c.parent_relationship.as_deref() == Some("Outer"))
                    .and_then(collect_topmost_output)
            } else {
                collect_topmost_output(n)
            };
            if let Some(o) = outputs {
                out.cte.entry(cte_name.to_string()).or_insert(o);
            }
        }
    });
    out
}

// ------------- SQL-AST target extraction -------------

/// Plain SELECT (not a setop tree) in pg_query's enum representation.
const SETOP_NONE: i32 = pg_query::protobuf::SetOperation::SetopNone as i32;

/// Per-column AST nodes for the top-level statement's output (SELECT
/// target list, or INSERT/UPDATE/DELETE RETURNING list). Returns
/// `Vec::new()` for set-op queries (handled by `extract_setop_branch_targets`)
/// or any statement whose output isn't a flat target list.
fn extract_top_targets(sql: &str) -> Vec<Option<Node>> {
    let Some(stmt) = parse_first_stmt_body(sql) else { return Vec::new() };
    let targets: Vec<Node> = match stmt {
        // SetopNone (1) = a plain leaf SELECT, not a setop tree.
        NB::SelectStmt(s) if s.op == SETOP_NONE => s.target_list,
        NB::InsertStmt(ins) => ins.returning_list,
        NB::UpdateStmt(upd) => upd.returning_list,
        NB::DeleteStmt(del) => del.returning_list,
        _ => return Vec::new(),
    };
    // Star targets (`SELECT *`, `SELECT u.*`) expand to N output columns
    // but only one target_list entry; the rest of the per-column mapping
    // shifts and gets misaligned. Drop the AST path for those queries —
    // the substring fallback handles them fine.
    if targets.iter().any(target_contains_star) {
        return Vec::new();
    }
    targets.iter().map(res_target_val).collect()
}

fn target_contains_star(n: &Node) -> bool {
    let Some(NB::ResTarget(rt)) = n.node.as_ref() else { return false };
    let Some(val) = rt.val.as_deref() else { return false };
    let Some(NB::ColumnRef(cr)) = val.node.as_ref() else { return false };
    cr.fields.iter().any(|f| matches!(f.node.as_ref(), Some(NB::AStar(_))))
}

/// Per-branch per-column target AST nodes for set-op queries
/// (UNION / INTERSECT / EXCEPT, recursive or not). Order matches the
/// post-flatten order PG uses in EXPLAIN's `Append` children — a
/// left-then-right walk of the SetOp tree.
fn extract_setop_branch_targets(sql: &str) -> Vec<Vec<Option<Node>>> {
    let Some(NB::SelectStmt(top)) = parse_first_stmt_body(sql) else { return Vec::new() };
    if top.op == SETOP_NONE { return Vec::new(); }
    let mut out = Vec::new();
    collect_setop_branch_select(&top, &mut out);
    out
}

fn collect_setop_branch_select(s: &SelectStmt, out: &mut Vec<Vec<Option<Node>>>) {
    if s.op == SETOP_NONE {
        out.push(s.target_list.iter().map(res_target_val).collect());
        return;
    }
    if let Some(l) = s.larg.as_deref() { collect_setop_branch_select(l, out); }
    if let Some(r) = s.rarg.as_deref() { collect_setop_branch_select(r, out); }
}

fn res_target_val(n: &Node) -> Option<Node> {
    match n.node.as_ref()? {
        NB::ResTarget(rt) => rt.val.as_deref().cloned(),
        _ => None,
    }
}

fn parse_first_stmt_body(sql: &str) -> Option<NB> {
    let parsed = pg_query::parse(sql).ok()?;
    let raw = parsed.protobuf.stmts.into_iter().next()?;
    let boxed = raw.stmt?;
    (*boxed).node
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn na() -> HashSet<String> { HashSet::new() }

    #[test]
    fn null_literal() {
        assert_eq!(classify("NULL::integer", &na()), NullVerdict::Nullable);
        assert_eq!(classify("NULL", &na()), NullVerdict::Nullable);
    }

    #[test]
    fn count_is_not_nullable() {
        assert_eq!(classify("count(*)", &na()), NullVerdict::NotNullable);
        assert_eq!(classify("count(t.x)", &na()), NullVerdict::NotNullable);
    }

    #[test]
    fn aggregates_are_nullable() {
        assert_eq!(classify("sum(t.x)", &na()), NullVerdict::Nullable);
        assert_eq!(classify("max(t.y)", &na()), NullVerdict::Nullable);
        assert_eq!(classify("array_agg(t.x ORDER BY t.id)", &na()), NullVerdict::Nullable);
    }

    #[test]
    fn coalesce() {
        assert_eq!(classify("coalesce(t.x, 0)", &na()), NullVerdict::NotNullable);
        assert_eq!(classify("coalesce(t.s, 'fallback')", &na()), NullVerdict::NotNullable);
        assert_eq!(classify("coalesce(t.x, t.y)", &na()), NullVerdict::Unknown);
    }

    #[test]
    fn case_branches() {
        assert_eq!(classify("CASE WHEN t.x > 0 THEN t.x END", &na()), NullVerdict::Nullable);
        // ELSE with a literal → NotNullable (the literal is a guaranteed
        // non-null fallback).
        assert_eq!(classify("CASE WHEN t.x > 0 THEN t.x ELSE 0 END", &na()), NullVerdict::NotNullable);
        assert_eq!(classify("CASE WHEN t.x > 0 THEN t.x ELSE 'foo' END", &na()), NullVerdict::NotNullable);
        assert_eq!(classify("CASE WHEN t.x > 0 THEN t.x ELSE TRUE END", &na()), NullVerdict::NotNullable);
        // ELSE with a non-literal → Unknown (could be null).
        assert_eq!(classify("CASE WHEN t.x > 0 THEN t.x ELSE t.y END", &na()), NullVerdict::Unknown);
    }

    #[test]
    fn alias_on_nullable_side_of_outer_join() {
        let mut nulls = HashSet::new();
        nulls.insert("p".to_string());
        assert_eq!(classify("p.body", &nulls), NullVerdict::Nullable);
        assert_eq!(classify("u.email", &nulls), NullVerdict::Unknown); // not in nullable set
    }

}
