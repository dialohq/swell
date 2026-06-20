//! EXPLAIN VERBOSE FORMAT JSON plan tree — the parts of it we use.
//!
//! We extract only structural facts (scan aliases, outer-join
//! widening, function-scan non-null sources). EXPLAIN's per-node
//! `Output` expression strings are *not* read here — expression-level
//! classification is the SQL AST's job (see `lowering`).

use anyhow::Result;
use pg_query::protobuf::{node::Node as NB, FuncCall};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tokio_postgres::Client;

/// What the plan-tree walk produces. Hands off to `Scope::build`.
#[derive(Debug, Clone, Default)]
pub struct PlanWalk {
    /// scan alias → (schema, relation_name)
    pub alias_to_table: HashMap<String, (String, String)>,
    /// Aliases widened to NULL by an outer join above.
    pub nullable_aliases: HashSet<String>,
    /// Aliases whose every output column is non-null by construction:
    ///   - `Function Scan` over `unnest(<literal-array>)`
    ///   - `Values Scan` over all-literal rows (we assume any VALUES
    ///     we recognise is bare literals — being optimistic)
    pub non_null_aliases: HashSet<String>,
    /// `Some((left, right))` when the topmost effective join is a
    /// FULL OUTER JOIN. Used by codegen's row-variant union.
    pub root_full_join: Option<(HashSet<String>, HashSet<String>)>,
}

pub async fn explain(client: &Client, sql: &str) -> Result<PlanWalk> {
    let stmt = format!("EXPLAIN (VERBOSE, FORMAT JSON, GENERIC_PLAN) {sql}");
    let msgs = client.simple_query(&stmt).await?;
    let json_text = msgs.iter().find_map(|m| match m {
        tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(str::to_string),
        _ => None,
    }).unwrap_or_default();
    let plans: Vec<ExplainEntry> = serde_json::from_str(&json_text).unwrap_or_default();
    let Some(entry) = plans.into_iter().next() else { return Ok(PlanWalk::default()) };
    let plan = entry.plan;

    // The SQL AST is the source of truth for the `unnest(...)` arg
    // shape — we walk it once and pre-collect the set of aliases whose
    // RangeFunction is a literal-array `unnest`. That lets the plan
    // walk decide non-null-ness from the SQL alias without ever
    // re-tokenising EXPLAIN's `Function Call` string.
    let literal_unnest_aliases = literal_unnest_aliases_from_sql(sql);

    Ok(PlanWalk {
        alias_to_table: collect_alias_to_table(&plan),
        nullable_aliases: collect_nullable_aliases(&plan),
        non_null_aliases: collect_non_null_source_aliases(&plan, &literal_unnest_aliases),
        root_full_join: detect_root_full_join(&plan),
    })
}

// ---------- Internal walks ----------

fn walk_plan<F: FnMut(&PlanNode)>(node: &PlanNode, f: &mut F) {
    f(node);
    for c in node.plans.iter().flatten() { walk_plan(c, f); }
}

fn collect_alias_to_table(node: &PlanNode) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    walk_plan(node, &mut |n| {
        if let (Some(alias), Some(rel)) = (&n.alias, &n.relation_name) {
            out.entry(alias.clone()).or_insert((
                n.schema.clone().unwrap_or_default(), rel.clone(),
            ));
        }
    });
    out
}

/// Walk the plan tree and return every alias whose rows can be NULL
/// because of an outer-join above it.
///
///   - LEFT  → mark Inner-side aliases nullable.
///   - RIGHT → mark Outer-side aliases nullable.
///   - FULL  → mark both sides nullable.
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

fn collect_non_null_source_aliases(
    node: &PlanNode, literal_unnest_aliases: &HashSet<String>,
) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_non_null_rec(node, literal_unnest_aliases, &mut out);
    out
}

/// Returns true iff `node` (or its single child chain through
/// `Subquery Scan` wrappers) is a literal-only source whose every
/// output column is non-null. We *also* add `node.alias` to `out`
/// whenever that's the case, so the SQL alias the user wrote (which
/// is on the wrapping `Subquery Scan`, not on the inner `Values Scan`
/// with its synthetic `*VALUES*` name) gets picked up.
fn collect_non_null_rec(
    node: &PlanNode, lit: &HashSet<String>, out: &mut HashSet<String>,
) -> bool {
    let is_source = match node.node_type.as_deref().unwrap_or("") {
        "Function Scan" => node.alias.as_deref()
            .is_some_and(|a| lit.contains(a)),
        "Values Scan" => true,
        _ => false,
    };
    if is_source {
        if let Some(a) = node.alias.as_deref() { out.insert(a.to_string()); }
    }
    let mut subtree_non_null = is_source;
    // Walk children. A passthrough wrapper (Subquery Scan / Result /
    // Sort / Materialize / Limit / Unique / WindowAgg) with a single
    // non-null source child carries the non-null verdict upward —
    // its alias is what the user wrote (`t` in `(VALUES …) AS t`),
    // even though the inner Values Scan has a synthetic `*VALUES*`
    // alias of its own.
    let children = node.plans.as_deref().unwrap_or(&[]);
    let mut all_children_non_null = !children.is_empty();
    for c in children {
        let nn = collect_non_null_rec(c, lit, out);
        if !nn { all_children_non_null = false; }
    }
    let is_passthrough = matches!(node.node_type.as_deref(),
        Some("Subquery Scan" | "Result" | "Sort" | "Incremental Sort"
            | "Materialize" | "Limit" | "Unique" | "WindowAgg"));
    if is_passthrough && all_children_non_null {
        if let Some(a) = node.alias.as_deref() { out.insert(a.to_string()); }
        subtree_non_null = true;
    }
    subtree_non_null
}

fn detect_root_full_join(plan: &PlanNode) -> Option<(HashSet<String>, HashSet<String>)> {
    // Step through known passthrough wrappers to find the topmost join.
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
            if !left.is_empty() && !right.is_empty() { return Some((left, right)); }
            return None;
        }
        let next = unwrap_passthrough(cur);
        if std::ptr::eq(next, cur) { return None; }
        cur = next;
    }
}

fn unwrap_passthrough(node: &PlanNode) -> &PlanNode {
    let is_pass = matches!(node.node_type.as_deref(),
        Some("Subquery Scan" | "Result" | "Sort" | "Incremental Sort"
            | "Materialize" | "Limit" | "Unique" | "WindowAgg"));
    if is_pass {
        if let [child] = node.plans.as_deref().unwrap_or(&[]) {
            return unwrap_passthrough(child);
        }
    }
    node
}

// ---------- SQL-AST side: literal-unnest detection ----------

/// Walk the SQL's `from_clause` for `RangeFunction` entries that call
/// `unnest(ARRAY[…])` or `unnest('{lit}'::T[])` — the AST tells us
/// structurally whether the arg is a literal array constructor or a
/// string literal, no text matching against EXPLAIN's deparse.
fn literal_unnest_aliases_from_sql(sql: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(parsed) = pg_query::parse(sql) else { return out };
    for raw in &parsed.protobuf.stmts {
        let Some(stmt) = raw.stmt.as_deref().and_then(|s| s.node.as_ref()) else { continue };
        let select = match stmt {
            NB::SelectStmt(s) => s,
            _ => continue,
        };
        for from in &select.from_clause {
            collect_unnest(from, &mut out);
        }
    }
    out
}

fn collect_unnest(node: &pg_query::protobuf::Node, out: &mut HashSet<String>) {
    match node.node.as_ref() {
        Some(NB::RangeFunction(rf)) => {
            // RangeFunction.functions is Vec<Node> where each is a List
            // wrapping [FuncCall, …]. Take the first element's FuncCall.
            let alias = rf.alias.as_ref().map(|a| a.aliasname.clone()).unwrap_or_default();
            if alias.is_empty() { return; }
            for outer in &rf.functions {
                let fc = match outer.node.as_ref() {
                    Some(NB::List(l)) => l.items.iter().find_map(|i| match i.node.as_ref()? {
                        NB::FuncCall(fc) => Some(fc.as_ref()),
                        _ => None,
                    }),
                    _ => None,
                };
                let Some(fc) = fc else { continue };
                if funcname_last(fc) == Some("unnest") && unnest_arg_is_literal(fc) {
                    out.insert(alias.clone());
                }
            }
        }
        Some(NB::JoinExpr(je)) => {
            if let Some(l) = je.larg.as_deref() { collect_unnest(l, out); }
            if let Some(r) = je.rarg.as_deref() { collect_unnest(r, out); }
        }
        _ => {}
    }
}

fn funcname_last(fc: &FuncCall) -> Option<&str> {
    fc.funcname.last().and_then(|n| match n.node.as_ref()? {
        NB::String(s) => Some(s.sval.as_str()),
        _ => None,
    })
}

fn unnest_arg_is_literal(fc: &FuncCall) -> bool {
    let Some(arg) = fc.args.first() else { return false };
    let Some(body) = arg.node.as_ref() else { return false };
    match body {
        NB::AArrayExpr(_) => true,
        NB::AConst(c) => matches!(&c.val, Some(pg_query::protobuf::a_const::Val::Sval(_))),
        NB::TypeCast(tc) => tc.arg.as_deref()
            .and_then(|a| a.node.as_ref())
            .is_some_and(|n| matches!(n, NB::AArrayExpr(_) | NB::AConst(_))),
        _ => false,
    }
}

// ---------- Plan tree types ----------

#[derive(Debug, Deserialize)]
struct ExplainEntry {
    #[serde(rename = "Plan")]
    plan: PlanNode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlanNode {
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
}
