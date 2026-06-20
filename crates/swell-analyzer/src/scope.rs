//! Name resolution + alias nullability for an `Analyzed` build.
//!
//! `Scope` is built once from the EXPLAIN plan tree, then consumed by
//! the lowering pass to resolve `ColumnRef`s to `ResolvedCol` with the
//! `not_null` bit already adjusted for outer-join widening.

use crate::analyzed::{CastPolicy, Expr};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use tokio_postgres::Client;

#[derive(Debug, Clone)]
pub struct Scope {
    aliases: HashMap<String, ResolvedTable>,
    /// SQL-level derived table aliases (RangeSubselect, CTE references)
    /// resolved to per-column lowered `Expr`s. Set after `Scope::build`
    /// by `Scope::attach_derived`. Lookup order in `lower_column_ref`
    /// is: aliases (real tables) → derived (subselects / CTEs).
    derived: HashMap<String, Vec<DerivedColumn>>,
    nullable: HashSet<String>,
    non_null: HashSet<String>,
    /// Per-analyzer cast policy — threaded through derived-column
    /// resolution so the verdict for a CTE / derived column matches
    /// what the top-level pass would produce.
    cast_policy: CastPolicy,
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
    /// column name → attnotnull
    columns: HashMap<String, bool>,
}

impl ResolvedTable {
    pub fn col_not_null(&self, col: &str) -> Option<bool> {
        self.columns.get(col).copied()
    }
}

impl Scope {
    /// Build a `Scope` for a query: map every plan-tree scan alias to
    /// its base table (with per-column `attnotnull` preloaded), then
    /// merge in the outer-join / non-null-source alias sets the plan
    /// walk produced.
    pub async fn build(
        client: &Client,
        alias_to_table: HashMap<String, (String, String)>,
        nullable: HashSet<String>,
        non_null: HashSet<String>,
        cast_policy: CastPolicy,
    ) -> Result<Self> {
        let distinct: HashSet<(String, String)> = alias_to_table.values().cloned().collect();
        let columns = fetch_attnotnull(client, &distinct).await?;
        let aliases = alias_to_table.into_iter()
            .filter_map(|(alias, (schema, name))| {
                let cols = columns.get(&(schema.clone(), name.clone()))?.clone();
                Some((alias, ResolvedTable { schema, name, columns: cols }))
            })
            .collect();
        Ok(Self { aliases, derived: HashMap::new(), nullable, non_null, cast_policy })
    }

    pub fn cast_policy(&self) -> CastPolicy { self.cast_policy }

    /// Attach derived-table aliases. Lowering pass calls this after
    /// `Scope::build` once it has the SQL parse tree — we can't do
    /// this in `build` because lowering depends on `Scope`, which
    /// would otherwise create a circular dependency.
    pub fn attach_derived(&mut self, derived: HashMap<String, Vec<DerivedColumn>>) {
        self.derived = derived;
    }

    pub fn derived(&self, alias: &str) -> Option<&[DerivedColumn]> {
        self.derived.get(alias).map(|v| v.as_slice())
    }

    pub fn resolve_alias(&self, alias: &str) -> Option<&ResolvedTable> {
        // PG renames duplicate scan aliases as `users_1`, `users_2` in
        // plans. Try the literal alias first; fall back to the
        // de-numbered form so the user-written alias resolves.
        self.aliases.get(alias).or_else(|| self.aliases.get(strip_suffix_digits(alias)))
    }

    /// Bare `<col>` resolves to a non-null source iff scope's aliases
    /// agree on its origin — either a single matching alias, or
    /// multiple aliases that all point at the same base `(schema,
    /// table)` (e.g. set-op branches both scanning the same relation;
    /// PG renames them `users_1`, `users_2`). Falls back to the
    /// plan-tree non-null verdict when there's a single non-null
    /// source alias (literal `unnest`, all-literal VALUES) and no
    /// catalog match.
    pub fn resolve_bare(&self, col: &str) -> Option<BareResolved> {
        let matches: Vec<(&String, &ResolvedTable, bool)> = self.aliases.iter()
            .filter_map(|(a, t)| t.col_not_null(col).map(|nn| (a, t, nn)))
            .collect();
        if !matches.is_empty() {
            let (_, first_table, _) = matches[0];
            let same_table = matches.iter()
                .all(|(_, t, _)| t.schema == first_table.schema && t.name == first_table.name);
            if !same_table { return None; }
            // Aliases agree on the origin table. Take the alias whose
            // nullability state is the strictest (any nullable-side
            // alias → result is widened). For set-op branches both
            // referring to the same base table, neither is widened, so
            // this picks any.
            let widening = matches.iter()
                .any(|(a, _, _)| self.is_nullable_alias(a));
            let force_non_null = matches.iter()
                .any(|(a, _, _)| self.is_non_null_alias(a));
            let base_not_null = matches.iter().all(|(_, _, nn)| *nn);
            let (alias, table, _) = matches[0];
            return Some(BareResolved {
                schema: table.schema.clone(),
                table: table.name.clone(),
                alias: alias.clone(),
                not_null: (base_not_null || force_non_null) && !widening,
            });
        }
        // Derived-table / CTE: bare `<col>` resolves if exactly one
        // derived alias has a column by that name.
        let derived_matches: Vec<(&String, &DerivedColumn)> = self.derived.iter()
            .filter_map(|(a, cols)| cols.iter().find(|c| c.name == col).map(|c| (a, c)))
            .collect();
        if derived_matches.len() == 1 {
            let (alias, dcol) = derived_matches[0];
            return Some(BareResolved {
                schema: String::new(),
                table: alias.clone(),
                alias: alias.clone(),
                not_null: crate::lowering::is_non_null(&dcol.expr, self.cast_policy),
            });
        }
        if self.non_null.len() == 1 {
            let alias = self.non_null.iter().next()?.clone();
            return Some(BareResolved {
                schema: String::new(),
                table: String::new(),
                alias,
                not_null: true,
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

    /// Find an alias whose underlying scan points at `(schema, table)`.
    /// Used by star expansion to recover the alias for `RangeVar`-only
    /// star outputs — when multiple aliases match (self-join + star),
    /// returns the first deterministically (sorted by alias name).
    pub fn find_alias(&self, schema: &str, table: &str) -> Option<&str> {
        let mut matches: Vec<&str> = self.aliases.iter()
            .filter(|(_, t)| t.schema == schema && t.name == table)
            .map(|(a, _)| a.as_str())
            .collect();
        matches.sort();
        matches.into_iter().next()
    }
}

pub struct BareResolved {
    pub schema: String,
    pub table: String,
    pub alias: String,
    pub not_null: bool,
}

/// `users_1` → `users`. PG appends `_N` to disambiguate duplicate scan
/// aliases within a single plan.
fn strip_suffix_digits(s: &str) -> &str {
    let t = s.trim_end_matches(|c: char| c.is_ascii_digit());
    t.trim_end_matches('_')
}

/// One round-trip: per (schema, table) → { column → attnotnull }.
async fn fetch_attnotnull(
    client: &Client, pairs: &HashSet<(String, String)>,
) -> Result<HashMap<(String, String), HashMap<String, bool>>> {
    if pairs.is_empty() { return Ok(HashMap::new()); }
    let schemas: Vec<&str> = pairs.iter().map(|p| p.0.as_str()).collect();
    let tables: Vec<&str>  = pairs.iter().map(|p| p.1.as_str()).collect();
    let rows = client.query(
        r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT n.nspname, c.relname, a.attname, a.attnotnull
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
        JOIN pg_attribute a ON a.attrelid = c.oid
        WHERE a.attnum > 0 AND NOT a.attisdropped
        "#,
        &[&schemas, &tables],
    ).await?;
    let mut out: HashMap<(String, String), HashMap<String, bool>> = HashMap::new();
    for row in &rows {
        let schema: String = row.get(0);
        let table: String  = row.get(1);
        let col: String    = row.get(2);
        let nn: bool       = row.get(3);
        out.entry((schema, table)).or_default().insert(col, nn);
    }
    Ok(out)
}
