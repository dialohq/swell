//! Shared pg_query AST helpers used by multiple analyzer modules.

use pg_query::protobuf::{node::Node as NB, FuncCall, Node, ParseResult, RangeVar, SelectStmt};

/// Visit `node` and recurse through any `JoinExpr` arms. Leaf nodes
/// (RangeVar, RangeSubselect, RangeFunction, …) are passed to `f`;
/// the caller pattern-matches on the variants it cares about.
pub fn walk_from_tree<F: FnMut(&Node)>(node: &Node, f: &mut F) {
    f(node);
    if let Some(NB::JoinExpr(je)) = node.node.as_ref() {
        if let Some(l) = je.larg.as_deref() {
            walk_from_tree(l, f);
        }
        if let Some(r) = je.rarg.as_deref() {
            walk_from_tree(r, f);
        }
    }
}

/// Top-level `SelectStmt` for every parsed statement.
pub fn select_stmts(p: &ParseResult) -> impl Iterator<Item = &SelectStmt> {
    p.stmts
        .iter()
        .filter_map(|raw| match raw.stmt.as_deref()?.node.as_ref()? {
            NB::SelectStmt(s) => Some(s.as_ref()),
            _ => None,
        })
}

/// Unqualified last component of a function name (`pg_catalog.foo` → `foo`).
pub fn funcname_last(fc: &FuncCall) -> Option<&str> {
    match fc.funcname.last()?.node.as_ref()? {
        NB::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

/// String contents of a `[Node]` slice (skips non-String nodes).
pub fn string_parts(nodes: &[Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            NB::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

/// `ResTarget.val` for a target-list entry. None when the entry isn't
/// a ResTarget or doesn't carry a value.
pub fn restarget_val(n: &Node) -> Option<&Node> {
    match n.node.as_ref()? {
        NB::ResTarget(rt) => rt.val.as_deref(),
        _ => None,
    }
}

/// User-written alias on a `RangeVar`, falling back to the relation
/// name when no alias was given.
pub fn range_var_alias(rv: &RangeVar) -> String {
    rv.alias
        .as_ref()
        .map(|a| a.aliasname.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&rv.relname)
        .to_string()
}

/// Empty schema name resolves to `public`.
pub fn norm_schema(s: &str) -> &str {
    if s.is_empty() {
        "public"
    } else {
        s
    }
}

/// `\"x\"` if `name` needs quoting, else bare. Robust TS escape on the
/// quoted form — handles arbitrary identifier text.
pub fn quote_field(name: &str) -> String {
    let simple = !name.is_empty()
        && name.chars().next().unwrap().is_ascii_alphabetic()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if simple {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\\\""))
    }
}
