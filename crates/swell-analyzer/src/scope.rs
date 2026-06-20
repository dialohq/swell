//! Name resolution + alias nullability for an `Analyzed` build.

use crate::analyzed::Expr;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use tokio_postgres::Client;

#[derive(Debug, Clone)]
pub struct Scope {
    aliases: HashMap<String, ResolvedTable>,
    /// SQL-level derived aliases (RangeSubselect / CTE / view).
    derived: HashMap<String, Vec<DerivedColumn>>,
    nullable: HashSet<String>,
    non_null: HashSet<String>,
    /// `(source_typoid, target_typoid)` pairs with user-defined
    /// `castmethod='f'` casts — per-Cast `is_unsafe` lookup.
    unsafe_casts: HashSet<(u32, u32)>,
    /// `pg_type.typname → oid` for resolving `TypeCast` targets.
    typname_to_oid: HashMap<String, u32>,
}

#[derive(Debug, Clone)]
pub struct DerivedColumn {
    pub name: String,
    pub expr: Expr,
}

#[derive(Debug, Clone)]
pub struct ResolvedTable {
    pub schema: String,
    pub name: String,
    /// column name → (attnotnull, atttypid)
    columns: HashMap<String, (bool, u32)>,
}

impl ResolvedTable {
    pub fn col_not_null(&self, col: &str) -> Option<bool> {
        self.columns.get(col).map(|(nn, _)| *nn)
    }
    pub fn col_typoid(&self, col: &str) -> Option<u32> {
        self.columns.get(col).map(|(_, oid)| *oid)
    }
}

impl Scope {
    pub async fn build(
        client: &Client,
        alias_to_table: HashMap<String, (String, String)>,
        nullable: HashSet<String>,
        non_null: HashSet<String>,
        unsafe_casts: HashSet<(u32, u32)>,
        typname_to_oid: HashMap<String, u32>,
    ) -> Result<Self> {
        let distinct: HashSet<(String, String)> = alias_to_table.values().cloned().collect();
        let columns = fetch_columns(client, &distinct).await?;
        let aliases = alias_to_table
            .into_iter()
            .filter_map(|(alias, (schema, name))| {
                let cols = columns.get(&(schema.clone(), name.clone()))?.clone();
                Some((
                    alias,
                    ResolvedTable {
                        schema,
                        name,
                        columns: cols,
                    },
                ))
            })
            .collect();
        Ok(Self {
            aliases,
            derived: HashMap::new(),
            nullable,
            non_null,
            unsafe_casts,
            typname_to_oid,
        })
    }

    pub fn typname_oid(&self, name: &str) -> Option<u32> {
        self.typname_to_oid.get(name).copied()
    }

    pub fn is_unsafe_cast(&self, source: u32, target: u32) -> bool {
        self.unsafe_casts.contains(&(source, target))
    }

    /// Lowering pass calls this after `Scope::build`. Two-phase to
    /// avoid circular dependency on lowering inside `build`.
    pub fn attach_derived(&mut self, derived: HashMap<String, Vec<DerivedColumn>>) {
        self.derived = derived;
    }

    pub fn derived(&self, alias: &str) -> Option<&[DerivedColumn]> {
        self.derived.get(alias).map(|v| v.as_slice())
    }

    pub fn resolve_alias(&self, alias: &str) -> Option<&ResolvedTable> {
        // PG renames duplicate scan aliases to `users_1`, `users_2` in
        // plans — fall back to the de-numbered form.
        self.aliases
            .get(alias)
            .or_else(|| self.aliases.get(strip_suffix_digits(alias)))
    }

    /// Bare `<col>` resolves only when scope's aliases agree on its
    /// origin (single alias, or multiple aliases on the same base
    /// table — e.g. set-op branches). Falls back to the plan-tree
    /// non-null verdict when there's a single non-null source alias
    /// (literal `unnest`, all-literal VALUES) and no catalog match.
    pub fn resolve_bare(&self, col: &str) -> Option<BareResolved> {
        let matches: Vec<(&String, &ResolvedTable, bool)> = self
            .aliases
            .iter()
            .filter_map(|(a, t)| t.col_not_null(col).map(|nn| (a, t, nn)))
            .collect();
        if let Some((alias, table, _)) = matches.first() {
            let same_table = matches
                .iter()
                .all(|(_, t, _)| t.schema == table.schema && t.name == table.name);
            if !same_table {
                return None;
            }
            let widening = matches.iter().any(|(a, _, _)| self.is_nullable_alias(a));
            let force_nn = matches.iter().any(|(a, _, _)| self.is_non_null_alias(a));
            let base_nn = matches.iter().all(|(_, _, nn)| *nn);
            return Some(BareResolved {
                schema: table.schema.clone(),
                table: table.name.clone(),
                alias: (*alias).clone(),
                not_null: (base_nn || force_nn) && !widening,
                typoid: table.col_typoid(col).unwrap_or(0),
            });
        }
        let mut derived_matches = self
            .derived
            .iter()
            .filter_map(|(a, cols)| cols.iter().find(|c| c.name == col).map(|c| (a, c)));
        if let Some((alias, dcol)) = derived_matches.next() {
            if derived_matches.next().is_none() {
                return Some(BareResolved {
                    table: alias.clone(),
                    alias: alias.clone(),
                    not_null: crate::lowering::is_non_null(&dcol.expr),
                    ..Default::default()
                });
            }
        }
        if self.non_null.len() == 1 {
            return Some(BareResolved {
                alias: self.non_null.iter().next()?.clone(),
                not_null: true,
                ..Default::default()
            });
        }
        None
    }

    pub fn is_nullable_alias(&self, alias: &str) -> bool {
        self.nullable.contains(alias) || self.nullable.contains(strip_suffix_digits(alias))
    }

    pub fn is_non_null_alias(&self, alias: &str) -> bool {
        self.non_null.contains(alias) || self.non_null.contains(strip_suffix_digits(alias))
    }

    /// First alias (alphabetically) pointing at `(schema, table)` —
    /// deterministic tiebreak for star-expansion outputs on self-joins.
    pub fn find_alias(&self, schema: &str, table: &str) -> Option<&str> {
        self.aliases
            .iter()
            .filter(|(_, t)| t.schema == schema && t.name == table)
            .map(|(a, _)| a.as_str())
            .min()
    }
}

#[derive(Default)]
pub struct BareResolved {
    pub schema: String,
    pub table: String,
    pub alias: String,
    pub not_null: bool,
    pub typoid: u32,
}

/// `users_1` → `users`. PG appends `_N` to disambiguate duplicate
/// scan aliases within a plan.
fn strip_suffix_digits(s: &str) -> &str {
    let t = s.trim_end_matches(|c: char| c.is_ascii_digit());
    t.trim_end_matches('_')
}

async fn fetch_columns(
    client: &Client,
    pairs: &HashSet<(String, String)>,
) -> Result<HashMap<(String, String), HashMap<String, (bool, u32)>>> {
    if pairs.is_empty() {
        return Ok(HashMap::new());
    }
    let schemas: Vec<&str> = pairs.iter().map(|p| p.0.as_str()).collect();
    let tables: Vec<&str> = pairs.iter().map(|p| p.1.as_str()).collect();
    let rows = client
        .query(
            r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT n.nspname, c.relname, a.attname, a.attnotnull, a.atttypid::oid
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
        JOIN pg_attribute a ON a.attrelid = c.oid
        WHERE a.attnum > 0 AND NOT a.attisdropped
        "#,
            &[&schemas, &tables],
        )
        .await?;
    let mut out: HashMap<(String, String), HashMap<String, (bool, u32)>> = HashMap::new();
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let col: String = row.get(2);
        let nn: bool = row.get(3);
        let oid: u32 = row.get(4);
        out.entry((schema, table))
            .or_default()
            .insert(col, (nn, oid));
    }
    Ok(out)
}
