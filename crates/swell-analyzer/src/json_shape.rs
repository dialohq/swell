//! Structural inference of JSON-building expressions.
//!
//! For each top-level target we recognise:
//!   * `jsonb_build_object(...)` / `json_build_object(...)`
//!   * `jsonb_agg(expr)` / `json_agg(expr)`            → `T[]`
//!   * `to_jsonb(alias)` / `row_to_json(alias)`        → `{ col: T; … }`
//!   * `jsonb_object_agg(k, v)` / `json_object_agg`    → `Record<string, V>`
//!
//! Anything else falls back to the OID-based mapping (`unknown` for
//! opaque jsonb, overridable via config or `as "col: T"`).

use crate::ts_types::{Direction, TypeCatalog};
use pg_query::protobuf::{node, FuncCall, RangeVar, SelectStmt};
use std::collections::HashMap;
use tokio_postgres::Client;

/// One inferred TS type per top-level target. `None` = defer to the
/// OID-based mapping.
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
    let mut out = JsonShapeInferred { by_target: vec![None; n_targets] };
    let parsed = match pg_query::parse(sql) {
        Ok(p) => p,
        Err(e) => { tracing::debug!("pg_query::parse failed for json_shape: {e}"); return out; }
    };
    let Some(select) = find_top_select(&parsed.protobuf) else { return out };
    let alias_oids = resolve_alias_oids(client, &build_alias_map(&select.from_clause)).await;

    for (i, t) in select.target_list.iter().take(n_targets).enumerate() {
        let Some(node::Node::ResTarget(res)) = t.node.as_ref() else { continue };
        let Some(val) = &res.val else { continue };
        // Only infer at the top level when the target IS a JSON-building
        // FuncCall. Bare column refs flow through the OID path in lib.rs.
        let Some(node::Node::FuncCall(fc)) = val.node.as_ref() else { continue };
        if let Some(ts) = infer_func(client, catalog, &alias_oids, fc).await {
            out.by_target[i] = Some(ts);
        }
    }
    out
}

fn find_top_select(p: &pg_query::protobuf::ParseResult) -> Option<&SelectStmt> {
    p.stmts.iter().find_map(|raw| match &raw.stmt.as_ref()?.node {
        Some(node::Node::SelectStmt(s)) => Some(s.as_ref()),
        _ => None,
    })
}

fn build_alias_map(from_clause: &[pg_query::protobuf::Node]) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    for n in from_clause { walk_from(n, &mut out); }
    out
}

fn walk_from(n: &pg_query::protobuf::Node, out: &mut HashMap<String, (String, String)>) {
    match n.node.as_ref() {
        Some(node::Node::RangeVar(rv)) => insert_rangevar(rv, out),
        Some(node::Node::JoinExpr(j)) => {
            if let Some(l) = &j.larg { walk_from(l, out); }
            if let Some(r) = &j.rarg { walk_from(r, out); }
        }
        _ => {}
    }
}

fn insert_rangevar(rv: &RangeVar, out: &mut HashMap<String, (String, String)>) {
    let alias = rv.alias.as_ref().map(|a| a.aliasname.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rv.relname.clone());
    out.insert(alias, (rv.schemaname.clone(), rv.relname.clone()));
}

async fn resolve_alias_oids(
    client: &Client, alias_map: &HashMap<String, (String, String)>,
) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    if alias_map.is_empty() { return out; }
    let schema_of = |s: &String| if s.is_empty() { "public".to_string() } else { s.clone() };
    let schemas: Vec<String> = alias_map.values().map(|(s, _)| schema_of(s)).collect();
    let names: Vec<String> = alias_map.values().map(|(_, n)| n.clone()).collect();
    let Ok(rows) = client.query(
        r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT n.nspname, c.relname, c.oid::bigint
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
        "#,
        &[&schemas, &names],
    ).await else { return out };
    let by_relname: HashMap<(String, String), u32> = rows.iter()
        .map(|row| ((row.get(0), row.get(1)), row.get::<_, i64>(2) as u32))
        .collect();
    for (alias, (s, n)) in alias_map {
        if let Some(&oid) = by_relname.get(&(schema_of(s), n.clone())) {
            out.insert(alias.clone(), oid);
        }
    }
    out
}

// ---------- Per-expression inference ----------

/// Boxed for async recursion through `FuncCall` arguments.
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
            node::Node::TypeCast(tc) => infer_type_cast(catalog, tc),
            _ => None,
        }
    })
}

/// `expr::T` → the catalog's render for `T` (handles domains, enums,
/// composites; falls back to built-in name mapping).
fn infer_type_cast(catalog: &TypeCatalog, tc: &pg_query::protobuf::TypeCast) -> Option<String> {
    let names: Vec<String> = tc.type_name.as_ref()?.names.iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();
    let name = names.last()?;
    Some(catalog.render_oid(0, name, Direction::Read))
}

async fn infer_func(
    client: &Client,
    catalog: &TypeCatalog,
    alias_oids: &HashMap<String, u32>,
    fc: &FuncCall,
) -> Option<String> {
    // Resolve to a canonical pg_catalog short name — refuses user-defined
    // shadows so we never produce a structural type that doesn't match
    // the runtime row.
    let canonical = resolve_to_safe_builtin(catalog, fc)?;
    match canonical {
        "jsonb_build_object" | "json_build_object" =>
            infer_build_object(client, catalog, alias_oids, &fc.args).await,
        "jsonb_agg" | "json_agg" => {
            let arg = fc.args.first()?;
            let inner = infer_node(client, catalog, alias_oids, arg).await
                .unwrap_or_else(|| "unknown".to_string());
            Some(format!("{}[]", inner))
        }
        "to_jsonb" | "row_to_json" =>
            infer_table_ref(client, catalog, alias_oids, fc.args.first()?).await,
        "jsonb_object_agg" | "json_object_agg" => {
            let val_ts = infer_node(client, catalog, alias_oids, fc.args.get(1)?).await
                .unwrap_or_else(|| "unknown".to_string());
            Some(format!("Record<string, {}>", val_ts))
        }
        _ => None,
    }
}

/// Look up the function's declared return type. Multiple overloads:
/// take the first match by name (and schema if qualified). Function
/// returns are nullable in PG — no signature-level NOT NULL.
async fn infer_user_func_return(
    client: &Client, catalog: &TypeCatalog, fc: &FuncCall,
) -> Option<String> {
    let names = funcname_parts(fc);
    let (schema, name) = funcname_split(&names)?;
    let row = client.query_opt(
        r#"
        SELECT t.oid::oid, t.typname
        FROM pg_proc p
        JOIN pg_type t ON t.oid = p.prorettype
        WHERE p.proname = $1
          AND ($2::text IS NOT NULL
               OR p.pronamespace = ANY(current_schemas(true)::regnamespace[]))
          AND ($2::text IS NULL
               OR p.pronamespace = (SELECT oid FROM pg_namespace WHERE nspname = $2))
        LIMIT 1
        "#,
        &[&name, &schema],
    ).await.ok()??;
    let oid: u32 = row.get(0);
    let typname: String = row.get(1);
    let base = catalog.render_oid(oid, &typname, Direction::Read);
    Some(format!("{} | null", base))
}

/// Canonical `pg_catalog` short name iff `fc` refers to the verified
/// builtin. Fully-qualified `pg_catalog.X` is trusted. Unqualified `X`
/// is only accepted when the connect-time probe confirmed no
/// user-defined shadow. Any other shape yields `None` and inference
/// falls through to the default Json rendering — worst case opaque,
/// never wrong.
fn resolve_to_safe_builtin<'c>(catalog: &'c TypeCatalog, fc: &FuncCall) -> Option<&'c str> {
    let parts = funcname_parts(fc);
    let (schema, name) = funcname_split(&parts)?;
    match schema {
        Some("pg_catalog") | None =>
            Some(catalog.safe_builtin_procs.get_key_value(name)?.0.as_str()),
        Some(_) => None,
    }
}

fn funcname_parts(fc: &FuncCall) -> Vec<String> {
    fc.funcname.iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

fn funcname_split(parts: &[String]) -> Option<(Option<&str>, &str)> {
    match parts {
        [name] => Some((None, name.as_str())),
        [schema, name] => Some((Some(schema.as_str()), name.as_str())),
        _ => None,
    }
}

enum KeyKind {
    Literal(String),
    Dynamic,
}

/// `jsonb_build_object(k1, v1, k2, v2, …)` →
///   - All literal keys           → `{ k1: V1; k2: V2; … }`
///   - Mixed with literal keys after all dynamic ones →
///     `{ [k: string]: V; k1: T1; … }` (literal fields survive PG's
///     last-occurrence-wins semantics).
///   - Literal preceding dynamic → `Record<string, V>` (literal could
///     be overwritten).
async fn infer_build_object(
    client: &Client, catalog: &TypeCatalog,
    alias_oids: &HashMap<String, u32>, args: &[pg_query::protobuf::Node],
) -> Option<String> {
    if args.is_empty() || !args.len().is_multiple_of(2) { return None; }
    let mut pairs: Vec<(KeyKind, String)> = Vec::with_capacity(args.len() / 2);
    let mut i = 0;
    while i < args.len() {
        let key_lit = match args[i].node.as_ref() {
            Some(node::Node::AConst(c)) => match c.val.as_ref() {
                Some(pg_query::protobuf::a_const::Val::Sval(s)) => Some(s.sval.clone()),
                Some(pg_query::protobuf::a_const::Val::Ival(v)) => Some(v.ival.to_string()),
                _ => None,
            },
            _ => None,
        };
        // In value position, fall back to `pg_proc.prorettype` for
        // user-defined function calls so `'k', my_func(...)` lands as
        // the function's declared return type (nullable) rather than
        // `unknown`.
        let val_node = &args[i + 1];
        let val_ts = match infer_node(client, catalog, alias_oids, val_node).await {
            Some(t) => t,
            None => match val_node.node.as_ref() {
                Some(node::Node::FuncCall(fc)) =>
                    infer_user_func_return(client, catalog, fc).await
                        .unwrap_or_else(|| "unknown".to_string()),
                _ => "unknown".to_string(),
            }
        };
        pairs.push((key_lit.map(KeyKind::Literal).unwrap_or(KeyKind::Dynamic), val_ts));
        i += 2;
    }
    let any_dynamic = pairs.iter().any(|(k, _)| matches!(k, KeyKind::Dynamic));
    let last_dynamic_idx = pairs.iter().rposition(|(k, _)| matches!(k, KeyKind::Dynamic));

    if !any_dynamic {
        let body: Vec<String> = pairs.iter()
            .filter_map(|(k, v)| match k {
                KeyKind::Literal(name) => Some(format!("{}: {}", quote_field(name), v)),
                _ => None,
            })
            .collect();
        return Some(format!("{{ {} }}", body.join("; ")));
    }

    // Broad case (dynamic) first, narrow constants after — reads
    // naturally: `Record<string, string | "owner" | "admin">`.
    let dedup_union = |groups: &[&[&str]]| -> String {
        let mut seen = std::collections::BTreeSet::new();
        let mut out: Vec<String> = Vec::new();
        for g in groups {
            for v in *g {
                if seen.insert(v.to_string()) { out.push(v.to_string()); }
            }
        }
        if out.is_empty() { "unknown".into() } else { out.join(" | ") }
    };
    let dyn_vals: Vec<&str> = pairs.iter()
        .filter_map(|(k, v)| matches!(k, KeyKind::Dynamic).then(|| v.as_str())).collect();
    let lit_vals: Vec<&str> = pairs.iter()
        .filter_map(|(k, v)| matches!(k, KeyKind::Literal(_)).then(|| v.as_str())).collect();

    let constants_all_last = last_dynamic_idx
        .is_some_and(|idx| pairs[..=idx].iter().all(|(k, _)| matches!(k, KeyKind::Dynamic)));

    if constants_all_last {
        let mut body = vec![format!("[k: string]: {}", dedup_union(&[&dyn_vals]))];
        for (k, v) in &pairs {
            if let KeyKind::Literal(name) = k {
                body.push(format!("{}: {}", quote_field(name), v));
            }
        }
        return Some(format!("{{ {} }}", body.join("; ")));
    }
    Some(format!("Record<string, {}>", dedup_union(&[&dyn_vals, &lit_vals])))
}

async fn infer_column_ref(
    client: &Client, catalog: &TypeCatalog,
    alias_oids: &HashMap<String, u32>, cr: &pg_query::protobuf::ColumnRef,
) -> Option<String> {
    let parts: Vec<String> = cr.fields.iter().filter_map(|n| match n.node.as_ref()? {
        node::Node::String(s) => Some(s.sval.clone()),
        _ => None,
    }).collect();
    let (alias, col) = match parts.as_slice() {
        // Bare column — unknown which table.
        [_] => return None,
        [a, c] => (a.clone(), c.clone()),
        [_, .., a, c] => (a.clone(), c.clone()),
        _ => return None,
    };
    let table_oid = *alias_oids.get(&alias)?;
    column_type(client, catalog, table_oid, &col).await
}

/// Fetch `(attname, oid, notnull, typname)` for one column or every
/// column of `table_oid` (ordered by attnum when `col` is `None`).
async fn fetch_attrs(
    client: &Client, table_oid: u32, col: Option<&str>,
) -> Vec<(String, u32, bool, String)> {
    let oid = table_oid as i64;
    let rows = match col {
        Some(c) => client.query(
            r#"SELECT a.attname, a.atttypid::bigint, a.attnotnull, t.typname
               FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
               WHERE a.attrelid::bigint = $1 AND a.attname = $2
                 AND a.attnum > 0 AND NOT a.attisdropped"#,
            &[&oid, &c],
        ).await,
        None => client.query(
            r#"SELECT a.attname, a.atttypid::bigint, a.attnotnull, t.typname
               FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
               WHERE a.attrelid::bigint = $1 AND a.attnum > 0 AND NOT a.attisdropped
               ORDER BY a.attnum"#,
            &[&oid],
        ).await,
    };
    let Ok(rows) = rows else { return Vec::new() };
    rows.iter().map(|row| {
        let oid: i64 = row.get(1);
        (row.get(0), oid as u32, row.get(2), row.get(3))
    }).collect()
}

fn render_attr(catalog: &TypeCatalog, oid: u32, typname: &str, notnull: bool) -> String {
    let ty = catalog.render_oid(oid, typname, Direction::Read);
    if notnull { ty } else { format!("{} | null", ty) }
}

async fn column_type(client: &Client, catalog: &TypeCatalog, table_oid: u32, col: &str) -> Option<String> {
    let (_, oid, nn, typname) = fetch_attrs(client, table_oid, Some(col)).await.into_iter().next()?;
    Some(render_attr(catalog, oid, &typname, nn))
}

async fn infer_table_ref(
    client: &Client, catalog: &TypeCatalog,
    alias_oids: &HashMap<String, u32>, arg: &pg_query::protobuf::Node,
) -> Option<String> {
    let node::Node::ColumnRef(cr) = arg.node.as_ref()? else { return None };
    let parts: Vec<String> = cr.fields.iter().filter_map(|n| match n.node.as_ref()? {
        node::Node::String(s) => Some(s.sval.clone()),
        _ => None,
    }).collect();
    if parts.len() != 1 { return None; }
    let table_oid = *alias_oids.get(&parts[0])?;
    let attrs = fetch_attrs(client, table_oid, None).await;
    if attrs.is_empty() { return None; }
    let fields: Vec<String> = attrs.iter().map(|(name, oid, nn, typname)| {
        format!("{}: {}", quote_field(name), render_attr(catalog, *oid, typname, *nn))
    }).collect();
    Some(format!("{{ {} }}", fields.join("; ")))
}

fn infer_a_const(c: &pg_query::protobuf::AConst) -> String {
    use pg_query::protobuf::a_const::Val;
    if c.isnull { return "null".to_string(); }
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
