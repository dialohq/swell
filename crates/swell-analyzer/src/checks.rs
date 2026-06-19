//! Refine generated column types from `CHECK` constraints.
//!
//! Tier 1 — column-level literal unions
//!   - `col = lit`                    → `"lit"`
//!   - `col = ANY (ARRAY[lit, lit])`  → `"a" | "b"`
//!   - `col IS NULL OR <above>`       → adds `| null`
//!
//! Tier 2 — JSON object shapes on `jsonb` columns. An `AND` chain of
//! atomic predicates that all target the same column reduces to a TS
//! object type. Recognised predicates:
//!   - `jsonb_typeof(col) = 'object'`        — object discriminator
//!   - `col ? 'k'` / `col ?& array['a','b']` — required keys (type Json)
//!   - `jsonb_typeof(col->'k') = '<type>'`   — key has scalar type
//!   - `col->>'k' = 'lit'`                   — key has literal value
//!   - key-count idiom                       — closes the object
//!
//! Tier 3 (column-level) — discriminated unions. An `OR` chain whose
//! branches are each AND-chains targeting the same column reduces to a
//! TS union (`{...} | {...}`). The branches typically share a literal
//! discriminant key, but we don't require it.
//!
//! Row-level disjunctions (`num_nonnulls(a,b) = 1`, `CASE` keyed on
//! another column) need codegen changes to wrap the whole row type in
//! a union — left for a follow-up PR (see issue #22).
//!
//! Bail liberally: anything we don't recognise drops the *whole*
//! refinement for that column. Partial narrowing is worse than none.

use pg_query::protobuf::{self, a_const, node::Node as NodeBody, Integer, ParseResult};
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio_postgres::Client;

// ----------------------------------------------------------------------
//   Public types
// ----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Literal {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Literal {
    fn render(&self) -> String {
        match self {
            Literal::Str(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            Literal::Int(i) => i.to_string(),
            Literal::Bool(b) => b.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JsonFieldType {
    String,
    Number,
    Boolean,
    Array,
    Object,
    Json,
    /// `payload->>'kind' = 'text'` → literal discriminant.
    LiteralStr(String),
}

impl JsonFieldType {
    fn render(&self) -> String {
        match self {
            JsonFieldType::String => "string".into(),
            JsonFieldType::Number => "number".into(),
            JsonFieldType::Boolean => "boolean".into(),
            JsonFieldType::Array => "Json[]".into(),
            JsonFieldType::Object => "Record<string, Json>".into(),
            JsonFieldType::Json => "Json".into(),
            JsonFieldType::LiteralStr(s) => format!(
                "\"{}\"",
                s.replace('\\', "\\\\").replace('"', "\\\"")
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ObjectShape {
    /// Required keys, in insertion order. Order matters for rendering.
    pub fields: BTreeMap<String, JsonFieldType>,
    /// `Some(n)` → object is closed at exactly n keys, render without
    /// the `& Record<string, Json>` extension. `None` → open object.
    pub closed_at: Option<usize>,
    pub allow_null: bool,
}

impl ObjectShape {
    fn render(&self) -> String {
        let body: Vec<String> = self.fields.iter()
            .map(|(k, v)| format!("{}: {}", quote_key(k), v.render()))
            .collect();
        let inner = format!("{{ {} }}", body.join("; "));
        // Don't bake `| null` into the rendered shape — the column's
        // nullability is tracked separately (attnotnull / decide_
        // nullability) and codegen appends `| null` based on that.
        // Emitting it here as well would produce `… | null | null`.
        match self.closed_at {
            Some(_) => inner,
            None => format!("{inner} & Record<string, Json>"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Refinement {
    LiteralUnion {
        literals: Vec<Literal>,
        allow_null: bool,
    },
    Object(ObjectShape),
    /// Discriminated union of two or more refinements (column-level).
    Union(Vec<Refinement>),
}

impl Refinement {
    pub fn render_ts(&self) -> Option<String> {
        Some(match self {
            // `allow_null` is informational only — the column's
            // nullability is tracked independently (attnotnull) and
            // codegen appends `| null` based on that. Emitting it here
            // would produce `… | null | null`.
            Refinement::LiteralUnion { literals, allow_null: _ } => {
                if literals.is_empty() {
                    return None;
                }
                literals.iter().map(Literal::render).collect::<Vec<_>>().join(" | ")
            }
            Refinement::Object(o) => o.render(),
            Refinement::Union(branches) => {
                let rendered: Vec<String> = branches.iter()
                    .filter_map(Refinement::render_ts)
                    .collect();
                if rendered.is_empty() {
                    return None;
                }
                rendered.join(" | ")
            }
        })
    }

    /// Combine two refinements on the same column: both predicates must
    /// hold. For literal unions this is set-intersection; for object
    /// shapes it merges field requirements; mismatched variants drop the
    /// refinement (we'd rather emit the base type than guess).
    pub fn intersect(self, other: Refinement) -> Option<Refinement> {
        match (self, other) {
            (
                Refinement::LiteralUnion { literals: a, allow_null: na },
                Refinement::LiteralUnion { literals: b, allow_null: nb },
            ) => {
                let bs: HashSet<_> = b.into_iter().collect();
                let literals: Vec<_> = a.into_iter().filter(|l| bs.contains(l)).collect();
                if literals.is_empty() {
                    return None;
                }
                Some(Refinement::LiteralUnion { literals, allow_null: na && nb })
            }
            (Refinement::Object(mut a), Refinement::Object(b)) => {
                for (k, v) in b.fields {
                    match a.fields.get(&k) {
                        Some(existing) if existing != &v => return None,
                        Some(_) => {} // identical — keep
                        None => { a.fields.insert(k, v); }
                    }
                }
                match (a.closed_at, b.closed_at) {
                    (Some(x), Some(y)) if x != y => return None,
                    (Some(_), _) => {}                 // keep a's
                    (None, Some(_)) => a.closed_at = b.closed_at,
                    (None, None) => {}
                }
                a.allow_null = a.allow_null && b.allow_null;
                Some(Refinement::Object(a))
            }
            _ => None,
        }
    }
}

// ----------------------------------------------------------------------
//   Row-level refinements (Tier 3, cross-column)
// ----------------------------------------------------------------------

/// One variant of a row-level union. Each entry restricts a column's
/// type within this variant (e.g. `email: string`, `phone: null`).
/// Columns not in the map keep their base type.
#[derive(Debug, Clone, Default)]
pub struct RowVariant {
    pub columns: BTreeMap<String, String>,
}

/// One row-level CHECK constraint reduced to a union of variants.
/// Multiple CHECKs on one table become multiple `RowRefinement`s and
/// are intersected in codegen as `Base & (u1) & (u2) & …`, letting TS
/// compute the joint constraint (disconnected CHECKs collapse cleanly;
/// connected ones produce `never` on incompatible branches).
#[derive(Debug, Clone)]
pub struct RowRefinement {
    pub variants: Vec<RowVariant>,
}

/// Map a single `CHECK` constraint to a row-level refinement when the
/// predicate is a cross-column disjunction we recognise. Two shapes:
///
///   - `num_nonnulls(a, b, c, …) = 1`  — XOR over the listed columns
///   - `CASE WHEN <col> = <lit> THEN <expr> [ … ELSE false] END`
///     where each `<expr>` reduces to a column refinement
pub fn parse_row_check_def(
    def: &str,
    table_columns: &HashMap<String, String>,
) -> Option<RowRefinement> {
    let expr = strip_check_wrapper(def)?;
    let stmt = format!("SELECT ({})", expr);
    let parsed = pg_query::parse(&stmt).ok()?;
    let node = top_select_first_target(&parsed.protobuf)?;
    reduce_row_predicate(node, table_columns)
}

fn reduce_row_predicate(
    node: &protobuf::Node,
    table_columns: &HashMap<String, String>,
) -> Option<RowRefinement> {
    match node.node.as_ref()? {
        NodeBody::AExpr(e) => reduce_num_nonnulls_eq(e, table_columns),
        NodeBody::CaseExpr(c) => reduce_case(c, table_columns),
        _ => None,
    }
}

/// `num_nonnulls(a, b, …) = 1` → one variant per argument.
fn reduce_num_nonnulls_eq(
    e: &protobuf::AExpr,
    table_columns: &HashMap<String, String>,
) -> Option<RowRefinement> {
    if op_name(e)? != "=" || e.kind != protobuf::AExprKind::AexprOp as i32 {
        return None;
    }
    let fc = func_call(e.lexpr.as_deref()?)?;
    if !is_named_func(fc, "num_nonnulls") {
        return None;
    }
    if int_literal(e.rexpr.as_deref()?) != Some(1) {
        return None;
    }
    let columns: Vec<String> = fc.args.iter()
        .filter_map(column_ref_name)
        .collect();
    if columns.len() != fc.args.len() || columns.len() < 2 {
        return None;
    }
    let mut variants = Vec::with_capacity(columns.len());
    for picked in &columns {
        let mut v = RowVariant::default();
        for c in &columns {
            let ts = if c == picked {
                non_null_form(table_columns.get(c)?)
            } else {
                "null".to_string()
            };
            v.columns.insert(c.clone(), ts);
        }
        variants.push(v);
    }
    Some(RowRefinement { variants })
}

/// `CASE WHEN <col> = <lit> THEN <pred> [WHEN …] [ELSE false] END`
/// — each WHEN branch becomes a variant. The discriminant column is
/// pinned to its literal, and any per-column refinement from the THEN
/// branch is folded in.
fn reduce_case(
    c: &protobuf::CaseExpr,
    table_columns: &HashMap<String, String>,
) -> Option<RowRefinement> {
    if c.arg.is_some() {
        // We only handle searched CASE (`CASE WHEN <cond> THEN …`), not
        // simple CASE (`CASE <expr> WHEN <val> THEN …`).
        return None;
    }
    // ELSE policy:
    //   - `ELSE false`         → exhaustive; no catch-all (rows outside
    //                            the WHEN literals are rejected by PG).
    //   - `ELSE true` / absent → non-exhaustive; PG accepts rows with
    //                            any other discriminant value, so emit
    //                            a catch-all variant whose discriminant
    //                            is `Exclude<base, "lit"|…>`.
    //   - any other ELSE       → bail; we can't represent partial
    //                            constraint over the leftover values.
    // PG normalizes a missing `ELSE` to `ELSE NULL::boolean` in
    // `pg_get_constraintdef`. `NULL` makes the CASE non-exhaustive
    // (CHECK passes for unmatched rows) — same as no-ELSE, same as
    // `ELSE true`.
    let need_catchall = match c.defresult.as_deref() {
        None => true,
        Some(n) if is_const_false(n) => false,
        Some(n) if is_const_true(n) || is_const_null(n) => true,
        Some(_) => return None,
    };

    let mut variants: Vec<RowVariant> = Vec::with_capacity(c.args.len() + 1);
    let mut disc_col: Option<String> = None;
    let mut covered: Vec<String> = Vec::new();
    for branch in &c.args {
        let when = match branch.node.as_ref()? {
            NodeBody::CaseWhen(cw) => cw,
            _ => return None,
        };
        let cond = when.expr.as_deref()?;
        let (col, lit) = column_eq_string_literal(cond)?;
        // Every WHEN must key on the same discriminant column —
        // otherwise we can't express the catch-all coherently.
        match &disc_col {
            None => disc_col = Some(col.clone()),
            Some(prev) if prev == &col => {}
            _ => return None,
        }
        let mut variant = RowVariant::default();
        variant.columns.insert(col.clone(), render_string_literal(&lit));
        covered.push(lit);
        let then = when.result.as_deref()?;
        if !is_const_true(then) {
            let (target, refinement) = reduce_predicate(then)?;
            if target != col {
                let ts = refinement.render_ts()?;
                variant.columns.insert(target, ts);
            }
        }
        variants.push(variant);
    }
    if need_catchall {
        let disc = disc_col?;
        let base = table_columns.get(&disc)?.clone();
        let lits = covered.iter()
            .map(|l| render_string_literal(l))
            .collect::<Vec<_>>()
            .join(" | ");
        let mut catchall = RowVariant::default();
        catchall.columns.insert(disc, format!("Exclude<{}, {}>", base, lits));
        variants.push(catchall);
    }
    if variants.len() < 2 { return None; }
    Some(RowRefinement { variants })
}

fn render_string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn column_eq_string_literal(node: &protobuf::Node) -> Option<(String, String)> {
    let e = match node.node.as_ref()? {
        NodeBody::AExpr(e) => e,
        _ => return None,
    };
    if op_name(e)? != "=" || e.kind != protobuf::AExprKind::AexprOp as i32 {
        return None;
    }
    let col = column_ref_name(e.lexpr.as_deref()?)?;
    let lit = string_literal(e.rexpr.as_deref()?)?;
    Some((col, lit))
}

fn is_const_false(node: &protobuf::Node) -> bool {
    matches!(node.node.as_ref(), Some(NodeBody::AConst(c))
        if !c.isnull && matches!(c.val.as_ref(), Some(a_const::Val::Boolval(b)) if !b.boolval))
}

fn is_const_true(node: &protobuf::Node) -> bool {
    matches!(node.node.as_ref(), Some(NodeBody::AConst(c))
        if !c.isnull && matches!(c.val.as_ref(), Some(a_const::Val::Boolval(b)) if b.boolval))
}

/// Recognise `NULL` and `NULL::<type>` (the form `pg_get_constraintdef`
/// emits for a missing `ELSE`). The cast may be wrapped in `TypeCast`.
fn is_const_null(node: &protobuf::Node) -> bool {
    match node.node.as_ref() {
        Some(NodeBody::AConst(c)) => c.isnull,
        Some(NodeBody::TypeCast(tc)) => tc.arg.as_deref().map(is_const_null).unwrap_or(false),
        _ => false,
    }
}

/// Strip a trailing `| null` so a variant can say "this column is
/// non-null in this branch" with the base scalar shape.
fn non_null_form(ts: &str) -> String {
    let trimmed = ts.trim_end();
    if let Some(stripped) = trimmed.strip_suffix("| null") {
        stripped.trim_end().to_string()
    } else if let Some(stripped) = trimmed.strip_suffix("|null") {
        stripped.trim_end().to_string()
    } else {
        ts.to_string()
    }
}

// ----------------------------------------------------------------------
//   Fetch from pg_constraint
// ----------------------------------------------------------------------

/// Row-level refinements per (schema, table) — each entry is the
/// **list of CHECKs** on that table, with each CHECK reduced to a
/// `RowRefinement` (union of variants). Codegen emits the table type
/// as `Base & (u1) & (u2) & …`, chaining one intersection per CHECK.
/// Only constraints `con.convalidated = true` are included — `NOT
/// VALID` constraints don't actually hold against existing rows.
pub async fn fetch_row_refinements(
    client: &Client,
    pairs: &[(String, String)],
    table_columns: &HashMap<(String, String), HashMap<String, String>>,
) -> HashMap<(String, String), Vec<RowRefinement>> {
    let mut out: HashMap<(String, String), Vec<RowRefinement>> = HashMap::new();
    if pairs.is_empty() {
        return out;
    }
    let schemas: Vec<&str> = pairs.iter().map(|(s, _)| s.as_str()).collect();
    let tables: Vec<&str> = pairs.iter().map(|(_, t)| t.as_str()).collect();
    let rows = match client.query(
        r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT n.nspname, c.relname, pg_get_constraintdef(con.oid)
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
        JOIN pg_constraint con ON con.conrelid = c.oid
                             AND con.contype = 'c'
                             AND con.convalidated
        "#,
        &[&schemas, &tables],
    ).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("fetch_row_refinements: {e}");
            return out;
        }
    };
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let def: String = row.get(2);
        let cols = match table_columns.get(&(schema.clone(), table.clone())) {
            Some(c) => c,
            None => continue,
        };
        let Some(refinement) = parse_row_check_def(&def, cols) else { continue };
        out.entry((schema, table)).or_default().push(refinement);
    }
    out
}

pub async fn fetch_refinements(
    client: &Client,
    pairs: &[(String, String)],
) -> HashMap<(String, String, String), Refinement> {
    let mut out: HashMap<(String, String, String), Refinement> = HashMap::new();
    if pairs.is_empty() {
        return out;
    }
    let schemas: Vec<&str> = pairs.iter().map(|(s, _)| s.as_str()).collect();
    let tables: Vec<&str> = pairs.iter().map(|(_, t)| t.as_str()).collect();
    let rows = match client.query(
        r#"
        WITH ask(schema, name) AS (SELECT * FROM unnest($1::text[], $2::text[]))
        SELECT n.nspname, c.relname, pg_get_constraintdef(con.oid)
        FROM ask
        JOIN pg_namespace n ON n.nspname = ask.schema
        JOIN pg_class c     ON c.relnamespace = n.oid AND c.relname = ask.name
        JOIN pg_constraint con ON con.conrelid = c.oid
                             AND con.contype = 'c'
                             AND con.convalidated
        "#,
        &[&schemas, &tables],
    ).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("fetch_refinements: {e}");
            return out;
        }
    };
    // Tracks columns where two CHECKs disagreed in ways `intersect`
    // can't reconcile (different scalar shapes, incompatible JSON
    // field types). Those drop entirely — partial narrowing is worse
    // than none, and non-deterministic narrowing (HashMap iteration
    // order) is the worst of all worlds.
    let mut conflicted: HashSet<(String, String, String)> = HashSet::new();
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let def: String = row.get(2);
        let Some((column, refinement)) = parse_check_def(&def) else { continue };
        let key = (schema, table, column);
        if conflicted.contains(&key) { continue; }
        match out.remove(&key) {
            Some(prev) => match prev.intersect(refinement) {
                Some(merged) => { out.insert(key, merged); }
                None => { conflicted.insert(key); }
            },
            None => { out.insert(key, refinement); }
        }
    }
    out
}

// ----------------------------------------------------------------------
//   Parser entry point
// ----------------------------------------------------------------------

pub fn parse_check_def(def: &str) -> Option<(String, Refinement)> {
    let expr = strip_check_wrapper(def)?;
    let stmt = format!("SELECT ({})", expr);
    let parsed = pg_query::parse(&stmt).ok()?;
    let node = top_select_first_target(&parsed.protobuf)?;
    reduce_predicate(node)
}

fn strip_check_wrapper(def: &str) -> Option<&str> {
    let trimmed = def.trim();
    let inside = trimmed.strip_prefix("CHECK")?.trim_start();
    let inside = inside.strip_prefix('(')?;
    // Strip optional Postgres-emitted suffixes that follow the predicate's
    // closing paren: `NO INHERIT`, `NOT VALID`. These aren't part of the
    // expression and would otherwise leave us with no trailing `)` to
    // strip. (`fetch_*` queries already filter on `convalidated` so `NOT
    // VALID` constraints don't reach this function, but unvalidated ones
    // on inherited tables can still arrive.)
    let mut tail = inside;
    for suffix in [" NO INHERIT", " NOT VALID"] {
        while let Some(s) = tail.trim_end().strip_suffix(suffix) {
            tail = s;
        }
    }
    let inside = tail.trim_end().strip_suffix(')')?;
    Some(inside.trim())
}

fn top_select_first_target(parsed: &ParseResult) -> Option<&protobuf::Node> {
    let raw = parsed.stmts.first()?;
    let stmt = raw.stmt.as_ref()?;
    let NodeBody::SelectStmt(s) = stmt.node.as_ref()? else { return None };
    let rt = s.target_list.first()?;
    let NodeBody::ResTarget(rt) = rt.node.as_ref()? else { return None };
    rt.val.as_deref()
}

// ----------------------------------------------------------------------
//   Reducer: predicate → refinement
// ----------------------------------------------------------------------

fn reduce_predicate(node: &protobuf::Node) -> Option<(String, Refinement)> {
    let n = node.node.as_ref()?;
    match n {
        NodeBody::BoolExpr(b) if b.boolop == protobuf::BoolExprType::OrExpr as i32 => {
            reduce_or(&b.args)
        }
        NodeBody::BoolExpr(b) if b.boolop == protobuf::BoolExprType::AndExpr as i32 => {
            reduce_and(&b.args)
        }
        NodeBody::AExpr(_) | NodeBody::NullTest(_) => {
            let mut acc = ClauseAccumulator::default();
            absorb_clause(node, &mut acc)?;
            acc.into_refinement()
        }
        _ => None,
    }
}

fn reduce_and(args: &[protobuf::Node]) -> Option<(String, Refinement)> {
    let mut acc = ClauseAccumulator::default();
    for arg in args {
        absorb_clause(arg, &mut acc)?;
    }
    acc.into_refinement()
}

/// `OR` branches reduce to either:
///   - the `col IS NULL OR <literal-set>` nullable idiom, OR
///   - a discriminated union of object shapes on a common column.
fn reduce_or(args: &[protobuf::Node]) -> Option<(String, Refinement)> {
    if let Some(r) = reduce_or_nullable_literal_set(args) {
        return Some(r);
    }
    // Discriminated union: every branch must reduce, and they must all
    // target the same column.
    let mut column: Option<String> = None;
    let mut branches: Vec<Refinement> = Vec::with_capacity(args.len());
    for arg in args {
        let (col, refinement) = reduce_predicate(arg)?;
        match &column {
            None => column = Some(col),
            Some(prev) if *prev == col => {}
            Some(_) => return None,
        }
        branches.push(refinement);
    }
    Some((column?, Refinement::Union(branches)))
}

fn reduce_or_nullable_literal_set(args: &[protobuf::Node]) -> Option<(String, Refinement)> {
    if args.len() != 2 {
        return None;
    }
    let mut null_test_col: Option<String> = None;
    let mut value_pred: Option<(String, Refinement)> = None;
    for arg in args {
        match arg.node.as_ref()? {
            NodeBody::NullTest(nt)
                if nt.nulltesttype == protobuf::NullTestType::IsNull as i32 =>
            {
                let col = column_ref_name(nt.arg.as_deref()?)?;
                null_test_col = Some(col);
            }
            _ => {
                if value_pred.is_some() {
                    return None;
                }
                value_pred = reduce_predicate(arg);
            }
        }
    }
    let (col, refinement) = value_pred?;
    if null_test_col.as_deref() != Some(col.as_str()) {
        return None;
    }
    match refinement {
        Refinement::LiteralUnion { literals, .. } => {
            Some((col, Refinement::LiteralUnion { literals, allow_null: true }))
        }
        Refinement::Object(mut o) => {
            o.allow_null = true;
            Some((col, Refinement::Object(o)))
        }
        Refinement::Union(branches) => {
            // Mark every branch nullable — same column applies.
            let branches = branches.into_iter().map(|r| match r {
                Refinement::LiteralUnion { literals, .. } =>
                    Refinement::LiteralUnion { literals, allow_null: true },
                Refinement::Object(mut o) => { o.allow_null = true; Refinement::Object(o) }
                r => r,
            }).collect();
            Some((col, Refinement::Union(branches)))
        }
    }
}

// ----------------------------------------------------------------------
//   Atomic-clause absorption
// ----------------------------------------------------------------------

/// One atomic predicate's contribution to a single column.
#[derive(Debug)]
enum ClauseKind {
    /// `jsonb_typeof(col) = 'object'`. Stamps the column as an object.
    JsonObjectMarker,
    /// `col ?& array['a','b']` or `col ? 'k'`. Required keys, no type info.
    RequiredKeys(Vec<String>),
    /// `jsonb_typeof(col -> 'k') = '<type>'`.
    FieldType(String, JsonFieldType),
    /// `col ->> 'k' = 'lit'` — literal discriminant.
    FieldLiteralStr(String, String),
    /// `col = lit` or `col = ANY (ARRAY[lit, ...])`.
    LiteralSet(Vec<Literal>),
    /// Key-count idiom — closes the object at this many keys.
    KeyCount(usize),
}

#[derive(Default)]
struct ClauseAccumulator {
    column: Option<String>,
    is_object: bool,
    fields: BTreeMap<String, JsonFieldType>,
    required: HashSet<String>,
    closed_at: Option<usize>,
    literals: Option<Vec<Literal>>,
}

impl ClauseAccumulator {
    fn into_refinement(mut self) -> Option<(String, Refinement)> {
        let col = self.column?;
        if let Some(lits) = self.literals.take() {
            // A literal-set predicate excludes object refinements.
            if self.is_object || !self.fields.is_empty() {
                return None;
            }
            return Some((col, Refinement::LiteralUnion {
                literals: lits,
                allow_null: false,
            }));
        }
        if !self.is_object && self.fields.is_empty() && self.required.is_empty() {
            return None;
        }
        // Ensure every required key has at least a Json type.
        for key in self.required {
            self.fields.entry(key).or_insert(JsonFieldType::Json);
        }
        Some((col, Refinement::Object(ObjectShape {
            fields: self.fields,
            closed_at: self.closed_at,
            allow_null: false,
        })))
    }
}

fn absorb_clause(node: &protobuf::Node, acc: &mut ClauseAccumulator) -> Option<()> {
    let kind = classify_clause(node)?;
    let col = clause_column(node)?;
    match &acc.column {
        None => acc.column = Some(col),
        Some(prev) if *prev == col => {}
        _ => return None,
    }
    match kind {
        ClauseKind::JsonObjectMarker => acc.is_object = true,
        ClauseKind::RequiredKeys(keys) => {
            for k in keys { acc.required.insert(k); }
        }
        ClauseKind::FieldType(k, t) => { acc.fields.insert(k, t); }
        ClauseKind::FieldLiteralStr(k, lit) => {
            acc.fields.insert(k, JsonFieldType::LiteralStr(lit));
        }
        ClauseKind::LiteralSet(lits) => {
            if acc.literals.is_some() { return None; }
            acc.literals = Some(lits);
        }
        ClauseKind::KeyCount(n) => acc.closed_at = Some(n),
    }
    Some(())
}

/// Identify which clause kind a single AST node represents. Returns
/// `None` for anything we don't recognise.
fn classify_clause(node: &protobuf::Node) -> Option<ClauseKind> {
    match node.node.as_ref()? {
        NodeBody::AExpr(e) => classify_aexpr(e),
        _ => None,
    }
}

/// Identify the column a clause targets. Mirrors `classify_clause`'s
/// structure — kept separate so the column-grouping check in
/// `absorb_clause` runs before we mutate the accumulator.
fn clause_column(node: &protobuf::Node) -> Option<String> {
    match node.node.as_ref()? {
        NodeBody::AExpr(e) => aexpr_column(e),
        _ => None,
    }
}

fn classify_aexpr(e: &protobuf::AExpr) -> Option<ClauseKind> {
    let op = op_name(e)?;

    // `col = lit` or `col = ANY (ARRAY[lit, ...])`.
    if op == "=" {
        if e.kind == protobuf::AExprKind::AexprOpAny as i32 {
            let arr = match e.rexpr.as_deref()?.node.as_ref()? {
                NodeBody::AArrayExpr(a) => a,
                _ => return None,
            };
            let mut literals = Vec::with_capacity(arr.elements.len());
            for el in &arr.elements {
                literals.push(scalar_literal(el)?);
            }
            if literals.is_empty() { return None; }
            // The LHS must be a bare column ref (not a json access).
            if column_ref_name(e.lexpr.as_deref()?).is_some() {
                return Some(ClauseKind::LiteralSet(literals));
            }
        }
        if e.kind == protobuf::AExprKind::AexprOp as i32 {
            // Try: jsonb_typeof(...) = 'type'  → JsonObjectMarker / FieldType
            if let Some(kind) = classify_jsonb_typeof_eq(e) {
                return Some(kind);
            }
            // Try: col ->> 'k' = 'lit'  → FieldLiteralStr
            if let Some(kind) = classify_field_literal(e) {
                return Some(kind);
            }
            // Try: key-count idiom = N
            if let Some(kind) = classify_key_count(e) {
                return Some(kind);
            }
            // Try: col = lit  (single literal)
            if column_ref_name(e.lexpr.as_deref()?).is_some() {
                let lit = scalar_literal(e.rexpr.as_deref()?)?;
                return Some(ClauseKind::LiteralSet(vec![lit]));
            }
        }
    }

    // `col ? 'k'`     — single required key
    // `col ?& array['a','b']` — multiple required keys
    if matches!(op, "?" | "?&") {
        let col = column_ref_name(e.lexpr.as_deref()?)?;
        let _ = col; // column captured separately by `aexpr_column`
        let keys = if op == "?" {
            vec![string_literal(e.rexpr.as_deref()?)?]
        } else {
            match e.rexpr.as_deref()?.node.as_ref()? {
                NodeBody::AArrayExpr(a) => {
                    let mut out = Vec::with_capacity(a.elements.len());
                    for el in &a.elements {
                        out.push(string_literal(el)?);
                    }
                    out
                }
                _ => return None,
            }
        };
        return Some(ClauseKind::RequiredKeys(keys));
    }

    None
}

fn aexpr_column(e: &protobuf::AExpr) -> Option<String> {
    let op = op_name(e)?;
    if op == "=" {
        if e.kind == protobuf::AExprKind::AexprOpAny as i32 {
            return column_ref_name(e.lexpr.as_deref()?);
        }
        if e.kind == protobuf::AExprKind::AexprOp as i32 {
            // jsonb_typeof(col) = 'object'
            if let Some(c) = jsonb_typeof_col(e.lexpr.as_deref()?) {
                return Some(c);
            }
            // col ->> 'k' = 'lit'
            if let Some(c) = json_extract_col(e.lexpr.as_deref()?) {
                return Some(c);
            }
            // jsonb_array_length(jsonb_path_query_array(col, …)) = N
            if let Some(c) = key_count_col(e.lexpr.as_deref()?) {
                return Some(c);
            }
            // col = lit
            return column_ref_name(e.lexpr.as_deref()?);
        }
    }
    if matches!(op, "?" | "?&") {
        return column_ref_name(e.lexpr.as_deref()?);
    }
    None
}

fn op_name(e: &protobuf::AExpr) -> Option<&str> {
    // `.last()` so schema-qualified operators like `OPERATOR(pg_catalog.=)`
    // match against the bare `=`, mirroring `is_named_func` below.
    e.name.last()
        .and_then(|n| n.node.as_ref())
        .and_then(|n| match n {
            NodeBody::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
}

// ---- jsonb_typeof handlers ----

/// `jsonb_typeof(<expr>) = '<type>'`.
fn classify_jsonb_typeof_eq(e: &protobuf::AExpr) -> Option<ClauseKind> {
    let fc = func_call(e.lexpr.as_deref()?)?;
    if !is_named_func(fc, "jsonb_typeof") || fc.args.len() != 1 {
        return None;
    }
    let type_name = string_literal(e.rexpr.as_deref()?)?;
    let json_type = match type_name.as_str() {
        "object" => Some(JsonFieldType::Object),
        "array" => Some(JsonFieldType::Array),
        "string" => Some(JsonFieldType::String),
        "number" => Some(JsonFieldType::Number),
        "boolean" => Some(JsonFieldType::Boolean),
        "null" => None,
        _ => return None,
    }?;
    // Two shapes: argument is a bare column → object marker; argument is
    // `col -> 'k'` → field type.
    let arg = &fc.args[0];
    if column_ref_name(arg).is_some() {
        if matches!(json_type, JsonFieldType::Object) {
            return Some(ClauseKind::JsonObjectMarker);
        }
        // jsonb_typeof(col) = 'string' (etc.) at the top level: less
        // useful for refinement (would shrink the col to a scalar JSON
        // value) — bail.
        return None;
    }
    if let Some((_, key)) = json_arrow_extract(arg) {
        return Some(ClauseKind::FieldType(key, json_type));
    }
    None
}

fn jsonb_typeof_col(node: &protobuf::Node) -> Option<String> {
    let fc = func_call(node)?;
    if !is_named_func(fc, "jsonb_typeof") || fc.args.len() != 1 {
        return None;
    }
    let arg = &fc.args[0];
    if let Some(c) = column_ref_name(arg) {
        return Some(c);
    }
    if let Some((c, _)) = json_arrow_extract(arg) {
        return Some(c);
    }
    None
}

// ---- col ->> 'k' = 'lit' ----

fn classify_field_literal(e: &protobuf::AExpr) -> Option<ClauseKind> {
    let (_col, key) = json_arrow2_extract(e.lexpr.as_deref()?)?;
    let lit = string_literal(e.rexpr.as_deref()?)?;
    Some(ClauseKind::FieldLiteralStr(key, lit))
}

fn json_extract_col(node: &protobuf::Node) -> Option<String> {
    if let Some((c, _)) = json_arrow_extract(node) {
        return Some(c);
    }
    if let Some((c, _)) = json_arrow2_extract(node) {
        return Some(c);
    }
    None
}

/// `col -> 'k'`   → returns `(col, k)`.
fn json_arrow_extract(node: &protobuf::Node) -> Option<(String, String)> {
    json_arrow_op(node, "->")
}

/// `col ->> 'k'`  → returns `(col, k)`.
fn json_arrow2_extract(node: &protobuf::Node) -> Option<(String, String)> {
    json_arrow_op(node, "->>")
}

fn json_arrow_op(node: &protobuf::Node, op: &str) -> Option<(String, String)> {
    let e = match node.node.as_ref()? {
        NodeBody::AExpr(e) => e,
        _ => return None,
    };
    if op_name(e)? != op || e.kind != protobuf::AExprKind::AexprOp as i32 {
        return None;
    }
    let col = column_ref_name(e.lexpr.as_deref()?)?;
    let key = string_literal(e.rexpr.as_deref()?)?;
    Some((col, key))
}

// ---- key-count idiom ----
//
// `jsonb_array_length(jsonb_path_query_array(col, '$.keyvalue().key')) = N`

fn classify_key_count(e: &protobuf::AExpr) -> Option<ClauseKind> {
    key_count_col(e.lexpr.as_deref()?)?;
    let n = int_literal(e.rexpr.as_deref()?)?;
    if n < 0 { return None; }
    Some(ClauseKind::KeyCount(n as usize))
}

fn key_count_col(node: &protobuf::Node) -> Option<String> {
    let outer = func_call(node)?;
    if !is_named_func(outer, "jsonb_array_length") || outer.args.len() != 1 {
        return None;
    }
    let inner = func_call(&outer.args[0])?;
    if !is_named_func(inner, "jsonb_path_query_array") || inner.args.len() < 2 {
        return None;
    }
    column_ref_name(&inner.args[0])
}

// ---- AST helpers ----

fn func_call(node: &protobuf::Node) -> Option<&protobuf::FuncCall> {
    match node.node.as_ref()? {
        NodeBody::FuncCall(f) => Some(f),
        _ => None,
    }
}

fn is_named_func(fc: &protobuf::FuncCall, name: &str) -> bool {
    let last = fc.funcname.last()
        .and_then(|n| n.node.as_ref())
        .and_then(|n| match n {
            NodeBody::String(s) => Some(s.sval.as_str()),
            _ => None,
        });
    last == Some(name)
}

fn column_ref_name(node: &protobuf::Node) -> Option<String> {
    let cr = match node.node.as_ref()? {
        NodeBody::ColumnRef(c) => c,
        _ => return None,
    };
    if cr.fields.len() != 1 {
        return None;
    }
    match cr.fields[0].node.as_ref()? {
        NodeBody::String(s) => Some(s.sval.clone()),
        _ => None,
    }
}

fn scalar_literal(node: &protobuf::Node) -> Option<Literal> {
    match node.node.as_ref()? {
        NodeBody::TypeCast(tc) => scalar_literal(tc.arg.as_deref()?),
        NodeBody::AConst(c) => {
            if c.isnull { return None; }
            match c.val.as_ref()? {
                a_const::Val::Sval(s) => Some(Literal::Str(s.sval.clone())),
                a_const::Val::Ival(Integer { ival }) => Some(Literal::Int(*ival as i64)),
                a_const::Val::Boolval(b) => Some(Literal::Bool(b.boolval)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn string_literal(node: &protobuf::Node) -> Option<String> {
    match scalar_literal(node)? {
        Literal::Str(s) => Some(s),
        _ => None,
    }
}

fn int_literal(node: &protobuf::Node) -> Option<i64> {
    match scalar_literal(node)? {
        Literal::Int(i) => Some(i),
        _ => None,
    }
}

fn quote_key(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.is_empty()
        && !s.chars().next().unwrap().is_ascii_digit()
    {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(def: &str) -> Option<(String, String)> {
        parse_check_def(def).map(|(c, r)| (c, r.render_ts().unwrap()))
    }

    // ---- Tier 1 ----

    #[test]
    fn equality_against_literal_set() {
        let (col, ts) = rendered(
            "CHECK ((color = ANY (ARRAY['red'::text, 'green'::text, 'blue'::text])))",
        ).unwrap();
        assert_eq!(col, "color");
        assert_eq!(ts, r#""red" | "green" | "blue""#);
    }

    #[test]
    fn single_string_literal() {
        let (col, ts) = rendered("CHECK ((kind = 'invoice'::text))").unwrap();
        assert_eq!(col, "kind");
        assert_eq!(ts, r#""invoice""#);
    }

    #[test]
    fn single_boolean_literal() {
        let (col, ts) = rendered("CHECK ((is_active = true))").unwrap();
        assert_eq!(col, "is_active");
        assert_eq!(ts, "true");
    }

    #[test]
    fn nullable_or_literal_set() {
        // `IS NULL OR <set>` produces a LiteralUnion with allow_null=true,
        // but the rendered TS doesn't include `| null` — codegen appends
        // it based on the column's `attnotnull` independently. Emitting
        // null here too would produce `… | null | null`.
        let (col, ts) = rendered(
            "CHECK (((priority IS NULL) OR (priority = ANY (ARRAY['low'::text, 'high'::text]))))",
        ).unwrap();
        assert_eq!(col, "priority");
        assert_eq!(ts, r#""low" | "high""#);
    }

    #[test]
    fn integer_equality() {
        let (col, ts) = rendered("CHECK ((rating = 5))").unwrap();
        assert_eq!(col, "rating");
        assert_eq!(ts, "5");
    }

    // ---- Tier 2 ----

    #[test]
    fn jsonb_object_with_required_typed_keys_open() {
        let (col, ts) = rendered(
            "CHECK (((jsonb_typeof(meta) = 'object'::text) \
              AND (meta ?& ARRAY['width'::text, 'height'::text]) \
              AND (jsonb_typeof((meta -> 'width'::text)) = 'number'::text) \
              AND (jsonb_typeof((meta -> 'height'::text)) = 'number'::text)))",
        ).unwrap();
        assert_eq!(col, "meta");
        // BTreeMap orders keys alphabetically.
        assert_eq!(ts, "{ height: number; width: number } & Record<string, Json>");
    }

    #[test]
    fn jsonb_object_with_single_required_key() {
        let (col, ts) = rendered(
            "CHECK (((jsonb_typeof(meta) = 'object'::text) AND (meta ? 'x'::text)))",
        ).unwrap();
        assert_eq!(col, "meta");
        assert_eq!(ts, "{ x: Json } & Record<string, Json>");
    }

    // ---- Tier 3 column-level ----

    #[test]
    fn jsonb_discriminated_union() {
        let (col, ts) = rendered(
            "CHECK (((((payload ->> 'kind'::text) = 'text'::text) \
                       AND (jsonb_typeof((payload -> 'body'::text)) = 'string'::text)) \
                     OR (((payload ->> 'kind'::text) = 'image'::text) \
                       AND (jsonb_typeof((payload -> 'url'::text)) = 'string'::text) \
                       AND (jsonb_typeof((payload -> 'alt'::text)) = 'string'::text))))",
        ).unwrap();
        assert_eq!(col, "payload");
        assert!(ts.contains("kind: \"text\""), "got {ts}");
        assert!(ts.contains("body: string"), "got {ts}");
        assert!(ts.contains("kind: \"image\""), "got {ts}");
        assert!(ts.contains("url: string"), "got {ts}");
        assert!(ts.contains("alt: string"), "got {ts}");
        assert!(ts.contains(" | "), "expected union, got {ts}");
    }

    // ---- Bail cases ----

    #[test]
    fn bails_on_relational_predicate() {
        assert!(parse_check_def("CHECK ((rating > 0))").is_none());
        assert!(parse_check_def("CHECK ((start_at < end_at))").is_none());
    }

    #[test]
    fn bails_on_between() {
        assert!(parse_check_def("CHECK ((rating BETWEEN 1 AND 5))").is_none());
    }

    #[test]
    fn bails_on_arbitrary_function() {
        assert!(parse_check_def("CHECK ((length(slug) > 0))").is_none());
        assert!(parse_check_def("CHECK ((slug ~ '^[a-z]+$'))").is_none());
    }

    #[test]
    fn bails_on_disjoint_or_columns() {
        // Two OR branches against different columns — can't reduce.
        assert!(parse_check_def(
            "CHECK (((color = 'red'::text) OR (kind = 'invoice'::text)))",
        ).is_none());
    }

    // ---- Tier 3 row-level ----

    fn types(cols: &[(&str, &str)]) -> HashMap<String, String> {
        cols.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn row_num_nonnulls_eq_one() {
        let t = types(&[("email", "string | null"), ("phone", "string | null")]);
        let r = parse_row_check_def(
            "CHECK ((num_nonnulls(email, phone) = 1))",
            &t,
        ).unwrap();
        assert_eq!(r.variants.len(), 2);
        // Variant 0: email non-null, phone null
        assert_eq!(r.variants[0].columns["email"], "string");
        assert_eq!(r.variants[0].columns["phone"], "null");
        // Variant 1: email null, phone non-null
        assert_eq!(r.variants[1].columns["email"], "null");
        assert_eq!(r.variants[1].columns["phone"], "string");
    }

    #[test]
    fn row_num_nonnulls_eq_one_three_cols() {
        let t = types(&[
            ("a", "string | null"),
            ("b", "string | null"),
            ("c", "string | null"),
        ]);
        let r = parse_row_check_def(
            "CHECK ((num_nonnulls(a, b, c) = 1))",
            &t,
        ).unwrap();
        assert_eq!(r.variants.len(), 3);
    }

    #[test]
    fn row_num_nonnulls_geq_one_bails() {
        let t = types(&[("a", "string | null"), ("b", "string | null")]);
        // ">=" isn't recognised — not enough info to enumerate variants.
        assert!(parse_row_check_def(
            "CHECK ((num_nonnulls(a, b) >= 1))",
            &t,
        ).is_none());
    }

    #[test]
    fn row_case_discriminated_union() {
        let t = types(&[("field_type", "string"), ("config", "Json")]);
        let r = parse_row_check_def(
            "CHECK (CASE \
                WHEN (field_type = 'text'::text) \
                  THEN (jsonb_typeof((config -> 'maxLength'::text)) = 'number'::text) \
                WHEN (field_type = 'select'::text) \
                  THEN (jsonb_typeof((config -> 'options'::text)) = 'array'::text) \
                ELSE false END)",
            &t,
        ).unwrap();
        assert_eq!(r.variants.len(), 2);
        assert_eq!(r.variants[0].columns["field_type"], "\"text\"");
        assert!(r.variants[0].columns["config"].contains("maxLength: number"));
        assert_eq!(r.variants[1].columns["field_type"], "\"select\"");
        assert!(r.variants[1].columns["config"].contains("options: Json[]"));
    }

    #[test]
    fn intersect_two_overlapping_sets() {
        let a = Refinement::LiteralUnion {
            literals: vec![Literal::Str("red".into()), Literal::Str("green".into())],
            allow_null: false,
        };
        let b = Refinement::LiteralUnion {
            literals: vec![Literal::Str("red".into()), Literal::Str("blue".into())],
            allow_null: false,
        };
        let merged = a.intersect(b).unwrap();
        let ts = merged.render_ts().unwrap();
        assert_eq!(ts, r#""red""#);
    }
}
