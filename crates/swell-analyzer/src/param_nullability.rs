//! Per-`$N` nullability via direct INSERT VALUES / UPDATE SET binding.
//!
//! Tightens to NOT NULL iff at least one textual reference to `$N`
//! binds *directly* to a NOT NULL column target. Other contexts
//! (WHERE, function args, expression wrappers, SELECT targets,
//! `coalesce($1, …)`) keep the param nullable since callers can
//! legitimately pass NULL there.

use crate::query::TableColRef;
use pg_query::protobuf::{node, InsertStmt, UpdateStmt};
use std::collections::{HashMap, HashSet};
use tokio_postgres::Client;

#[derive(Debug, Clone, Default)]
pub struct ParamInfo {
    pub nullable: bool,
    pub table_ref: Option<TableColRef>,
}

pub async fn infer(client: &Client, sql: &str, n_params: usize) -> Vec<ParamInfo> {
    let mut out: Vec<ParamInfo> = vec![
        ParamInfo {
            nullable: true,
            table_ref: None
        };
        n_params
    ];
    if n_params == 0 {
        return out;
    }

    let parsed = match pg_query::parse(sql) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("pg_query::parse failed for param nullability: {e}");
            return out;
        }
    };

    let mut bindings: Vec<Binding> = Vec::new();
    for raw in &parsed.protobuf.stmts {
        let Some(node) = raw.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
            continue;
        };
        match node {
            node::Node::InsertStmt(ins) => collect_insert(ins, &mut bindings),
            node::Node::UpdateStmt(upd) => collect_update(upd, &mut bindings),
            _ => {}
        }
    }
    if bindings.is_empty() {
        return out;
    }

    let attnotnull = resolve_attnotnull(client, &bindings).await;
    for b in &bindings {
        if b.param_index == 0 || b.param_index > n_params {
            continue;
        }
        let schema = normalize_schema(&b.schema);
        let key = (schema.clone(), b.table.clone(), b.column.clone());
        let entry = &mut out[b.param_index - 1];
        let Some(&not_null) = attnotnull.get(&key) else {
            continue;
        };
        if entry.table_ref.is_none() {
            entry.table_ref = Some(TableColRef {
                schema,
                table: b.table.clone(),
                column: b.column.clone(),
            });
        }
        // Strictest binding wins: `INSERT INTO t (a NOT NULL, b nullable)
        // VALUES ($1, $1)` requires non-null at the call site.
        if not_null {
            entry.nullable = false;
        }
    }
    out
}

async fn resolve_attnotnull(
    client: &Client,
    bindings: &[Binding],
) -> HashMap<(String, String, String), bool> {
    let unique: HashSet<(String, String, String)> = bindings
        .iter()
        .map(|b| {
            (
                normalize_schema(&b.schema),
                b.table.clone(),
                b.column.clone(),
            )
        })
        .collect();
    if unique.is_empty() {
        return HashMap::new();
    }
    let schemas: Vec<&str> = unique.iter().map(|(s, _, _)| s.as_str()).collect();
    let tables: Vec<&str> = unique.iter().map(|(_, t, _)| t.as_str()).collect();
    let columns: Vec<&str> = unique.iter().map(|(_, _, c)| c.as_str()).collect();
    let rows = match client
        .query(
            r#"
        WITH ask(schema, tbl, col) AS (
            SELECT * FROM unnest($1::text[], $2::text[], $3::text[])
        )
        SELECT ask.schema, ask.tbl, ask.col, a.attnotnull
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.tbl
        JOIN pg_attribute a ON a.attrelid = c.oid AND a.attname = ask.col
        WHERE a.attnum > 0 AND NOT a.attisdropped
        "#,
            &[&schemas, &tables, &columns],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("resolve_attnotnull: {e}");
            return HashMap::new();
        }
    };
    rows.iter()
        .map(|row| ((row.get(0), row.get(1), row.get(2)), row.get(3)))
        .collect()
}

struct Binding {
    /// 1-based, matches `$N`.
    param_index: usize,
    /// Empty = unqualified.
    schema: String,
    table: String,
    column: String,
}

fn collect_insert(ins: &InsertStmt, out: &mut Vec<Binding>) {
    let Some(rel) = ins.relation.as_ref() else {
        return;
    };
    // No explicit column list means we'd need the table's column order
    // to map params — skip rather than guess.
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

    let Some(select_box) = ins.select_stmt.as_ref() else {
        return;
    };
    let Some(node::Node::SelectStmt(sel)) = select_box.node.as_ref() else {
        return;
    };

    for row in &sel.values_lists {
        let Some(node::Node::List(list)) = row.node.as_ref() else {
            continue;
        };
        for (i, expr) in list.items.iter().enumerate() {
            if i >= cols.len() {
                continue;
            }
            let Some(node::Node::ParamRef(p)) = expr.node.as_ref() else {
                continue;
            };
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
    let Some(rel) = upd.relation.as_ref() else {
        return;
    };
    for tgt in &upd.target_list {
        let Some(node::Node::ResTarget(rt)) = tgt.node.as_ref() else {
            continue;
        };
        let Some(val) = rt.val.as_ref() else { continue };
        let Some(node::Node::ParamRef(p)) = val.node.as_ref() else {
            continue;
        };
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
