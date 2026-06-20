//! AST-based nullability classification of SELECT target expressions.
//!
//! Mirrors what `nullability::classify_with` does on EXPLAIN VERBOSE
//! expression text, but operates on the pg_query parse tree directly.
//! Exact node + funcname matching beats substring matching against the
//! deparsed EXPLAIN text — no false positives on user-defined
//! `sum_squared(...)` or `coalesce_safe(...)`, no parser quirks
//! around quoted aliases or planner constant folding.
//!
//! The classifier returns `None` for shapes it doesn't recognise; the
//! caller falls back to EXPLAIN-text classification (currently still
//! needed for SubPlan / CTE indirection where there is no convenient
//! SQL-level node to consult).
//!
//! Aggregate / never-null function names match the canonical
//! `pg_catalog` short names exactly. A user-defined `public.sum(...)`
//! that shadows the builtin would be a FuncCall with
//! `funcname = ["sum"]`; without schema qualification we can't tell it
//! apart from the catalog `sum`. The same ambiguity exists in EXPLAIN
//! text; the answer is `--types.by_name` / `pg_proc` cross-check
//! upstream, not stricter matching here.

use crate::nullability::NullVerdict;
use pg_query::protobuf::{node::Node as NB, Node};
use std::collections::HashSet;

/// Aggregate functions that return `NULL` over an empty input set.
const NULLABLE_AGGS: &[&str] = &[
    "sum", "avg", "min", "max",
    "array_agg", "json_agg", "jsonb_agg",
    "string_agg", "bool_and", "bool_or",
];

/// Functions whose return value is guaranteed non-NULL by construction.
/// Covers window funcs with non-empty partitions, time / session
/// builtins, UUID generators, advisory locks, and JSON / row builders.
const NEVER_NULL_FUNCS: &[&str] = &[
    // count — always returns ≥ 0.
    "count",
    // Window funcs over a non-empty partition.
    "row_number", "rank", "dense_rank", "ntile", "cume_dist", "percent_rank",
    // Session / time builtins.
    "now", "current_timestamp", "current_date", "current_time",
    "localtimestamp", "localtime",
    "current_user", "session_user", "current_database",
    "current_schema", "current_setting",
    "gen_random_uuid", "uuid_generate_v1", "uuid_generate_v4",
    "pg_advisory_lock", "pg_advisory_xact_lock",
    // JSON / row builders — construct value from args, never short-circuit.
    "jsonb_build_object", "json_build_object",
    "jsonb_build_array", "json_build_array",
    "to_jsonb", "to_json", "row_to_json", "array_to_json",
];

/// Try to classify an EXPLAIN expression by reparsing it as SQL.
/// Wraps the text as `SELECT <expr>;` and runs the regular AST
/// classifier on the resulting target node. Returns `None` when
/// reparsing fails (synthetic refs like `(SubPlan 1)`,
/// `"*VALUES*"."column1"`, planner-only forms) or the AST shape isn't
/// one the classifier handles.
pub fn try_classify_text(
    expr: &str,
    nullable_aliases: &HashSet<String>,
    non_null_aliases: &HashSet<String>,
) -> Option<NullVerdict> {
    let parsed = pg_query::parse(&format!("SELECT {expr}")).ok()?;
    let raw = parsed.protobuf.stmts.into_iter().next()?;
    let body = raw.stmt?.node?;
    let select = match body { NB::SelectStmt(s) => s, _ => return None };
    let target = select.target_list.into_iter().next()?;
    let res = match target.node? { NB::ResTarget(rt) => rt, _ => return None };
    let val = res.val?;
    classify(&val, nullable_aliases, non_null_aliases)
}

/// Classify a SQL expression's AST node. Returns `None` for shapes the
/// classifier doesn't handle.
pub fn classify(
    node: &Node,
    nullable_aliases: &HashSet<String>,
    non_null_aliases: &HashSet<String>,
) -> Option<NullVerdict> {
    let body = node.node.as_ref()?;
    Some(match body {
        // Bare literals.
        NB::AConst(c) => if c.isnull { NullVerdict::Nullable } else { NullVerdict::NotNullable },

        // `<expr>::T` — the cast preserves the inner's nullability for
        // the purposes of classification. `NULL::int` → AConst{isnull}
        // under TypeCast → Nullable.
        NB::TypeCast(tc) => classify(tc.arg.as_ref()?, nullable_aliases, non_null_aliases)?,

        // Array constructor — non-null even when elements are.
        NB::AArrayExpr(_) => NullVerdict::NotNullable,

        // pg_query reports `coalesce(...)` as a FuncCall at parse time;
        // CoalesceExpr only appears post-analysis. We handle both.
        NB::CoalesceExpr(ce) => coalesce_verdict(&ce.args, nullable_aliases, non_null_aliases)?,

        NB::CaseExpr(ce) => case_verdict(ce, nullable_aliases, non_null_aliases)?,

        NB::FuncCall(fc) => {
            let name = last_funcname(&fc.funcname)?;
            if name == "coalesce" {
                coalesce_verdict(&fc.args, nullable_aliases, non_null_aliases)?
            } else if NEVER_NULL_FUNCS.contains(&name) {
                NullVerdict::NotNullable
            } else if NULLABLE_AGGS.contains(&name) {
                NullVerdict::Nullable
            } else {
                return None;
            }
        }

        // Plain `<alias>.<col>` reference — alias decides the verdict.
        // Bare `<col>` (single segment) only resolves when there's a
        // single known non-null source in scope; otherwise we don't
        // know which table the column belongs to.
        NB::ColumnRef(cr) => match leading_alias(&cr.fields) {
            Some(alias) if nullable_aliases.contains(alias) => NullVerdict::Nullable,
            Some(alias) if non_null_aliases.contains(alias) => NullVerdict::NotNullable,
            Some(_) => return None,
            None if cr.fields.len() == 1 && non_null_aliases.len() == 1 => NullVerdict::NotNullable,
            None => return None,
        },

        _ => return None,
    })
}

fn coalesce_verdict(
    args: &[Node], nullable: &HashSet<String>, non_null: &HashSet<String>,
) -> Option<NullVerdict> {
    // Trailing literal (or any provably non-null arg) makes the whole
    // expression non-null.
    let last = args.last()?;
    match classify(last, nullable, non_null)? {
        NullVerdict::NotNullable => Some(NullVerdict::NotNullable),
        _ => None,
    }
}

fn case_verdict(
    ce: &pg_query::protobuf::CaseExpr,
    nullable: &HashSet<String>, non_null: &HashSet<String>,
) -> Option<NullVerdict> {
    // No ELSE → all unmatched rows return NULL.
    let Some(def) = ce.defresult.as_ref() else { return Some(NullVerdict::Nullable) };
    // ELSE <non-null> → the whole expression is non-null (the THEN
    // branches don't matter — the ELSE is a guaranteed fallback). We
    // recurse on the ELSE; only NotNullable maps to NotNullable.
    match classify(def, nullable, non_null)? {
        NullVerdict::NotNullable => Some(NullVerdict::NotNullable),
        _ => None,
    }
}

/// Take the last segment of a (possibly qualified) function name. PG
/// emits `["pg_catalog", "count"]` for a fully-qualified call and just
/// `["count"]` for the unqualified form. The last segment is the
/// catalog-relative short name.
fn last_funcname(parts: &[Node]) -> Option<&str> {
    let last = parts.last()?;
    match last.node.as_ref()? {
        NB::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

/// Take the first segment of a `<alias>.<col>` ColumnRef. Returns
/// `None` for bare unqualified refs (PG omits the alias in EXPLAIN
/// when there's only one relation in scope).
fn leading_alias(fields: &[Node]) -> Option<&str> {
    if fields.len() < 2 { return None; }
    match fields.first()?.node.as_ref()? {
        NB::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pg_query::protobuf::node::Node as NB;

    fn na() -> HashSet<String> { HashSet::new() }

    /// Parse `sql`, return the val of the first top-level target.
    fn target0(sql: &str) -> pg_query::protobuf::Node {
        let parsed = pg_query::parse(sql).expect("parse");
        let raw = parsed.protobuf.stmts.first().expect("stmt");
        let stmt = raw.stmt.as_ref().unwrap().node.as_ref().unwrap();
        let select = match stmt {
            NB::SelectStmt(s) => s,
            _ => panic!("not a SELECT"),
        };
        let target = select.target_list.first().expect("target");
        let res = match target.node.as_ref().unwrap() {
            NB::ResTarget(rt) => rt,
            _ => panic!("not ResTarget"),
        };
        *res.val.clone().expect("val")
    }

    #[test]
    fn null_literal_is_nullable() {
        assert_eq!(classify(&target0("SELECT NULL"), &na(), &na()), Some(NullVerdict::Nullable));
        assert_eq!(classify(&target0("SELECT NULL::int"), &na(), &na()), Some(NullVerdict::Nullable));
    }

    #[test]
    fn bare_literals_are_not_nullable() {
        assert_eq!(classify(&target0("SELECT 42"), &na(), &na()), Some(NullVerdict::NotNullable));
        assert_eq!(classify(&target0("SELECT 'foo'"), &na(), &na()), Some(NullVerdict::NotNullable));
        assert_eq!(classify(&target0("SELECT true"), &na(), &na()), Some(NullVerdict::NotNullable));
        assert_eq!(classify(&target0("SELECT 'foo'::text"), &na(), &na()), Some(NullVerdict::NotNullable));
    }

    #[test]
    fn count_is_not_nullable() {
        assert_eq!(classify(&target0("SELECT count(*) FROM t"), &na(), &na()), Some(NullVerdict::NotNullable));
        assert_eq!(classify(&target0("SELECT count(t.x) FROM t"), &na(), &na()), Some(NullVerdict::NotNullable));
    }

    #[test]
    fn nullable_aggs() {
        assert_eq!(classify(&target0("SELECT sum(t.x) FROM t"), &na(), &na()), Some(NullVerdict::Nullable));
        assert_eq!(classify(&target0("SELECT max(t.y) FROM t"), &na(), &na()), Some(NullVerdict::Nullable));
        assert_eq!(classify(&target0("SELECT array_agg(t.x ORDER BY t.id) FROM t"), &na(), &na()),
            Some(NullVerdict::Nullable));
    }

    #[test]
    fn coalesce_with_trailing_literal() {
        assert_eq!(classify(&target0("SELECT coalesce(t.x, 0) FROM t"), &na(), &na()),
            Some(NullVerdict::NotNullable));
        assert_eq!(classify(&target0("SELECT coalesce(t.s, 'fb') FROM t"), &na(), &na()),
            Some(NullVerdict::NotNullable));
        // Unprovable trailing arg → fall through.
        assert_eq!(classify(&target0("SELECT coalesce(t.x, t.y) FROM t"), &na(), &na()), None);
    }

    #[test]
    fn case_without_else_is_nullable() {
        assert_eq!(classify(&target0("SELECT CASE WHEN t.x > 0 THEN t.x END FROM t"), &na(), &na()),
            Some(NullVerdict::Nullable));
    }

    #[test]
    fn case_with_literal_else_is_not_nullable() {
        let v = classify(&target0("SELECT CASE WHEN t.x > 0 THEN t.x ELSE 0 END FROM t"), &na(), &na());
        assert_eq!(v, Some(NullVerdict::NotNullable));
        let v = classify(&target0("SELECT CASE WHEN t.x > 0 THEN t.x ELSE 'foo' END FROM t"), &na(), &na());
        assert_eq!(v, Some(NullVerdict::NotNullable));
        let v = classify(&target0("SELECT CASE WHEN t.x > 0 THEN t.x ELSE TRUE END FROM t"), &na(), &na());
        assert_eq!(v, Some(NullVerdict::NotNullable));
        // ELSE with a non-literal → fall through.
        let v = classify(&target0("SELECT CASE WHEN t.x > 0 THEN t.x ELSE t.y END FROM t"), &na(), &na());
        assert_eq!(v, None);
    }

    #[test]
    fn column_ref_on_nullable_alias() {
        let nulls: HashSet<String> = ["p".to_string()].into_iter().collect();
        assert_eq!(classify(&target0("SELECT p.body FROM posts p"), &nulls, &na()), Some(NullVerdict::Nullable));
        // Alias not in either set → caller defaults to attnotnull.
        assert_eq!(classify(&target0("SELECT u.email FROM users u"), &nulls, &na()), None);
    }

    #[test]
    fn unrecognised_funccall_falls_through() {
        assert_eq!(classify(&target0("SELECT my_func(t.x) FROM t"), &na(), &na()), None);
    }

    #[test]
    fn array_constructor_is_not_nullable() {
        assert_eq!(classify(&target0("SELECT ARRAY[1, 2, 3]"), &na(), &na()), Some(NullVerdict::NotNullable));
    }

    #[test]
    fn window_func_row_number_is_not_nullable() {
        let v = classify(&target0(
            "SELECT row_number() OVER (PARTITION BY t.id ORDER BY t.x) FROM t",
        ), &na(), &na());
        assert_eq!(v, Some(NullVerdict::NotNullable));
    }

    #[test]
    fn try_classify_text_handles_row_number_over() {
        let v = try_classify_text(
            "row_number() OVER (PARTITION BY t.id ORDER BY t.x DESC)",
            &na(), &na(),
        );
        assert_eq!(v, Some(NullVerdict::NotNullable));
    }
}
