//! LEFT JOIN over a nullable foreign key → discriminated row variants.
//!
//! When a query `LEFT JOIN`s a parent relation on `parent.key =
//! child.fk`, and `child.fk` is a *nullable* column carrying a real
//! single-column FK to `parent(key)`, referential integrity makes the
//! parent's presence in any output row *exactly* `child.fk IS NOT
//! NULL`:
//!
//!   * `child.fk` non-null  ⇒ the FK guarantees a matching parent row,
//!     so every `parent.*` output column is present (non-null).
//!   * `child.fk` null      ⇒ `parent.key = NULL` never matches, so
//!     every `parent.*` output column is `null`.
//!
//! On its own that just re-derives the column's nullability. The value
//! comes from *correlating* it with a row-level CHECK on the child. An
//! exclusive-arc child —
//!
//!   CHECK (num_nonnulls(comment_id, alert_id) = 1)
//!
//! — pins each fk to null-or-non-null per variant, so
//!
//!   SELECT c.body, a.message
//!   FROM notifications n
//!   LEFT JOIN comments c ON c.id = n.comment_id
//!   LEFT JOIN alerts   a ON a.id = n.alert_id
//!
//! becomes `{ body: Comments["body"]; message: null }
//!         | { body: null; message: Alerts["message"] }` — the absent
//! arc is provably null in each variant.
//!
//! ### The "presence condition" abstraction
//!
//! Each LEFT-joined relation is reduced to a single fact: the child
//! column that must be non-null for the parent to be present. Only a
//! bare `parent.key = child.fk` ON clause yields one (referential
//! integrity gives `child.fk IS NOT NULL`). Anything trickier — extra
//! `AND`/`OR` conjuncts, filters, non-equi joins, `USING`, bare
//! (unqualified) column refs — produces no presence condition and the
//! join is skipped. `join_presence` is the single place that knows how
//! to read an ON clause, so widening the set of recognised shapes is
//! local.
//!
//! ### Soundness gate
//!
//! Codegen renders a variant's non-overridden columns as *non-null*
//! (they're known present in that arm). That is only safe if every
//! join-*widened* output column (inherently `NOT NULL`, nulled solely
//! by an outer join) belongs to one of the parent relations this
//! module controls. A widened column from an unrelated join would be
//! silently forced non-null — so its presence makes us bail entirely.

use crate::checks::RowRefinement;
use crate::pg_util::{range_var_alias, select_stmts, string_parts, walk_from_tree};
use crate::plan::PlanWalk;
use crate::query::{RowVariant, TableColRef};
use pg_query::protobuf::{node::Node as NB, AExprKind, JoinExpr, JoinType, Node};
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio_postgres::Client;

/// One LEFT-joined parent relation whose presence is exactly
/// `fk IS NOT NULL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkLink {
    /// The child column carrying the FK (`notifications.comment_id`).
    pub fk: TableColRef,
    /// The child's query alias (`n`).
    pub child_alias: String,
    /// The parent's query alias (`c`) — the nullable side of the join.
    pub parent_alias: String,
}

/// One output column, paired with the source alias it was resolved
/// from and whether an outer join widened it.
#[derive(Debug, Clone)]
pub struct OutputCol {
    pub name: String,
    /// Source relation alias (`ResolvedCol.alias`); empty if the output
    /// isn't a plain column ref.
    pub alias: String,
    /// Base `(schema, table)` for a base-column ref.
    pub table: Option<(String, String)>,
    /// Base column name.
    pub column: Option<String>,
    /// Non-null TS rendering of the column's own type (no `| null`),
    /// used to pin a selected fk column in its present variant.
    pub ts_type: String,
    /// `true` when the column is inherently `NOT NULL` but the final
    /// verdict is nullable — i.e. an outer join above widened it.
    pub widened: bool,
}

/// Detect qualifying LEFT-join FK relationships and synthesise the
/// discriminated row variants. Returns empty when nothing qualifies.
pub async fn left_join_fk_variants(
    client: &Client,
    sql: &str,
    plan: &PlanWalk,
    out_cols: &[OutputCol],
    row_refs: &HashMap<(String, String), Vec<RowRefinement>>,
) -> Vec<RowVariant> {
    let cands = collect_left_join_cands(sql, plan);
    if cands.is_empty() {
        return Vec::new();
    }
    let links = validate_fks(client, &cands).await;
    if links.is_empty() {
        return Vec::new();
    }
    synth(&links, out_cols, row_refs)
}

// ---------- SQL: candidate LEFT joins ----------

#[derive(Debug, Clone)]
struct JoinCand {
    parent_alias: String,
    parent_table: (String, String),
    parent_col: String,
    child_alias: String,
    child_table: (String, String),
    child_col: String,
}

fn collect_left_join_cands(sql: &str, plan: &PlanWalk) -> Vec<JoinCand> {
    let mut out = Vec::new();
    let Ok(parsed) = pg_query::parse(sql) else {
        return out;
    };
    for select in select_stmts(&parsed.protobuf) {
        for from in &select.from_clause {
            walk_from_tree(from, &mut |n| {
                if let Some(NB::JoinExpr(je)) = n.node.as_ref() {
                    if let Some(c) = join_candidate(je, plan) {
                        out.push(c);
                    }
                }
            });
        }
    }
    out
}

fn join_candidate(je: &JoinExpr, plan: &PlanWalk) -> Option<JoinCand> {
    if je.jointype != JoinType::JoinLeft as i32 {
        return None;
    }
    // The joined (right) operand must be a plain table — the nullable
    // side. Subqueries / nested joins don't carry an FK we can read.
    let NB::RangeVar(rv) = je.rarg.as_deref()?.node.as_ref()? else {
        return None;
    };
    let parent_alias = range_var_alias(rv);
    let (parent_col, child_alias, child_col) = join_presence(je.quals.as_deref()?, &parent_alias)?;

    // The parent must actually be the outer-join-widened side.
    if !alias_in(&plan.nullable_aliases, &parent_alias) {
        return None;
    }
    let parent_table = plan
        .alias_to_table
        .get(&parent_alias)
        .cloned()
        .unwrap_or_else(|| (norm(&rv.schemaname), rv.relname.clone()));
    let child_table = plan.alias_to_table.get(&child_alias).cloned()?;
    Some(JoinCand {
        parent_alias,
        parent_table,
        parent_col,
        child_alias,
        child_table,
        child_col,
    })
}

/// Reduce an ON clause to `(parent_col, child_alias, child_col)` — the
/// presence condition `child.child_col IS NOT NULL`. Only a single
/// `parent.key = child.fk` equality (either operand order) qualifies.
fn join_presence(quals: &Node, parent_alias: &str) -> Option<(String, String, String)> {
    let NB::AExpr(e) = quals.node.as_ref()? else {
        return None;
    };
    if e.kind != AExprKind::AexprOp as i32 || op_name(e)? != "=" {
        return None;
    }
    let lhs = qualified_col(e.lexpr.as_deref()?)?;
    let rhs = qualified_col(e.rexpr.as_deref()?)?;
    let ((_, pcol), (calias, ccol)) = if lhs.0 == parent_alias {
        (lhs, rhs)
    } else if rhs.0 == parent_alias {
        (rhs, lhs)
    } else {
        return None;
    };
    // A self-equality `parent.x = parent.y` isn't a child fk.
    if calias == parent_alias {
        return None;
    }
    Some((pcol, calias, ccol))
}

fn qualified_col(node: &Node) -> Option<(String, String)> {
    let NB::ColumnRef(cr) = node.node.as_ref()? else {
        return None;
    };
    match string_parts(&cr.fields).as_slice() {
        [alias, col] => Some((alias.clone(), col.clone())),
        _ => None,
    }
}

fn op_name(e: &pg_query::protobuf::AExpr) -> Option<&str> {
    match e.name.last()?.node.as_ref()? {
        NB::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

fn norm(s: &str) -> String {
    if s.is_empty() { "public".into() } else { s.to_string() }
}

fn alias_in(set: &HashSet<String>, alias: &str) -> bool {
    set.contains(alias) || set.contains(strip_suffix_digits(alias))
}

fn strip_suffix_digits(s: &str) -> &str {
    s.trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches('_')
}

// ---------- Catalog: validate FK direction ----------

/// `(schema, table, column)` triple identifying one side of an FK.
type ColKey = (String, String, String);
/// FK target plus whether the source column is nullable.
type FkTarget = (String, String, String, bool);

async fn validate_fks(client: &Client, cands: &[JoinCand]) -> Vec<FkLink> {
    let child_tables: HashSet<(String, String)> =
        cands.iter().map(|c| c.child_table.clone()).collect();
    let schemas: Vec<&str> = child_tables.iter().map(|(s, _)| s.as_str()).collect();
    let names: Vec<&str> = child_tables.iter().map(|(_, t)| t.as_str()).collect();
    // (src schema, src table, src col) → (tgt schema, tgt table, tgt col, src nullable)
    let Ok(rows) = client
        .query(
            r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT ns.nspname, sc.relname, sa.attname, NOT sa.attnotnull,
               nt.nspname, tc.relname, ta.attname
        FROM ask
        JOIN pg_namespace ns  ON ns.nspname = ask.schema
        JOIN pg_class sc      ON sc.relnamespace = ns.oid AND sc.relname = ask.name
        JOIN pg_constraint con ON con.conrelid = sc.oid AND con.contype = 'f'
                              AND cardinality(con.conkey) = 1
        JOIN pg_class tc      ON tc.oid = con.confrelid
        JOIN pg_namespace nt  ON nt.oid = tc.relnamespace
        JOIN pg_attribute sa  ON sa.attrelid = con.conrelid AND sa.attnum = con.conkey[1]
        JOIN pg_attribute ta  ON ta.attrelid = con.confrelid AND ta.attnum = con.confkey[1]
        "#,
            &[&schemas, &names],
        )
        .await
        .inspect_err(|e| tracing::debug!("validate_fks: {e}"))
    else {
        return Vec::new();
    };
    // (src schema, src table, src col) → (tgt triple, src nullable)
    let mut fks: HashMap<ColKey, FkTarget> = HashMap::new();
    for r in &rows {
        fks.insert(
            (r.get(0), r.get(1), r.get(2)),
            (r.get(4), r.get(5), r.get(6), r.get(3)),
        );
    }
    cands
        .iter()
        .filter_map(|c| {
            let (tgt_s, tgt_t, tgt_c, nullable) = fks.get(&(
                c.child_table.0.clone(),
                c.child_table.1.clone(),
                c.child_col.clone(),
            ))?;
            let points_at_parent = *tgt_s == c.parent_table.0
                && *tgt_t == c.parent_table.1
                && *tgt_c == c.parent_col;
            if !points_at_parent || !nullable {
                return None;
            }
            Some(FkLink {
                fk: TableColRef {
                    schema: c.child_table.0.clone(),
                    table: c.child_table.1.clone(),
                    column: c.child_col.clone(),
                },
                child_alias: c.child_alias.clone(),
                parent_alias: c.parent_alias.clone(),
            })
        })
        .collect()
}

// ---------- Synthesis: links + CHECK → row variants ----------

const BASE: &str = "\0base"; // sentinel: column keeps its base rendering

pub(crate) fn synth(
    links: &[FkLink],
    out: &[OutputCol],
    row_refs: &HashMap<(String, String), Vec<RowRefinement>>,
) -> Vec<RowVariant> {
    // Group links by their child (the fk-holder). The CHECK that
    // correlates the arcs lives on that child table.
    let mut by_child: BTreeMap<(String, String, String), Vec<&FkLink>> = BTreeMap::new();
    for l in links {
        by_child
            .entry((l.fk.schema.clone(), l.fk.table.clone(), l.child_alias.clone()))
            .or_default()
            .push(l);
    }
    // CHECK-driven first: a child with a row-CHECK pinning every arc.
    for ((schema, table, child_alias), group) in &by_child {
        let refs = row_refs.get(&(schema.clone(), table.clone()));
        if let Some(v) = refs.and_then(|refs| check_driven(refs, group, child_alias, out)) {
            return v;
        }
    }
    // No CHECK: a lone arc whose fk column is itself selected still
    // correlates the fk with the parent's presence.
    for l in links {
        if let Some(v) = no_check(l, out) {
            return v;
        }
    }
    Vec::new()
}

fn check_driven(
    refs: &[RowRefinement],
    group: &[&FkLink],
    child_alias: &str,
    out: &[OutputCol],
) -> Option<Vec<RowVariant>> {
    let handled: HashSet<&str> = group.iter().map(|l| l.parent_alias.as_str()).collect();
    // Every join-widened output column must belong to a parent we drive.
    if out.iter().any(|c| c.widened && !handled.contains(c.alias.as_str())) {
        return None;
    }
    for rr in refs {
        // Need every arc's fk pinned (null vs not) in every variant.
        let pins_all = !rr.variants.is_empty()
            && rr
                .variants
                .iter()
                .all(|v| group.iter().all(|l| v.columns.contains_key(&l.fk.column)));
        if !pins_all {
            continue;
        }
        let variants: Vec<RowVariant> = rr
            .variants
            .iter()
            .map(|v| {
                let mut ov = BTreeMap::new();
                // Child column pins → matching output columns.
                for oc in out {
                    if oc.alias == child_alias {
                        if let Some(col) = &oc.column {
                            if let Some(ts) = v.columns.get(col) {
                                ov.insert(oc.name.clone(), ts.clone());
                            }
                        }
                    }
                }
                // Absent arcs → null every one of their output columns.
                for l in group {
                    if v.columns.get(&l.fk.column).map(String::as_str) == Some("null") {
                        for oc in out.iter().filter(|c| c.alias == l.parent_alias) {
                            ov.insert(oc.name.clone(), "null".into());
                        }
                    }
                }
                RowVariant { overrides: ov }
            })
            .collect();
        if meaningful(&variants, out) {
            return Some(variants);
        }
    }
    None
}

fn no_check(link: &FkLink, out: &[OutputCol]) -> Option<Vec<RowVariant>> {
    // The fk column must be selected, else the split adds no info.
    let fk_oc = out.iter().find(|c| {
        c.alias == link.child_alias && c.column.as_deref() == Some(link.fk.column.as_str())
    })?;
    // Only this parent may contribute widened columns.
    if out
        .iter()
        .any(|c| c.widened && c.alias != link.parent_alias)
    {
        return None;
    }
    let present = {
        let mut ov = BTreeMap::new();
        ov.insert(fk_oc.name.clone(), fk_oc.ts_type.clone());
        RowVariant { overrides: ov }
    };
    let absent = {
        let mut ov = BTreeMap::new();
        ov.insert(fk_oc.name.clone(), "null".into());
        for oc in out.iter().filter(|c| c.alias == link.parent_alias) {
            ov.insert(oc.name.clone(), "null".into());
        }
        RowVariant { overrides: ov }
    };
    let variants = vec![present, absent];
    meaningful(&variants, out).then_some(variants)
}

/// At least two variants that differ on some output column (treating a
/// missing override as the column's base rendering).
fn meaningful(variants: &[RowVariant], out: &[OutputCol]) -> bool {
    if variants.len() < 2 {
        return false;
    }
    out.iter().any(|oc| {
        let mut seen: HashSet<&str> = HashSet::new();
        for v in variants {
            seen.insert(v.overrides.get(&oc.name).map_or(BASE, String::as_str));
        }
        seen.len() > 1
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{RowRefinement, RowVariant as CheckVariant};

    fn oc(name: &str, alias: &str, table: &str, col: &str, ts: &str, widened: bool) -> OutputCol {
        OutputCol {
            name: name.into(),
            alias: alias.into(),
            table: Some(("billing".into(), table.into())),
            column: Some(col.into()),
            ts_type: ts.into(),
            widened,
        }
    }

    fn link(fk_table: &str, fk_col: &str, child_alias: &str, parent_alias: &str) -> FkLink {
        FkLink {
            fk: TableColRef {
                schema: "billing".into(),
                table: fk_table.into(),
                column: fk_col.into(),
            },
            child_alias: child_alias.into(),
            parent_alias: parent_alias.into(),
        }
    }

    fn check(variants: Vec<Vec<(&str, &str)>>) -> RowRefinement {
        RowRefinement {
            variants: variants
                .into_iter()
                .map(|cols| CheckVariant {
                    columns: cols
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                })
                .collect(),
        }
    }

    fn refs(table: &str, rr: RowRefinement) -> HashMap<(String, String), Vec<RowRefinement>> {
        let mut m = HashMap::new();
        m.insert(("billing".into(), table.into()), vec![rr]);
        m
    }

    #[test]
    fn exclusive_arc_two_links() {
        // num_nonnulls(comment_id, alert_id) = 1.
        let rr = check(vec![
            vec![("comment_id", "string"), ("alert_id", "null")],
            vec![("comment_id", "null"), ("alert_id", "string")],
        ]);
        let links = vec![
            link("notifications", "comment_id", "n", "c"),
            link("notifications", "alert_id", "n", "a"),
        ];
        let out = vec![
            oc("comment_body", "c", "comments", "body", "string", true),
            oc("alert_message", "a", "alerts", "message", "string", true),
        ];
        let v = synth(&links, &out, &refs("notifications", rr));
        assert_eq!(v.len(), 2);
        // variant 0: comment present, alert absent.
        assert_eq!(v[0].overrides.get("alert_message").unwrap(), "null");
        assert!(v[0].overrides.get("comment_body").is_none());
        // variant 1: comment absent, alert present.
        assert_eq!(v[1].overrides.get("comment_body").unwrap(), "null");
        assert!(v[1].overrides.get("alert_message").is_none());
    }

    #[test]
    fn single_link_check_pins_other_column() {
        // num_nonnulls(prose, article_id) = 1; prose is an output col.
        let rr = check(vec![
            vec![("prose", "string"), ("article_id", "null")],
            vec![("prose", "null"), ("article_id", "string")],
        ]);
        let links = vec![link("feed_items", "article_id", "f", "ar")];
        let out = vec![
            oc("prose", "f", "feed_items", "prose", "string", false),
            oc("title", "ar", "articles", "title", "string", true),
        ];
        let v = synth(&links, &out, &refs("feed_items", rr));
        assert_eq!(v.len(), 2);
        // variant 0 picks prose → article_id null → article absent.
        assert_eq!(v[0].overrides.get("prose").unwrap(), "string");
        assert_eq!(v[0].overrides.get("title").unwrap(), "null");
        // variant 1 picks article_id → article present, prose null.
        assert_eq!(v[1].overrides.get("prose").unwrap(), "null");
        assert_eq!(v[1].overrides.get("title"), None);
    }

    #[test]
    fn no_check_fk_in_output() {
        let links = vec![link("bookmarks", "article_id", "b", "ar")];
        let out = vec![
            oc("article_id", "b", "bookmarks", "article_id", "string", false),
            oc("title", "ar", "articles", "title", "string", true),
        ];
        let v = synth(&links, &out, &HashMap::new());
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].overrides.get("article_id").unwrap(), "string");
        assert_eq!(v[1].overrides.get("article_id").unwrap(), "null");
        assert_eq!(v[1].overrides.get("title").unwrap(), "null");
    }

    #[test]
    fn no_check_fk_not_in_output_bails() {
        // fk not selected, no check → no information gain.
        let links = vec![link("bookmarks", "article_id", "b", "ar")];
        let out = vec![
            oc("id", "b", "bookmarks", "id", "string", false),
            oc("title", "ar", "articles", "title", "string", true),
        ];
        assert!(synth(&links, &out, &HashMap::new()).is_empty());
    }

    #[test]
    fn unrelated_widened_column_bails() {
        // A widened column from an alias we don't drive → unsound to
        // force non-null, so bail.
        let rr = check(vec![
            vec![("prose", "string"), ("article_id", "null")],
            vec![("prose", "null"), ("article_id", "string")],
        ]);
        let links = vec![link("feed_items", "article_id", "f", "ar")];
        let out = vec![
            oc("prose", "f", "feed_items", "prose", "string", false),
            oc("title", "ar", "articles", "title", "string", true),
            oc("extra", "x", "other", "col", "string", true), // unrelated widened
        ];
        assert!(synth(&links, &out, &refs("feed_items", rr)).is_empty());
    }
}
