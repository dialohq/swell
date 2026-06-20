//! EXPLAIN VERBOSE FORMAT JSON plan walk. We extract only structural
//! facts (scan aliases, outer-join widening, function-scan non-null
//! sources). Per-node `Output` expression strings are *not* read —
//! expression classification is the SQL AST's job (see `lowering`).

use anyhow::Result;
use pg_query::protobuf::{node::Node as NB, FuncCall};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tokio_postgres::Client;

#[derive(Debug, Clone, Default)]
pub struct PlanWalk {
    /// scan alias → (schema, relation_name)
    pub alias_to_table: HashMap<String, (String, String)>,
    /// Aliases widened to NULL by an outer join above.
    pub nullable_aliases: HashSet<String>,
    /// Aliases whose every output is non-null by construction
    /// (`Function Scan` over literal `unnest`, `Values Scan`).
    pub non_null_aliases: HashSet<String>,
    /// `(left, right)` alias sets when the topmost join is FULL.
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

    // SQL AST decides which `unnest(...)` calls have literal args —
    // EXPLAIN's `Function Call` string would otherwise need re-tokenising.
    let literal_unnest = literal_unnest_aliases_from_sql(sql);

    Ok(PlanWalk {
        alias_to_table: collect_alias_to_table(&plan),
        nullable_aliases: collect_nullable_aliases(&plan),
        non_null_aliases: collect_non_null_source_aliases(&plan, &literal_unnest),
        root_full_join: detect_root_full_join(&plan),
    })
}

// ---------- Plan tree walks ----------

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

/// LEFT → Inner-side aliases nullable. RIGHT → Outer. FULL → both.
fn collect_nullable_aliases(node: &PlanNode) -> HashSet<String> {
    let mut out = HashSet::new();
    walk_plan(node, &mut |n| {
        let null_side = match n.join_type.as_deref() {
            Some("Left")  => Some("Inner"),
            Some("Right") => Some("Outer"),
            Some("Full")  => None,
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
    node: &PlanNode, literal_unnest: &HashSet<String>,
) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_non_null_rec(node, literal_unnest, &mut out);
    out
}

/// Returns true iff `node` (or its passthrough chain) yields only
/// non-null outputs. Also adds the node's alias to `out` in that case,
/// so the user-written wrapper alias (`t` in `(VALUES …) AS t`) gets
/// picked up even when the inner `Values Scan` is named `*VALUES*`.
fn collect_non_null_rec(
    node: &PlanNode, lit: &HashSet<String>, out: &mut HashSet<String>,
) -> bool {
    let is_source = match node.node_type.as_deref().unwrap_or("") {
        "Function Scan" => node.alias.as_deref().is_some_and(|a| lit.contains(a)),
        "Values Scan" => true,
        _ => false,
    };
    if is_source {
        if let Some(a) = node.alias.as_deref() { out.insert(a.to_string()); }
    }
    let children = node.plans.as_deref().unwrap_or(&[]);
    let mut all_non_null = !children.is_empty();
    for c in children {
        if !collect_non_null_rec(c, lit, out) { all_non_null = false; }
    }
    let is_passthrough = matches!(node.node_type.as_deref(),
        Some("Subquery Scan" | "Result" | "Sort" | "Incremental Sort"
            | "Materialize" | "Limit" | "Unique" | "WindowAgg"));
    if is_passthrough && all_non_null {
        if let Some(a) = node.alias.as_deref() { out.insert(a.to_string()); }
        return true;
    }
    is_source
}

fn detect_root_full_join(plan: &PlanNode) -> Option<(HashSet<String>, HashSet<String>)> {
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
            return (!left.is_empty() && !right.is_empty()).then_some((left, right));
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

// ---------- SQL AST: literal-unnest detection ----------

fn literal_unnest_aliases_from_sql(sql: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(parsed) = pg_query::parse(sql) else { return out };
    for raw in &parsed.protobuf.stmts {
        let Some(NB::SelectStmt(select)) = raw.stmt.as_deref().and_then(|s| s.node.as_ref())
            else { continue };
        for from in &select.from_clause { collect_unnest(from, &mut out); }
    }
    out
}

fn collect_unnest(node: &pg_query::protobuf::Node, out: &mut HashSet<String>) {
    match node.node.as_ref() {
        Some(NB::RangeFunction(rf)) => {
            let alias = rf.alias.as_ref().map(|a| a.aliasname.clone()).unwrap_or_default();
            if alias.is_empty() { return; }
            for outer in &rf.functions {
                let Some(NB::List(l)) = outer.node.as_ref() else { continue };
                let fc = l.items.iter().find_map(|i| match i.node.as_ref()? {
                    NB::FuncCall(fc) => Some(fc.as_ref()),
                    _ => None,
                });
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
    let last = fc.funcname.last()?;
    match last.node.as_ref()? {
        NB::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

fn unnest_arg_is_literal(fc: &FuncCall) -> bool {
    let Some(body) = fc.args.first().and_then(|a| a.node.as_ref()) else { return false };
    match body {
        NB::AArrayExpr(_) => true,
        NB::AConst(c) => matches!(&c.val, Some(pg_query::protobuf::a_const::Val::Sval(_))),
        NB::TypeCast(tc) => tc.arg.as_deref()
            .and_then(|a| a.node.as_ref())
            .is_some_and(|n| matches!(n, NB::AArrayExpr(_) | NB::AConst(_))),
        _ => false,
    }
}

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
