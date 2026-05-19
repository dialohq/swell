//! Structural inference of JSON-building expressions.
//!
//! Handles, for each top-level target in a SELECT:
//!
//!   - `jsonb_build_object('a', expr_a, 'b', expr_b, …)`     → `{ a: T_a; b: T_b }`
//!   - `json_build_object(...)`                               → same
//!   - `jsonb_agg(expr)` / `json_agg(expr)`                   → `T[]`
//!   - `to_jsonb(table_alias)` / `row_to_json(table_alias)`   → `{ col1: T1; … }`
//!
//! For column references inside these calls we query the catalog (via the
//! same `tokio_postgres::Client` the analyzer already uses) for the
//! column's OID and `attnotnull`. Anything more elaborate — sub-selects,
//! function calls other than the ones above, complex expressions — falls
//! back to `unknown`.
//!
//! When the topmost target isn't a JSON-building call this module returns
//! `None` for that target — caller keeps whatever the OID-based mapping
//! produced (which for `jsonb` columns is `unknown`, overridable via
//! config or `as "col: T"`).

use crate::ts_types::{Direction, TypeCatalog};
use pg_query::protobuf::{node, FuncCall, JoinExpr, RangeVar, SelectStmt};
use std::collections::HashMap;
use tokio_postgres::Client;

/// One inferred TS type per top-level target, in target-list order. `None`
/// means "no JSON shape inference applied — caller defers to OID mapping".
#[derive(Debug, Clone, Default)]
pub struct JsonShapeInferred {
    pub by_target: Vec<Option<String>>,
}

pub async fn infer_shapes(
    client: &Client,
    catalog: &TypeCatalog,
    sql: &str,
    n_targets: usize,
) -> JsonShapeInferred {
    let mut out = JsonShapeInferred {
        by_target: vec![None; n_targets],
    };

    let parsed = match pg_query::parse(sql) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("pg_query::parse failed for json_shape: {e}");
            return out;
        }
    };
    // Find the first SELECT stmt at top level.
    let select = match find_top_select(&parsed.protobuf) {
        Some(s) => s,
        None => return out,
    };

    let alias_map = build_alias_map(&select.from_clause);
    // Resolve alias name → table OID once per query.
    let alias_oids = resolve_alias_oids(client, &alias_map).await;

    for (i, t) in select.target_list.iter().take(n_targets).enumerate() {
        if let Some(node::Node::ResTarget(res)) = t.node.as_ref() {
            if let Some(val) = &res.val {
                // Only infer at the top level when the target IS a JSON-
                // building function. Bare column refs and other expressions
                // are typed via the OID/attnotnull/EXPLAIN path in lib.rs.
                if let Some(node::Node::FuncCall(fc)) = val.node.as_ref() {
                    if let Some(ts) = infer_func(client, catalog, &alias_oids, fc).await {
                        out.by_target[i] = Some(ts);
                    }
                }
            }
        }
    }
    out
}

// ----- AST helpers -----

fn find_top_select(p: &pg_query::protobuf::ParseResult) -> Option<&SelectStmt> {
    for raw in &p.stmts {
        if let Some(stmt) = &raw.stmt {
            if let Some(node::Node::SelectStmt(s)) = &stmt.node {
                return Some(s);
            }
        }
    }
    None
}

/// Walk from_clause + JoinExpr trees, returning a map of alias → relation.
/// `Relation` is `(schema, name)` with empty schema meaning unqualified.
fn build_alias_map(from_clause: &[pg_query::protobuf::Node]) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    for n in from_clause {
        walk_from(n, &mut out);
    }
    out
}

fn walk_from(n: &pg_query::protobuf::Node, out: &mut HashMap<String, (String, String)>) {
    match n.node.as_ref() {
        Some(node::Node::RangeVar(rv)) => insert_rangevar(rv, out),
        Some(node::Node::JoinExpr(j)) => {
            let j: &JoinExpr = j;
            if let Some(l) = &j.larg { walk_from(l, out); }
            if let Some(r) = &j.rarg { walk_from(r, out); }
        }
        _ => { /* sub-selects etc. — skip */ }
    }
}

fn insert_rangevar(rv: &RangeVar, out: &mut HashMap<String, (String, String)>) {
    let alias_name = rv.alias.as_ref().map(|a| a.aliasname.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rv.relname.clone());
    out.insert(alias_name, (rv.schemaname.clone(), rv.relname.clone()));
}

async fn resolve_alias_oids(
    client: &Client,
    alias_map: &HashMap<String, (String, String)>,
) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    if alias_map.is_empty() { return out; }
    let names: Vec<String> = alias_map.values().map(|(_, n)| n.clone()).collect();
    let schemas: Vec<String> = alias_map.values()
        .map(|(s, _)| if s.is_empty() { "public".to_string() } else { s.clone() })
        .collect();
    let rows_result = client.query(
        r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT n.nspname, c.relname, c.oid::bigint
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
        "#,
        &[&schemas, &names],
    ).await;
    let rows = match rows_result { Ok(r) => r, Err(_) => return out };
    let mut by_relname: HashMap<(String, String), u32> = HashMap::new();
    for row in &rows {
        let s: &str = row.get(0);
        let n: &str = row.get(1);
        let oid: i64 = row.get(2);
        by_relname.insert((s.to_string(), n.to_string()), oid as u32);
    }
    for (alias, (s, n)) in alias_map {
        let schema = if s.is_empty() { "public".to_string() } else { s.clone() };
        if let Some(oid) = by_relname.get(&(schema, n.clone())) {
            out.insert(alias.clone(), *oid);
        }
    }
    out
}

// ----- Per-expression inference -----

/// Infer a TS type for a node. Returns `None` when we don't recognise the
/// shape. Boxed because of recursion through async fn.
fn infer_node<'a>(
    client: &'a Client,
    catalog: &'a TypeCatalog,
    alias_oids: &'a HashMap<String, u32>,
    node: &'a pg_query::protobuf::Node,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>> {
    Box::pin(async move {
        match node.node.as_ref()? {
            node::Node::FuncCall(fc) => infer_func(client, catalog, alias_oids, fc).await,
            node::Node::ColumnRef(cr) => infer_column_ref(client, catalog, alias_oids, cr).await,
            node::Node::AConst(c) => Some(infer_a_const(c)),
            // CASE, COALESCE, etc. — defer to caller (they'll get the OID
            // type from RowDescription).
            _ => None,
        }
    })
}

async fn infer_func(
    client: &Client,
    catalog: &TypeCatalog,
    alias_oids: &HashMap<String, u32>,
    fc: &FuncCall,
) -> Option<String> {
    // Resolve the funcname to a *canonical* `pg_catalog` short name. Returns
    // `None` for any function that isn't a verified built-in — we never want
    // to apply the transform to a user-defined `public.jsonb_build_object`
    // (or whatever name shadows the catalog one) and silently produce a
    // structural type that no longer matches the runtime row.
    let canonical = resolve_to_safe_builtin(catalog, fc)?;
    match canonical {
        "jsonb_build_object" | "json_build_object" => {
            infer_build_object(client, catalog, alias_oids, &fc.args).await
        }
        "jsonb_agg" | "json_agg" => {
            let arg = fc.args.first()?;
            let inner = infer_node(client, catalog, alias_oids, arg).await
                .unwrap_or_else(|| "unknown".to_string());
            Some(format!("{}[]", inner))
        }
        "to_jsonb" | "row_to_json" => {
            infer_table_ref(client, catalog, alias_oids, fc.args.first()?).await
        }
        "jsonb_object_agg" | "json_object_agg" => {
            // jsonb_object_agg(key, value) → Record<string, V>.
            // Aggregates over (key, value) pairs and emits a JSON object.
            let val_node = fc.args.get(1)?;
            let val_ts = infer_node(client, catalog, alias_oids, val_node).await
                .unwrap_or_else(|| "unknown".to_string());
            Some(format!("Record<string, {}>", val_ts))
        }
        _ => None,
    }
}

/// Returns the canonical short name (e.g. `"jsonb_build_object"`) iff the
/// `FuncCall` refers to the genuine `pg_catalog` function. Cases:
///
/// 1. Fully-qualified `pg_catalog.X` — trusted; just strip the schema.
/// 2. Unqualified `X` — only accepted when the analyzer's connect-time probe
///    confirmed that `to_regproc('X')` resolves to the catalog OID under
///    the dev DB's `search_path` (no user-defined shadow). The set is in
///    `catalog.safe_builtin_procs`.
///
/// All other forms (schema-qualified with a non-`pg_catalog` schema; an
/// unqualified name that *is* shadowed) yield `None`, so inference falls
/// through to the default `Json` rendering. Worst case: opaque jsonb; never
/// a wrong structural type.
fn resolve_to_safe_builtin<'c>(
    catalog: &'c TypeCatalog,
    fc: &FuncCall,
) -> Option<&'c str> {
    let parts: Vec<String> = fc
        .funcname
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();

    let (schema, name) = match parts.as_slice() {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => return None,
    };

    match schema {
        // Explicit pg_catalog qualification — always trusted.
        Some("pg_catalog") => Some(catalog.safe_builtin_procs.get_key_value(name)?.0.as_str()),
        // Any other schema → user-defined; never apply the transform.
        Some(_) => None,
        // Unqualified — only safe if connect-time probe confirmed it resolves
        // to the catalog OID under the current search_path.
        None => Some(catalog.safe_builtin_procs.get_key_value(name)?.0.as_str()),
    }
}

async fn infer_build_object(
    client: &Client,
    catalog: &TypeCatalog,
    alias_oids: &HashMap<String, u32>,
    args: &[pg_query::protobuf::Node],
) -> Option<String> {
    if args.is_empty() || !args.len().is_multiple_of(2) {
        return None;
    }

    // Walk every (key, value) pair. If every key is a literal we emit a
    // structural object; if any key is dynamic we emit a homogeneous
    // `Record<string, V>` whose value type is the union of all observed
    // value types.
    let mut literal_fields: Vec<(String, String)> = Vec::with_capacity(args.len() / 2);
    let mut value_types: Vec<String> = Vec::with_capacity(args.len() / 2);
    let mut any_dynamic_key = false;

    let mut i = 0;
    while i < args.len() {
        let key_node = &args[i];
        let val_node = &args[i + 1];

        let key_lit: Option<String> = match key_node.node.as_ref() {
            Some(node::Node::AConst(c)) => match c.val.as_ref() {
                Some(pg_query::protobuf::a_const::Val::Sval(s)) => Some(s.sval.clone()),
                Some(pg_query::protobuf::a_const::Val::Ival(v)) => Some(v.ival.to_string()),
                _ => None,
            },
            _ => None,
        };

        let val_ts = infer_node(client, catalog, alias_oids, val_node).await
            .unwrap_or_else(|| "unknown".to_string());
        value_types.push(val_ts.clone());

        if let Some(k) = key_lit {
            literal_fields.push((k, val_ts));
        } else {
            any_dynamic_key = true;
        }
        i += 2;
    }

    if any_dynamic_key {
        // Deduplicate value types preserving first-seen order.
        let mut seen = std::collections::BTreeSet::new();
        let mut dedup: Vec<String> = Vec::new();
        for v in &value_types {
            if seen.insert(v.clone()) {
                dedup.push(v.clone());
            }
        }
        let union = if dedup.is_empty() { "unknown".to_string() } else { dedup.join(" | ") };
        Some(format!("Record<string, {}>", union))
    } else {
        let body: Vec<String> = literal_fields.iter()
            .map(|(k, v)| format!("{}: {}", quote_field(k), v))
            .collect();
        Some(format!("{{ {} }}", body.join("; ")))
    }
}

async fn infer_column_ref(
    client: &Client,
    catalog: &TypeCatalog,
    alias_oids: &HashMap<String, u32>,
    cr: &pg_query::protobuf::ColumnRef,
) -> Option<String> {
    let parts: Vec<String> = cr.fields.iter().filter_map(|n| {
        match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        }
    }).collect();
    if parts.is_empty() { return None; }

    let (alias, col) = if parts.len() == 1 {
        // Bare column — we don't know which table it's from without resolution.
        // Fall back to unknown; caller can override.
        return None;
    } else if parts.len() == 2 {
        (parts[0].clone(), parts[1].clone())
    } else {
        // schema.table.col → use last two
        let n = parts.len();
        (parts[n - 2].clone(), parts[n - 1].clone())
    };

    let table_oid = *alias_oids.get(&alias)?;
    column_type(client, catalog, table_oid, &col).await
}

async fn column_type(client: &Client, catalog: &TypeCatalog, table_oid: u32, col: &str) -> Option<String> {
    let row = client
        .query_opt(
            r#"
            SELECT a.atttypid::bigint, a.attnotnull, t.typname
            FROM pg_attribute a
            JOIN pg_type t ON t.oid = a.atttypid
            WHERE a.attrelid::bigint = $1 AND a.attname = $2 AND a.attnum > 0 AND NOT a.attisdropped
            "#,
            &[&(table_oid as i64), &col],
        )
        .await
        .ok()??;
    let oid: i64 = row.get(0);
    let notnull: bool = row.get(1);
    let name: String = row.get(2);
    let ty_string = catalog.render_oid(oid as u32, &name, Direction::Read);
    Some(if notnull { ty_string } else { format!("{} | null", ty_string) })
}

async fn infer_table_ref(
    client: &Client,
    catalog: &TypeCatalog,
    alias_oids: &HashMap<String, u32>,
    arg: &pg_query::protobuf::Node,
) -> Option<String> {
    let cr = match arg.node.as_ref()? {
        node::Node::ColumnRef(c) => c,
        _ => return None,
    };
    let parts: Vec<String> = cr.fields.iter().filter_map(|n| match n.node.as_ref()? {
        node::Node::String(s) => Some(s.sval.clone()),
        _ => None,
    }).collect();
    if parts.len() != 1 { return None; }
    let table_oid = *alias_oids.get(&parts[0])?;
    enumerate_table(client, catalog, table_oid).await
}

async fn enumerate_table(client: &Client, catalog: &TypeCatalog, table_oid: u32) -> Option<String> {
    let rows = client.query(
        r#"
        SELECT a.attname, a.atttypid::bigint, a.attnotnull, t.typname
        FROM pg_attribute a
        JOIN pg_type t ON t.oid = a.atttypid
        WHERE a.attrelid::bigint = $1 AND a.attnum > 0 AND NOT a.attisdropped
        ORDER BY a.attnum
        "#,
        &[&(table_oid as i64)],
    ).await.ok()?;
    if rows.is_empty() { return None; }
    let fields: Vec<String> = rows.iter().map(|row| {
        let name: String = row.get(0);
        let oid: i64 = row.get(1);
        let notnull: bool = row.get(2);
        let typname: String = row.get(3);
        let ty = catalog.render_oid(oid as u32, &typname, Direction::Read);
        let ty = if notnull { ty } else { format!("{} | null", ty) };
        format!("{}: {}", quote_field(&name), ty)
    }).collect();
    Some(format!("{{ {} }}", fields.join("; ")))
}

fn infer_a_const(c: &pg_query::protobuf::AConst) -> String {
    use pg_query::protobuf::a_const::Val;
    if c.isnull {
        return "null".to_string();
    }
    match c.val.as_ref() {
        Some(Val::Ival(_)) | Some(Val::Fval(_)) => "number".to_string(),
        Some(Val::Sval(_)) => "string".to_string(),
        Some(Val::Boolval(_)) => "boolean".to_string(),
        _ => "unknown".to_string(),
    }
}

fn quote_field(name: &str) -> String {
    let simple = !name.is_empty()
        && name.chars().next().unwrap().is_ascii_alphabetic()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if simple { name.to_string() } else { format!("\"{}\"", name.replace('"', "\\\"")) }
}
