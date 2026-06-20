//! Reduce per-table CHECK predicates to TS-level column refinements.
//!
//! Reads `pg_get_constraintdef(conoid, true)` for every column-bound
//! CHECK and runs the body through `pg_query::parse_expression`. The
//! resulting AST is matched against a fixed set of *narrowing* shapes;
//! anything we don't recognise bails to the base type — partial
//! narrowing is worse than none.
//!
//! Tier 1 (this module):
//!   * `col = 'lit'`                             → `"lit"`
//!   * `col IN ('a', 'b', ...)`                  → `"a" | "b" | ...`
//!   * `col = ANY (ARRAY['a', 'b', ...])`        → `"a" | "b" | ...`
//!   * `col IS NULL OR <one of the above>`       → `... | null`

use pg_query::protobuf::{a_const::Val, node::Node as NB, AExprKind, BoolExprType, Node};
use std::collections::HashMap;
use tokio_postgres::Client;

/// One CHECK constraint reduced to a per-column TS refinement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColRefinement {
    pub column: String,
    pub refined_ts: String,
}

/// Read every CHECK constraint on `(schema, table)` and reduce each
/// column-bound one to its TS refinement.
pub async fn refinements_for(client: &Client, schema: &str, table: &str) -> Vec<ColRefinement> {
    let Ok(rows) = client
        .query(
            r#"
            SELECT pg_get_constraintdef(c.oid, true) AS def
            FROM pg_constraint c
            JOIN pg_namespace n ON n.oid = c.connamespace
            JOIN pg_class      t ON t.oid = c.conrelid
            WHERE c.contype = 'c'
              AND n.nspname = $1
              AND t.relname = $2
            "#,
            &[&schema, &table],
        )
        .await
        .inspect_err(|e| tracing::debug!("read CHECKs for {schema}.{table}: {e}"))
    else {
        return Vec::new();
    };
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for row in &rows {
        let def: String = row.get(0);
        // pg_get_constraintdef returns "CHECK (<expr>)". Strip the wrap
        // before handing the inside to pg_query.
        let Some(body) = strip_check_wrap(&def) else {
            continue;
        };
        let Some(refinement) = reduce_predicate(body) else {
            continue;
        };
        grouped
            .entry(refinement.column)
            .or_default()
            .push(refinement.refined_ts);
    }
    // Stable iteration order — alphabetical by column name.
    let mut out: Vec<(String, Vec<String>)> = grouped.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.into_iter()
        .map(|(column, parts)| ColRefinement {
            column,
            refined_ts: intersect_ts(&parts),
        })
        .collect()
}

/// `CHECK ( <expr> )` → `<expr>`. Whitespace tolerant on both sides;
/// returns None for anything that doesn't match this exact prefix.
fn strip_check_wrap(def: &str) -> Option<&str> {
    let s = def.trim();
    let s = s.strip_prefix("CHECK")?.trim_start();
    // Tolerate stray spaces before the opening paren.
    let s = s.strip_prefix('(')?;
    let s = s.strip_suffix(')')?;
    Some(s.trim())
}

fn reduce_predicate(body: &str) -> Option<ColRefinement> {
    // pg_query has no `parse_expression`; wrap the predicate in a
    // tiny SELECT so we get a real ParseResult.
    let wrapped = format!("SELECT {body}");
    let parsed = pg_query::parse(&wrapped).ok()?;
    let raw = parsed.protobuf.stmts.into_iter().next()?;
    let node = raw.stmt?.node?;
    let target = match node {
        NB::SelectStmt(s) => s.target_list.into_iter().next()?,
        _ => return None,
    };
    let NB::ResTarget(rt) = target.node? else {
        return None;
    };
    let val = (*rt.val?).node?;
    reduce_expr(&val)
}

/// Match the AST against the supported predicate shapes and emit one
/// `ColRefinement`. Returns None on anything we don't recognise.
fn reduce_expr(node: &NB) -> Option<ColRefinement> {
    // `col IS NULL OR <pred>` widens the inner refinement with `null`.
    if let NB::BoolExpr(b) = node {
        if b.boolop == BoolExprType::OrExpr as i32 && b.args.len() == 2 {
            let (a0, a1) = (b.args[0].node.as_ref()?, b.args[1].node.as_ref()?);
            let (col, inner) = match (extract_is_null(a0), reduce_expr(a1).as_ref()) {
                (Some(c), Some(_)) => (c, a1),
                _ => match (extract_is_null(a1), reduce_expr(a0).as_ref()) {
                    (Some(c), Some(_)) => (c, a0),
                    _ => return None,
                },
            };
            let mut inner_refinement = reduce_expr(inner)?;
            if inner_refinement.column != col {
                return None;
            }
            inner_refinement.refined_ts = format!("{} | null", inner_refinement.refined_ts);
            return Some(inner_refinement);
        }
    }
    let NB::AExpr(e) = node else { return None };
    let lhs = e.lexpr.as_deref().and_then(|n| n.node.as_ref())?;
    let col = extract_column_ref(lhs)?;

    // `col = ANY (ARRAY[...])`
    if e.kind == AExprKind::AexprOpAny as i32 {
        let rhs = e.rexpr.as_deref().and_then(|n| n.node.as_ref())?;
        let arr = match rhs {
            NB::AArrayExpr(a) => &a.elements,
            _ => return None,
        };
        let vs = arr
            .iter()
            .filter_map(extract_ts_literal)
            .collect::<Vec<_>>();
        if vs.len() != arr.len() || vs.is_empty() {
            return None;
        }
        return Some(ColRefinement {
            column: col,
            refined_ts: union_dedup(&vs),
        });
    }

    // `col IN ('a', 'b', ...)`
    if e.kind == AExprKind::AexprIn as i32 {
        let rhs = e.rexpr.as_deref()?;
        let items = match rhs.node.as_ref()? {
            NB::List(l) => &l.items,
            _ => return None,
        };
        let vs = items
            .iter()
            .filter_map(extract_ts_literal)
            .collect::<Vec<_>>();
        if vs.len() != items.len() || vs.is_empty() {
            return None;
        }
        return Some(ColRefinement {
            column: col,
            refined_ts: union_dedup(&vs),
        });
    }

    // `col = literal`
    if op_name(&e.name).as_deref() == Some("=") {
        let rhs = e.rexpr.as_deref().and_then(|n| n.node.as_ref())?;
        let lit = extract_ts_literal_from_node(rhs)?;
        return Some(ColRefinement {
            column: col,
            refined_ts: lit,
        });
    }
    None
}

fn extract_is_null(node: &NB) -> Option<String> {
    let NB::NullTest(nt) = node else { return None };
    if nt.nulltesttype != pg_query::protobuf::NullTestType::IsNull as i32 {
        return None;
    }
    let arg = nt.arg.as_deref()?.node.as_ref()?;
    extract_column_ref(arg)
}

/// Bare or table-qualified column ref → leaf identifier.
fn extract_column_ref(node: &NB) -> Option<String> {
    let cr = match node {
        NB::ColumnRef(c) => c,
        _ => return None,
    };
    let last = cr.fields.last()?.node.as_ref()?;
    match last {
        NB::String(s) => Some(s.sval.clone()),
        _ => None,
    }
}

fn op_name(names: &[Node]) -> Option<String> {
    let last = names.last()?.node.as_ref()?;
    match last {
        NB::String(s) => Some(s.sval.clone()),
        _ => None,
    }
}

fn extract_ts_literal(n: &Node) -> Option<String> {
    extract_ts_literal_from_node(n.node.as_ref()?)
}

fn extract_ts_literal_from_node(node: &NB) -> Option<String> {
    let c = match node {
        NB::AConst(c) => c,
        // TypeCast wrapping a literal: peel through to the constant.
        NB::TypeCast(tc) => return extract_ts_literal_from_node(tc.arg.as_deref()?.node.as_ref()?),
        _ => return None,
    };
    if c.isnull {
        return Some("null".to_string());
    }
    match c.val.as_ref()? {
        Val::Sval(s) => Some(format!("\"{}\"", s.sval.replace('"', "\\\""))),
        Val::Ival(i) => Some(i.ival.to_string()),
        Val::Fval(f) => Some(f.fval.clone()),
        Val::Boolval(b) => Some(b.boolval.to_string()),
        _ => None,
    }
}

fn union_dedup(parts: &[String]) -> String {
    let mut seen: Vec<&String> = Vec::new();
    for p in parts {
        if !seen.iter().any(|q| *q == p) {
            seen.push(p);
        }
    }
    seen.iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Multiple CHECKs on the same column AND together — the column has to
/// satisfy every refinement. We model that as a TS intersection but
/// keep it simple: if there's one refinement we drop the parens.
fn intersect_ts(parts: &[String]) -> String {
    if parts.len() == 1 {
        return parts[0].clone();
    }
    parts
        .iter()
        .map(|p| {
            if p.contains(" | ") && !p.starts_with('(') {
                format!("({})", p)
            } else {
                p.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" & ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reduce(check_body: &str) -> Option<ColRefinement> {
        reduce_predicate(check_body)
    }

    #[test]
    fn equality_literal_string() {
        assert_eq!(
            reduce("status = 'open'").unwrap(),
            ColRefinement {
                column: "status".into(),
                refined_ts: "\"open\"".into()
            },
        );
    }

    #[test]
    fn equality_literal_int() {
        assert_eq!(
            reduce("n = 7").unwrap(),
            ColRefinement {
                column: "n".into(),
                refined_ts: "7".into()
            },
        );
    }

    #[test]
    fn in_string_list() {
        assert_eq!(
            reduce("role IN ('owner', 'admin', 'member')")
                .unwrap()
                .refined_ts,
            "\"owner\" | \"admin\" | \"member\"",
        );
    }

    #[test]
    fn any_array_string() {
        assert_eq!(
            reduce("role = ANY (ARRAY['owner', 'admin'])")
                .unwrap()
                .refined_ts,
            "\"owner\" | \"admin\"",
        );
    }

    #[test]
    fn is_null_or_widens_with_null() {
        assert_eq!(
            reduce("status IS NULL OR status = 'open'")
                .unwrap()
                .refined_ts,
            "\"open\" | null",
        );
        // Order swapped — predicate-first OR is-null also recognised.
        assert_eq!(
            reduce("status = 'open' OR status IS NULL")
                .unwrap()
                .refined_ts,
            "\"open\" | null",
        );
    }

    #[test]
    fn cast_wrapper_around_literal_is_peeled() {
        assert_eq!(
            reduce("status = 'open'::text").unwrap().refined_ts,
            "\"open\"",
        );
    }

    #[test]
    fn qualified_col_ref_extracts_the_leaf() {
        assert_eq!(reduce("t.status = 'open'").unwrap().column, "status",);
    }

    #[test]
    fn bails_on_relational_operator() {
        assert!(reduce("n > 0").is_none());
    }

    #[test]
    fn bails_on_function_call() {
        assert!(reduce("length(slug) > 0").is_none());
    }

    #[test]
    fn bails_on_two_column_compare() {
        assert!(reduce("a = b").is_none());
    }

    #[test]
    fn intersect_two_refinements_emits_intersection() {
        let parts = vec!["\"a\" | \"b\"".to_string(), "\"a\" | \"c\"".to_string()];
        assert_eq!(intersect_ts(&parts), "(\"a\" | \"b\") & (\"a\" | \"c\")");
    }

    #[test]
    fn single_refinement_no_intersection() {
        assert_eq!(intersect_ts(&["\"a\"".to_string()]), "\"a\"");
    }

    #[test]
    fn strip_check_wrap_handles_whitespace() {
        assert_eq!(strip_check_wrap("CHECK (a = 1)"), Some("a = 1"));
        assert_eq!(strip_check_wrap("CHECK(a = 1)"), Some("a = 1"));
        assert_eq!(strip_check_wrap("  CHECK ( a = 1 ) "), Some("a = 1"));
        assert_eq!(strip_check_wrap("NOT CHECK (a = 1)"), None);
    }
}
