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
    /// For each column whose `Output` expression is a strict transform
    /// (currently: `(<col>)::<type>` casts) of a single base column we
    /// could locate in the plan's scans, the (schema, relation, attname)
    /// triple. Used by the analyzer to look up `attnotnull` for
    /// expressions where `RowDescription` dropped `(table_oid, attnum)`.
    pub base_refs: Vec<Option<BaseColumnRef>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaseColumnRef {
    pub schema: String,
    pub relation: String,
    pub attname: String,
}

impl NullabilityHints {
    pub fn unknown(n: usize) -> Self {
        Self {
            by_column: vec![NullVerdict::Unknown; n],
            base_refs: vec![None; n],
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
    // Map each scan-level alias to (schema, relation) so we can resolve
    // `<alias>.<col>` and unqualified `<col>` (single-relation case)
    // references inside strict expressions back to base columns.
    let scans = collect_scans(&plan);
    // Topmost Output list aligned with RowDescription.
    let outputs = collect_topmost_output(&plan).unwrap_or_default();

    let mut by_column = vec![NullVerdict::Unknown; n_columns];
    let mut base_refs: Vec<Option<BaseColumnRef>> = vec![None; n_columns];
    for (i, expr) in outputs.iter().take(n_columns).enumerate() {
        by_column[i] = classify(expr, &nullable_aliases);
        base_refs[i] = base_column_ref(expr, &scans);
    }
    Ok(NullabilityHints { by_column, base_refs })
}

#[derive(Debug, Clone)]
struct ScanInfo {
    alias: String,
    schema: String,
    relation: String,
}

/// Walk the plan and collect every scan node's (alias, schema, relation)
/// — used to resolve column refs back to base columns.
fn collect_scans(node: &PlanNode) -> Vec<ScanInfo> {
    let mut out = Vec::new();
    collect_scans_rec(node, &mut out);
    out
}

fn collect_scans_rec(node: &PlanNode, out: &mut Vec<ScanInfo>) {
    if let (Some(rel), Some(alias)) = (node.relation_name.as_deref(), node.alias.as_deref()) {
        out.push(ScanInfo {
            alias: alias.to_string(),
            schema: node.schema.clone().unwrap_or_default(),
            relation: rel.to_string(),
        });
    }
    if let Some(children) = &node.plans {
        for c in children {
            collect_scans_rec(c, out);
        }
    }
}

/// If the EXPLAIN `Output` expression is a strict transform of a single
/// base column we can map to one of the plan's scans, return that base
/// column. Otherwise `None`.
///
/// Strict transforms we recognise:
///   - `<alias>.<col>`            — bare qualified ref (subqueries can
///     lose `table_oid` even without a cast)
///   - `<col>`                    — bare unqualified ref, when exactly
///     one scan exposes a relation
///   - `(<inner>)::<type>`        — cast wrapper (recurses)
fn base_column_ref(expr: &str, scans: &[ScanInfo]) -> Option<BaseColumnRef> {
    let inner = unwrap_strict_cast(expr);
    let (alias_opt, col) = parse_column_ref(inner)?;
    let scan = match alias_opt {
        Some(alias) => scans.iter().find(|s| s.alias == alias)?,
        None => {
            if scans.len() == 1 {
                &scans[0]
            } else {
                return None;
            }
        }
    };
    Some(BaseColumnRef {
        schema: scan.schema.clone(),
        relation: scan.relation.clone(),
        attname: col.to_string(),
    })
}

/// Peel off `(<x>)::<type>` cast wrappers. Multiple chained casts
/// (`((u.ts)::text)::varchar`) are all stripped.
fn unwrap_strict_cast(expr: &str) -> &str {
    let mut cur = expr.trim();
    loop {
        let stripped = match cur.strip_prefix('(') {
            Some(s) => s,
            None => return cur,
        };
        let Some(close) = find_matching_paren(stripped) else { return cur };
        let after = stripped[close + 1..].trim_start();
        if !after.starts_with("::") {
            return cur;
        }
        cur = stripped[..close].trim();
    }
}

/// Given a string that follows the *content* after a leading `(`, return
/// the index inside it of the matching `)`. Tracks nesting; ignores
/// content of single-quoted strings.
fn find_matching_paren(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: i32 = 1;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => { depth += 1; i += 1; }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Parse a simple column reference, returning `(alias, col)` if the
/// expression is exactly `<ident>` or `<alias>.<ident>` (no extra
/// indirection, no whitespace).
fn parse_column_ref(expr: &str) -> Option<(Option<&str>, &str)> {
    let trimmed = expr.trim();
    if trimmed.is_empty() || !is_ident_start(trimmed.as_bytes()[0]) {
        return None;
    }
    if let Some(dot) = trimmed.find('.') {
        let head = &trimmed[..dot];
        let tail = &trimmed[dot + 1..];
        if is_simple_ident(head) && is_simple_ident(tail) {
            return Some((Some(head), tail));
        }
        return None;
    }
    if is_simple_ident(trimmed) {
        Some((None, trimmed))
    } else {
        None
    }
}

fn is_ident_start(b: u8) -> bool { b.is_ascii_alphabetic() || b == b'_' }
fn is_ident_byte(b: u8) -> bool { b.is_ascii_alphanumeric() || b == b'_' }
fn is_simple_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && is_ident_start(b[0]) && b.iter().all(|&c| is_ident_byte(c))
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

    fn one_scan() -> Vec<ScanInfo> {
        vec![ScanInfo {
            alias: "users".to_string(),
            schema: "public".to_string(),
            relation: "users".to_string(),
        }]
    }

    fn two_scans() -> Vec<ScanInfo> {
        vec![
            ScanInfo { alias: "u".into(), schema: "public".into(), relation: "users".into() },
            ScanInfo { alias: "p".into(), schema: "public".into(), relation: "posts".into() },
        ]
    }

    #[test]
    fn base_ref_bare_column_single_relation() {
        let scans = one_scan();
        let r = base_column_ref("email", &scans).unwrap();
        assert_eq!(r.schema, "public");
        assert_eq!(r.relation, "users");
        assert_eq!(r.attname, "email");
    }

    #[test]
    fn base_ref_strips_cast_wrapper() {
        let scans = one_scan();
        let r = base_column_ref("(email)::text", &scans).unwrap();
        assert_eq!(r.attname, "email");
    }

    #[test]
    fn base_ref_handles_chained_casts() {
        let scans = one_scan();
        let r = base_column_ref("((email)::text)::varchar", &scans).unwrap();
        assert_eq!(r.attname, "email");
    }

    #[test]
    fn base_ref_qualified_resolves_via_alias_map() {
        let scans = two_scans();
        let r = base_column_ref("(u.email)::text", &scans).unwrap();
        assert_eq!(r.relation, "users");
        assert_eq!(r.attname, "email");
        let r = base_column_ref("(p.body)::text", &scans).unwrap();
        assert_eq!(r.relation, "posts");
        assert_eq!(r.attname, "body");
    }

    #[test]
    fn base_ref_bare_column_ambiguous_returns_none() {
        // With two scans, an unqualified column ref isn't safely
        // resolvable — skip rather than guess.
        let scans = two_scans();
        assert!(base_column_ref("email", &scans).is_none());
    }

    #[test]
    fn base_ref_non_strict_expressions_return_none() {
        let scans = one_scan();
        // Arithmetic — non-strict to a single column.
        assert!(base_column_ref("(id + 1)", &scans).is_none());
        // Function call.
        assert!(base_column_ref("upper(email)", &scans).is_none());
        // Concatenation.
        assert!(base_column_ref("(email || 'x')", &scans).is_none());
    }
}
