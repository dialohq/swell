//! Per-`$N` nullability inference.
//!
//! Postgres's PARSE/DESCRIBE returns only the type OID for each
//! prepared-statement parameter — no nullability. We default every `$N`
//! to nullable (callers may legitimately pass NULL), then tighten to
//! non-nullable iff **at least one** textual reference to `$N` binds
//! *directly* to a NOT NULL column. Two contexts qualify as direct
//! binding:
//!
//!   - `INSERT INTO t (a, b, …) VALUES ($1, $2, …)` — the i-th `$N` in
//!     a row pairs with the i-th column in the column list.
//!   - `UPDATE t SET col = $N` — the SET target's column.
//!
//! Anything else (WHERE clauses, function arguments, expressions that
//! merely contain `$N`, SELECT targets) keeps the param nullable.
//! `coalesce($1, …)` keeps it nullable too: even if the surrounding
//! column is NOT NULL, the user can pass null and `coalesce` will
//! substitute the default.
//!
//! Rationale for "at least one" rather than "all":
//! `INSERT INTO t (a, b) VALUES ($1, $1)` with `a` NOT NULL and `b`
//! nullable — `$1` going into `a` forbids null at the call site, so
//! the strictest binding wins.

use crate::catalog::fetch_attnotnull;
use pg_query::protobuf::{node, InsertStmt, UpdateStmt};
use std::collections::{HashMap, HashSet};
use tokio_postgres::Client;

pub async fn infer(client: &Client, sql: &str, n_params: usize) -> Vec<bool> {
    let mut nullable = vec![true; n_params];
    if n_params == 0 {
        return nullable;
    }

    let parsed = match pg_query::parse(sql) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("pg_query::parse failed for param nullability: {e}");
            return nullable;
        }
    };

    let mut bindings: Vec<Binding> = Vec::new();
    for raw in &parsed.protobuf.stmts {
        let Some(stmt) = raw.stmt.as_ref() else { continue };
        let Some(node) = stmt.node.as_ref() else { continue };
        match node {
            node::Node::InsertStmt(ins) => collect_insert(ins, &mut bindings),
            node::Node::UpdateStmt(upd) => collect_update(upd, &mut bindings),
            _ => {}
        }
    }
    if bindings.is_empty() {
        return nullable;
    }

    let table_oids = resolve_table_oids(client, &bindings).await;
    if table_oids.is_empty() {
        return nullable;
    }

    let attnums = resolve_attnums(client, &bindings, &table_oids).await;
    if attnums.is_empty() {
        return nullable;
    }

    let pair_vec: Vec<(u32, i16)> = attnums
        .iter()
        .map(|((tbl, _col), attnum)| (*tbl, *attnum))
        .collect();
    let attnotnull = fetch_attnotnull(client, &pair_vec)
        .await
        .ok()
        .unwrap_or_default();

    for b in &bindings {
        if b.param_index == 0 || b.param_index > n_params {
            continue;
        }
        let key = (normalize_schema(&b.schema), b.table.clone());
        let Some(&table_oid) = table_oids.get(&key) else { continue };
        let Some(&attnum) = attnums.get(&(table_oid, b.column.clone())) else { continue };
        if attnotnull
            .get(&(table_oid, attnum))
            .copied()
            .unwrap_or(false)
        {
            nullable[b.param_index - 1] = false;
        }
    }
    nullable
}

struct Binding {
    param_index: usize, // 1-based, matches $N
    schema: String,     // empty = unqualified
    table: String,
    column: String,
}

fn collect_insert(ins: &InsertStmt, out: &mut Vec<Binding>) {
    let Some(rel) = ins.relation.as_ref() else { return };
    // INSERT without an explicit column list (`INSERT INTO t VALUES (…)`)
    // would require us to know the table's column order to map params —
    // skip rather than guess.
    let cols: Vec<String> = ins
        .cols
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::ResTarget(rt) => Some(rt.name.clone()),
            _ => None,
        })
        .collect();
    if cols.is_empty() {
        return;
    }

    let Some(select_box) = ins.select_stmt.as_ref() else { return };
    let Some(select_node) = select_box.node.as_ref() else { return };
    let node::Node::SelectStmt(sel) = select_node else { return };

    for row in &sel.values_lists {
        let Some(node::Node::List(list)) = row.node.as_ref() else { continue };
        for (i, expr) in list.items.iter().enumerate() {
            if i >= cols.len() {
                continue;
            }
            let Some(node::Node::ParamRef(p)) = expr.node.as_ref() else { continue };
            out.push(Binding {
                param_index: p.number as usize,
                schema: rel.schemaname.clone(),
                table: rel.relname.clone(),
                column: cols[i].clone(),
            });
        }
    }
}

fn collect_update(upd: &UpdateStmt, out: &mut Vec<Binding>) {
    let Some(rel) = upd.relation.as_ref() else { return };
    for tgt in &upd.target_list {
        let Some(node::Node::ResTarget(rt)) = tgt.node.as_ref() else { continue };
        let Some(val) = rt.val.as_ref() else { continue };
        let Some(node::Node::ParamRef(p)) = val.node.as_ref() else { continue };
        out.push(Binding {
            param_index: p.number as usize,
            schema: rel.schemaname.clone(),
            table: rel.relname.clone(),
            column: rt.name.clone(),
        });
    }
}

fn normalize_schema(s: &str) -> String {
    if s.is_empty() {
        "public".to_string()
    } else {
        s.to_string()
    }
}

async fn resolve_table_oids(
    client: &Client,
    bindings: &[Binding],
) -> HashMap<(String, String), u32> {
    let mut unique: HashSet<(String, String)> = HashSet::new();
    for b in bindings {
        unique.insert((normalize_schema(&b.schema), b.table.clone()));
    }
    let mut out = HashMap::new();
    if unique.is_empty() {
        return out;
    }
    let schemas: Vec<String> = unique.iter().map(|(s, _)| s.clone()).collect();
    let names: Vec<String> = unique.iter().map(|(_, n)| n.clone()).collect();
    let rows = match client
        .query(
            r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT n.nspname, c.relname, c.oid::bigint
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = ask.name
        "#,
            &[&schemas, &names],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("resolve_table_oids: {e}");
            return out;
        }
    };
    for row in &rows {
        let s: String = row.get(0);
        let n: String = row.get(1);
        let oid: i64 = row.get(2);
        out.insert((s, n), oid as u32);
    }
    out
}

async fn resolve_attnums(
    client: &Client,
    bindings: &[Binding],
    table_oids: &HashMap<(String, String), u32>,
) -> HashMap<(u32, String), i16> {
    let mut tables: HashSet<u32> = HashSet::new();
    let mut columns: HashSet<String> = HashSet::new();
    for b in bindings {
        if let Some(&oid) = table_oids.get(&(normalize_schema(&b.schema), b.table.clone())) {
            tables.insert(oid);
            columns.insert(b.column.clone());
        }
    }
    let mut out = HashMap::new();
    if tables.is_empty() || columns.is_empty() {
        return out;
    }
    let tables_i64: Vec<i64> = tables.iter().map(|x| *x as i64).collect();
    let columns_vec: Vec<String> = columns.into_iter().collect();
    let rows = match client
        .query(
            r#"
        SELECT a.attrelid::bigint, a.attname, a.attnum
        FROM pg_attribute a
        WHERE a.attrelid::bigint = ANY($1::bigint[])
          AND a.attname = ANY($2::text[])
          AND a.attnum > 0 AND NOT a.attisdropped
        "#,
            &[&tables_i64, &columns_vec],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("resolve_attnums: {e}");
            return out;
        }
    };
    for row in &rows {
        let t: i64 = row.get(0);
        let n: String = row.get(1);
        let a: i16 = row.get(2);
        out.insert((t as u32, n), a);
    }
    out
}
