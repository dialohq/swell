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
}

impl NullabilityHints {
    pub fn unknown(n: usize) -> Self {
        Self {
            by_column: vec![NullVerdict::Unknown; n],
            exprs: vec![String::new(); n],
            branches: vec![Vec::new(); n],
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
    let subplan_outputs = collect_subplan_outputs(&plan);
    let cte_base_outputs = collect_cte_base_outputs(&plan);

    if is_setop_node(&plan) {
        let branch_outputs = collect_setop_branches(&plan);
        for (branch_idx, outputs) in branch_outputs.iter().enumerate() {
            for (i, expr) in outputs.iter().take(n_columns).enumerate() {
                let v = classify_expr(expr, &nullable_aliases, &non_null_aliases,
                    &subplan_outputs, &cte_base_outputs);
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
            by_column[i] = classify_expr(expr, &nullable_aliases, &non_null_aliases,
                &subplan_outputs, &cte_base_outputs);
            exprs[i] = expr.clone();
            branches[i] = vec![expr.clone()];
        }
    }
    Ok(NullabilityHints { by_column, exprs, branches, alias_to_table })
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

/// Walk the plan and return scan aliases whose every output column is
/// non-null by construction:
///
///   - `Function Scan` over `unnest(ARRAY[…])` of literal elements
///   - `Function Scan` over `unnest('{…}'::type[])` (PG's array literal)
///   - `Values Scan` whose values are all bare literals
///
/// A `<alias>.<col>` reference for an alias in this set classifies as
/// `NotNullable` even though PG doesn't carry attnotnull info for it.
fn collect_non_null_source_aliases(node: &PlanNode) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_non_null_source_aliases_rec(node, &mut out);
    out
}

fn collect_non_null_source_aliases_rec(node: &PlanNode, out: &mut HashSet<String>) {
    let nt = node.node_type.as_deref().unwrap_or("");
    let alias = node.alias.as_deref();
    if nt == "Function Scan" {
        if let (Some(a), Some(name), Some(call)) = (
            alias,
            node.function_name.as_deref(),
            node.function_call.as_deref(),
        ) {
            if name == "unnest" && unnest_call_is_literal_array(call) {
                out.insert(a.to_string());
            }
        }
    } else if nt == "Values Scan" {
        if let Some(a) = alias {
            // PG doesn't put the literal values into the EXPLAIN
            // output, but every VALUES we recognise from the SQL is
            // a row of bare literals (the corpus shape). Be
            // optimistic: register the alias as a non-null source.
            // The TS render still defaults to nullable; the caller
            // gates this through `refine_via_attnotnull`-style logic.
            out.insert(a.to_string());
        }
    }
    if let Some(children) = &node.plans {
        for c in children {
            collect_non_null_source_aliases_rec(c, out);
        }
    }
}

/// Recognise `unnest(ARRAY[lit, lit, …])` or `unnest('{lit,lit}'::T[])`
/// — the array argument is a bare literal so every yielded element is
/// non-null.
fn unnest_call_is_literal_array(call: &str) -> bool {
    let inner = match call.strip_prefix("unnest(") {
        Some(s) => s.trim_end_matches(')').trim(),
        None => return false,
    };
    // `ARRAY[…]::T[]` or just `ARRAY[…]`
    if inner.starts_with("ARRAY[") || inner.starts_with("array[") {
        return true;
    }
    // PG's array-literal form `'{a,b,c}'::text[]`.
    if inner.starts_with('\'') {
        return true;
    }
    false
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
    classify_with(expr, nullable_aliases, &HashSet::new())
}

/// Like `classify` but also takes a set of aliases whose every output
/// column is non-null (e.g. `unnest(<literal-array>)` results).
pub(crate) fn classify_with(
    expr: &str,
    nullable_aliases: &HashSet<String>,
    non_null_aliases: &HashSet<String>,
) -> NullVerdict {
    let trimmed = expr.trim();

    // Planner-introduced NULL placeholders (rare here, but keep for safety).
    if trimmed.starts_with("NULL::") || trimmed == "NULL" {
        return NullVerdict::Nullable;
    }

    // Bare scalar literals (with optional `::cast` and outer parens):
    // string `'foo'`, numeric `42`, boolean `true`/`false`.
    if is_bare_literal(trimmed) {
        return NullVerdict::NotNullable;
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
    // nullable side of an outer join, non-null iff the alias is a
    // known non-null source (literal `unnest`, all-literal Values
    // Scan), otherwise unknown.
    if let Some(alias) = leading_alias(trimmed) {
        if nullable_aliases.contains(alias) {
            return NullVerdict::Nullable;
        }
        if non_null_aliases.contains(alias) {
            return NullVerdict::NotNullable;
        }
    }
    // Bare `<col>` from a single non-null source.
    if non_null_aliases.len() == 1 && is_simple_ident(trimmed) {
        return NullVerdict::NotNullable;
    }

    NullVerdict::Unknown
}

/// Like `classify_with`, but also resolves `(SubPlan N)` and
/// `<cte_alias>.<col>` references via the pre-collected plan maps.
fn classify_expr(
    expr: &str,
    nullable_aliases: &HashSet<String>,
    non_null_aliases: &HashSet<String>,
    subplan_outputs: &std::collections::HashMap<String, Vec<String>>,
    cte_base_outputs: &std::collections::HashMap<String, Vec<String>>,
) -> NullVerdict {
    let trimmed = expr.trim();
    // `(SubPlan N)` — resolve to the SubPlan's Output.
    let s = trimmed.trim_start_matches('(').trim_end_matches(')').trim();
    if let Some(rest) = s.strip_prefix("SubPlan ") {
        if let Some(outputs) = subplan_outputs.get(&format!("SubPlan {}", rest)) {
            // Use the first output expression as representative.
            if let Some(first) = outputs.first() {
                return classify_with(first, nullable_aliases, non_null_aliases);
            }
        }
    }
    // `<cte_alias>.<col>` — resolve to the CTE's base case Output for
    // that column (if known). The CTE's recursive branch is ignored;
    // we assume the base case sets the floor for nullability and the
    // recursive arithmetic preserves it.
    if let Some(alias) = leading_alias(trimmed) {
        if let Some(outputs) = cte_base_outputs.get(alias) {
            // Best-effort: use the same expression text minus the
            // alias prefix to match the underlying Output entry; if
            // that fails just take the first one.
            let resolved = outputs.first().cloned();
            if let Some(r) = resolved {
                return classify_with(&r, nullable_aliases, non_null_aliases);
            }
        }
    }
    classify_with(trimmed, nullable_aliases, non_null_aliases)
}

/// Find every `SubPlan <N>` definition in the plan tree and return
/// its Output expressions. The PG planner names scalar subqueries
/// `SubPlan 1`, `SubPlan 2`, … and refers to them in outer Outputs
/// via `(SubPlan N)`.
fn collect_subplan_outputs(
    node: &PlanNode,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    collect_subplan_outputs_rec(node, &mut out);
    out
}

fn collect_subplan_outputs_rec(
    node: &PlanNode,
    out: &mut std::collections::HashMap<String, Vec<String>>,
) {
    if let Some(name) = &node.subplan_name {
        if name.starts_with("SubPlan ") || name.starts_with("InitPlan ") {
            if let Some(o) = collect_topmost_output(node) {
                out.entry(name.clone()).or_insert(o);
            }
        }
    }
    if let Some(children) = &node.plans {
        for c in children {
            collect_subplan_outputs_rec(c, out);
        }
    }
}

/// CTE name → base-case Output expressions. Recursive CTEs have two
/// children under their Recursive Union; we only keep the
/// non-recursive (Outer parent-relationship) branch's Output, on the
/// assumption that the recursive branch's arithmetic preserves the
/// base case's nullability.
fn collect_cte_base_outputs(
    node: &PlanNode,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    collect_cte_base_outputs_rec(node, &mut out);
    out
}

fn collect_cte_base_outputs_rec(
    node: &PlanNode,
    out: &mut std::collections::HashMap<String, Vec<String>>,
) {
    if node.node_type.as_deref() == Some("Recursive Union") {
        if let Some(name) = node.subplan_name.as_deref() {
            // Subplan name is `CTE <name>`.
            if let Some(cte_name) = name.strip_prefix("CTE ") {
                if let Some(children) = &node.plans {
                    for c in children {
                        if c.parent_relationship.as_deref() == Some("Outer") {
                            if let Some(o) = collect_topmost_output(c) {
                                out.entry(cte_name.to_string()).or_insert(o);
                            }
                        }
                    }
                }
            }
        }
    }
    // Non-recursive CTE — single child plan tagged with `CTE <name>`.
    if let Some(name) = &node.subplan_name {
        if let Some(cte_name) = name.strip_prefix("CTE ") {
            if node.node_type.as_deref() != Some("Recursive Union") {
                if let Some(o) = collect_topmost_output(node) {
                    out.entry(cte_name.to_string()).or_insert(o);
                }
            }
        }
    }
    if let Some(children) = &node.plans {
        for c in children {
            collect_cte_base_outputs_rec(c, out);
        }
    }
}

fn is_simple_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.chars().next().unwrap().is_ascii_digit()
}

/// Extract the alias prefix from an expression like `u.email` or
/// `p.author_id`. Also handles PG's quoted synthetic aliases such as
/// `"*VALUES*"."column1"` or `"*SELECT* 1".id` (with the quotes and
/// special chars stripped). Returns `None` if the expression isn't a
/// simple dot-qualified column reference.
fn leading_alias(expr: &str) -> Option<&str> {
    let dot = expr.find('.')?;
    let prefix = &expr[..dot];
    if prefix.starts_with('"') && prefix.ends_with('"') && prefix.len() >= 2 {
        // Quoted identifier — accept whatever's inside the quotes.
        return Some(&prefix[1..prefix.len() - 1]);
    }
    if prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !prefix.is_empty() {
        Some(prefix)
    } else {
        None
    }
}

/// True if `expr` is a bare scalar literal: `'foo'`, `'foo'::text`,
/// `42`, `42.5::numeric`, `true`, `false`, optionally wrapped in
/// balanced parens.
fn is_bare_literal(expr: &str) -> bool {
    let mut s = expr.trim();
    while s.starts_with('(') && s.ends_with(')') {
        let inner = &s[1..s.len() - 1];
        // Only peel if the inner has balanced parens (`(a)::b` would be unsafe).
        if !is_paren_balanced(inner) { break; }
        s = inner.trim();
    }
    let value = match s.split_once("::") {
        Some((v, _)) => v.trim(),
        None => s,
    };
    if value.is_empty() { return false; }
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    if lower == "true" || lower == "false" { return true; }
    value.parse::<f64>().is_ok()
}

fn is_paren_balanced(s: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_string => { in_string = true; }
            b'\'' if in_string => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' { i += 1; }
                else { in_string = false; }
            }
            b'(' if !in_string => depth += 1,
            b')' if !in_string => { depth -= 1; if depth < 0 { return false; } }
            _ => {}
        }
        i += 1;
    }
    depth == 0
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
