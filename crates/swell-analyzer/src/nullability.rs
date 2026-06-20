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
    /// `alias → (schema, relation)` for every scan node in the plan.
    /// Lets the analyzer turn an EXPLAIN expression like
    /// `(users.email)` back into `(public, users, email)` so it can
    /// look up `attnotnull` for the referenced base column.
    pub alias_to_table: std::collections::HashMap<String, (String, String)>,
}

impl NullabilityHints {
    pub fn unknown(n: usize) -> Self {
        Self {
            by_column: vec![NullVerdict::Unknown; n],
            exprs: vec![String::new(); n],
            alias_to_table: std::collections::HashMap::new(),
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
    let json_text = extract_first_value(&msgs).unwrap_or_default();

    let json: serde_json::Value = serde_json::from_str(&json_text).unwrap_or(serde_json::Value::Null);
    let plans: Vec<ExplainEntry> = serde_json::from_value(json).unwrap_or_default();
    let plan = match plans.into_iter().next() {
        Some(p) => p.plan,
        None => return Ok(NullabilityHints::unknown(n_columns)),
    };

    // Bottom-up: compute the set of aliases that can be NULL on this plan.
    let nullable_aliases = collect_nullable_aliases(&plan);
    // Topmost Output list aligned with RowDescription.
    let outputs = collect_topmost_output(&plan).unwrap_or_default();
    // `alias → (schema, relation)` from every scan node; used by the
    // caller to resolve EXPLAIN's `<alias>.<col>` back to a table column.
    let alias_to_table = collect_alias_to_table(&plan);

    let mut by_column = vec![NullVerdict::Unknown; n_columns];
    let mut exprs = vec![String::new(); n_columns];
    for (i, expr) in outputs.iter().take(n_columns).enumerate() {
        by_column[i] = classify(expr, &nullable_aliases);
        exprs[i] = expr.clone();
    }
    Ok(NullabilityHints { by_column, exprs, alias_to_table })
}

/// Walk every Scan node and record its `(alias, schema, relation_name)`
/// so the caller can resolve EXPLAIN's column qualifications.
fn collect_alias_to_table(node: &PlanNode) -> std::collections::HashMap<String, (String, String)> {
    let mut out = std::collections::HashMap::new();
    collect_alias_to_table_rec(node, &mut out);
    out
}

fn collect_alias_to_table_rec(
    node: &PlanNode,
    out: &mut std::collections::HashMap<String, (String, String)>,
) {
    if let (Some(alias), Some(rel)) = (&node.alias, &node.relation_name) {
        let schema = node.schema.clone().unwrap_or_default();
        out.entry(alias.clone()).or_insert((schema, rel.clone()));
    }
    if let Some(children) = &node.plans {
        for c in children {
            collect_alias_to_table_rec(c, out);
        }
    }
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
    #[allow(dead_code)]
    node_type: Option<String>,
}

fn collect_topmost_output(node: &PlanNode) -> Option<Vec<String>> {
    if let Some(out) = &node.output {
        return Some(out.clone());
    }
    if let Some(children) = &node.plans {
        for c in children {
            if let Some(o) = collect_topmost_output(c) {
                return Some(o);
            }
        }
    }
    None
}

/// Walk the plan tree and return every `Alias` whose rows can be NULL due
/// to an outer-join above it. Built bottom-up:
///
///   - Scan nodes contribute their own alias only (and never make it
///     nullable).
///   - Inner Join nodes propagate nullability from their children.
///   - Left Join nodes mark the Inner-side aliases as nullable.
///   - Right Join nodes mark the Outer-side aliases as nullable.
///   - Full Join nodes mark both sides as nullable.
fn collect_nullable_aliases(node: &PlanNode) -> HashSet<String> {
    let mut already_nullable = HashSet::new();
    walk(node, &mut already_nullable);
    already_nullable
}

fn walk(node: &PlanNode, nullable: &mut HashSet<String>) {
    let join_type = node.join_type.as_deref();
    let children = node.plans.as_deref().unwrap_or(&[]);

    // First, descend into children — preserves anything they marked.
    for c in children {
        walk(c, nullable);
    }

    // Then apply the join-type rule at this node.
    match join_type {
        Some("Left") => {
            for c in children {
                if c.parent_relationship.as_deref() == Some("Inner") {
                    nullable.extend(collect_subtree_aliases(c));
                }
            }
        }
        Some("Right") => {
            for c in children {
                if c.parent_relationship.as_deref() == Some("Outer") {
                    nullable.extend(collect_subtree_aliases(c));
                }
            }
        }
        Some("Full") => {
            for c in children {
                nullable.extend(collect_subtree_aliases(c));
            }
        }
        _ => {} // Inner / Anti / Semi / no join → propagate child decisions only
    }
}

fn collect_subtree_aliases(node: &PlanNode) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(a) = &node.alias {
        set.insert(a.clone());
    }
    if let Some(children) = &node.plans {
        for c in children {
            set.extend(collect_subtree_aliases(c));
        }
    }
    set
}

// ------------- Per-expression classification -------------

pub(crate) fn classify(expr: &str, nullable_aliases: &HashSet<String>) -> NullVerdict {
    let trimmed = expr.trim();

    // Planner-introduced NULL placeholders (rare here, but keep for safety).
    if trimmed.starts_with("NULL::") || trimmed == "NULL" {
        return NullVerdict::Nullable;
    }

    let lower = trimmed.to_ascii_lowercase();

    if lower.starts_with("count(") {
        return NullVerdict::NotNullable;
    }

    // Array literal constructor — never null even when elements are.
    if lower.starts_with("array[") {
        return NullVerdict::NotNullable;
    }

    // Window functions that always return a value (the partition is
    // non-empty by construction at the call site).
    let non_null_window_funcs = [
        "row_number(", "rank(", "dense_rank(", "ntile(",
        "cume_dist(", "percent_rank(",
    ];
    if non_null_window_funcs.iter().any(|p| lower.starts_with(p)) {
        return NullVerdict::NotNullable;
    }

    // SQL builtins / system functions that always produce a value.
    let never_null_builtins = [
        "now(", "current_timestamp", "current_date", "current_time",
        "localtimestamp", "localtime",
        "current_user", "session_user", "current_database(",
        "current_schema(", "current_setting(",
        "gen_random_uuid(", "uuid_generate_v1(", "uuid_generate_v4(",
        "pg_advisory_lock(", "pg_advisory_xact_lock(",
    ];
    if never_null_builtins.iter().any(|p| lower.starts_with(p)) {
        return NullVerdict::NotNullable;
    }

    // Functions whose return value is never NULL by construction (apart
    // from the degenerate case of being called on NULL input). These build
    // a value from their arguments and never short-circuit to NULL.
    let never_null_funcs = [
        "jsonb_build_object(", "json_build_object(",
        "jsonb_build_array(", "json_build_array(",
        "to_jsonb(", "to_json(",
        "row_to_json(",
        "array_to_json(",
    ];
    if never_null_funcs.iter().any(|p| lower.starts_with(p)) {
        return NullVerdict::NotNullable;
    }

    let nullable_aggs = [
        "sum(", "avg(", "min(", "max(",
        "array_agg(", "json_agg(", "jsonb_agg(",
        "string_agg(", "bool_and(", "bool_or(",
    ];
    if nullable_aggs.iter().any(|p| lower.starts_with(p)) {
        return NullVerdict::Nullable;
    }

    if lower.starts_with("case ") {
        if !lower.contains(" else ") {
            return NullVerdict::Nullable;
        }
        // CASE … ELSE <something> END. If the ELSE branch is a literal
        // (number / quoted string / TRUE / FALSE), the whole expression
        // is non-null. We don't bother proving the THEN branches non-null
        // — even if they're nullable, the ELSE literal is still a fallback.
        if let Some(else_idx) = lower.find(" else ") {
            let after = &expr[else_idx + 6..]; // " else "
            let trimmed_after = after.trim();
            // First token after ELSE.
            let first = trimmed_after.split_whitespace().next().unwrap_or("");
            if first.starts_with('\'')
                || first.chars().next().map(|c| c.is_ascii_digit() || c == '-' || c == '+').unwrap_or(false)
                || first.eq_ignore_ascii_case("true")
                || first.eq_ignore_ascii_case("false")
            {
                return NullVerdict::NotNullable;
            }
        }
        return NullVerdict::Unknown;
    }

    if lower.starts_with("coalesce(") {
        return if has_trailing_non_null_literal(trimmed) {
            NullVerdict::NotNullable
        } else {
            NullVerdict::Unknown
        };
    }

    // Plain `<alias>.<col>` reference: nullable iff alias is on the
    // nullable side of an outer join.
    if let Some(alias) = leading_alias(trimmed) {
        if nullable_aliases.contains(alias) {
            return NullVerdict::Nullable;
        }
    }

    NullVerdict::Unknown
}

/// Extract the alias prefix from an expression like `u.email` or
/// `p.author_id`. Returns None if the expression isn't a simple
/// dot-qualified column reference.
fn leading_alias(expr: &str) -> Option<&str> {
    let dot = expr.find('.')?;
    let prefix = &expr[..dot];
    if prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !prefix.is_empty() {
        // Avoid catching schema.table — we want just the alias.
        // Bare ident before the dot is fine.
        Some(prefix)
    } else {
        None
    }
}

fn has_trailing_non_null_literal(expr: &str) -> bool {
    let inner = match expr
        .trim_start_matches(|c: char| c.is_alphabetic() || c == '_')
        .strip_prefix('(')
    {
        Some(s) => s,
        None => return false,
    };
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    let last = inner.rsplit(',').next().unwrap_or("").trim();
    if last.is_empty() { return false; }
    if last.chars().next().map(|c| c.is_ascii_digit() || c == '-' || c == '+').unwrap_or(false) {
        return true;
    }
    if last.starts_with('\'') && !last.starts_with("''") {
        return true;
    }
    false
}

fn extract_first_value(msgs: &[tokio_postgres::SimpleQueryMessage]) -> Option<String> {
    for m in msgs {
        if let tokio_postgres::SimpleQueryMessage::Row(row) = m {
            if let Some(s) = row.get(0) {
                return Some(s.to_string());
            }
        }
    }
    None
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
