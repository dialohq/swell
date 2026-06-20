//! Lowering: pg_query parse tree → `Expr`.
//!
//! Single pass. Each visitor produces the verdict-ready node, with
//! base column refs resolved against the active `Scope`, function
//! calls categorised against the catalog short-name sets, and casts
//! recursed through.

use crate::analyzed::{Expr, FuncKind, Lit, ResolvedCol};
use crate::query::TableColRef;
use crate::scope::Scope;
use pg_query::protobuf::{a_const::Val, node::Node as NB, Node, SubLinkType, TypeName};

/// Aggregate functions that return `NULL` over an empty input set.
const NULLABLE_AGGS: &[&str] = &[
    "sum", "avg", "min", "max",
    "array_agg", "json_agg", "jsonb_agg",
    "string_agg", "bool_and", "bool_or",
];

/// Functions whose return value is guaranteed non-NULL by construction.
const NEVER_NULL_FUNCS: &[&str] = &[
    "count",
    // Window funcs over a non-empty partition.
    "row_number", "rank", "dense_rank", "ntile", "cume_dist", "percent_rank",
    // Session / time builtins.
    "now", "current_timestamp", "current_date", "current_time",
    "localtimestamp", "localtime",
    "current_user", "session_user", "current_database",
    "current_schema", "current_setting",
    "gen_random_uuid", "uuid_generate_v1", "uuid_generate_v4",
    "pg_advisory_lock", "pg_advisory_xact_lock",
    // JSON / row builders — construct value from args, never short-circuit.
    "jsonb_build_object", "json_build_object",
    "jsonb_build_array", "json_build_array",
    "to_jsonb", "to_json", "row_to_json", "array_to_json",
];

/// Lower a SQL expression node to `Expr`. Returns `Expr::Unknown` for
/// shapes we don't categorise — caller defers to `attnotnull` or
/// renders as nullable.
pub fn lower(node: &Node, scope: &Scope) -> Expr {
    let Some(body) = node.node.as_ref() else { return Expr::Unknown };
    match body {
        NB::AConst(c) => {
            if c.isnull { return Expr::Null; }
            match c.val.as_ref() {
                Some(Val::Sval(s)) => Expr::Literal(Lit::Str(s.sval.clone())),
                Some(Val::Ival(i)) => Expr::Literal(Lit::Num(i.ival.to_string())),
                Some(Val::Fval(f)) => Expr::Literal(Lit::Num(f.fval.clone())),
                Some(Val::Boolval(b)) => Expr::Literal(Lit::Bool(b.boolval)),
                _ => Expr::Unknown,
            }
        }

        // `<expr>::T` — recurse on the inner and check the specific
        // `(source_oid, target_oid)` pair against this database's
        // user-defined `castmethod='f'` set.
        NB::TypeCast(tc) => {
            let inner = tc.arg.as_deref()
                .map(|a| lower(a, scope))
                .unwrap_or(Expr::Unknown);
            let target_oid = tc.type_name.as_ref()
                .and_then(|tn| resolve_typename_oid(tn, scope))
                .unwrap_or(0);
            let source_oid = inner_type_oid(&inner);
            let is_unsafe = match (source_oid, target_oid) {
                (Some(src), tgt) if tgt != 0 => scope.is_unsafe_cast(src, tgt),
                _ => false,
            };
            Expr::Cast { inner: Box::new(inner), target_oid, is_unsafe }
        }

        // `ARRAY[…]` — non-null regardless of elements.
        NB::AArrayExpr(_) => Expr::ArrayConstructor,

        // Parse-time COALESCE comes through as a FuncCall; the
        // CoalesceExpr variant exists in the protobuf but post-analysis.
        NB::CoalesceExpr(ce) => Expr::Coalesce(lower_args(&ce.args, scope)),

        NB::CaseExpr(ce) => {
            let has_else_non_null = ce.defresult.as_deref()
                .map(|d| is_non_null(&lower(d, scope)))
                .unwrap_or(false);
            Expr::Case { has_else_non_null }
        }

        NB::FuncCall(fc) => {
            let name = match fc.funcname.last().and_then(|n| n.node.as_ref()) {
                Some(NB::String(s)) => s.sval.as_str(),
                _ => return Expr::Unknown,
            };
            if name == "coalesce" {
                Expr::Coalesce(lower_args(&fc.args, scope))
            } else if NEVER_NULL_FUNCS.contains(&name) {
                Expr::Func { kind: FuncKind::NeverNull, args: lower_args(&fc.args, scope) }
            } else if NULLABLE_AGGS.contains(&name) {
                Expr::Func { kind: FuncKind::NullableAgg, args: lower_args(&fc.args, scope) }
            } else {
                Expr::Func { kind: FuncKind::Other, args: lower_args(&fc.args, scope) }
            }
        }

        NB::ColumnRef(cr) => match lower_column_ref(cr, scope) {
            Some(rc) => Expr::Column(rc),
            None => Expr::Unknown,
        },

        NB::SubLink(sl) => lower_sublink(sl, scope),

        _ => Expr::Unknown,
    }
}

/// Type-aware SubLink lowering. The `SubLinkType` discriminator
/// changes the result's nullability story:
///
///   - `EXISTS_SUBLINK`     — boolean, never NULL (true | false).
///   - `ARRAY_SUBLINK`      — array, never NULL (`[]` for zero rows).
///   - `EXPR_SUBLINK`       — scalar; returns NULL if the subquery has
///     zero rows. Provably non-null only when the subquery is a
///     single-row aggregate (target_list of `count`/`sum`/`max`/… and
///     no GROUP BY) — in that case the verdict comes from the inner
///     aggregate (count → non-null, sum/max → nullable).
///   - `ANY_SUBLINK` / `ALL_SUBLINK` / `ROWCOMPARE_SUBLINK` — boolean
///     comparison; result can be NULL if operands include NULL.
///   - `MULTIEXPR_SUBLINK` / `CTE_SUBLINK` — rare; default to Unknown.
fn lower_sublink(sl: &pg_query::protobuf::SubLink, scope: &Scope) -> Expr {
    let kind = SubLinkType::try_from(sl.sub_link_type).unwrap_or(SubLinkType::Undefined);
    match kind {
        SubLinkType::ExistsSublink => Expr::Func {
            kind: FuncKind::NeverNull, args: Vec::new(),
        },
        SubLinkType::ArraySublink => Expr::ArrayConstructor,
        SubLinkType::ExprSublink => {
            let Some(sub) = sl.subselect.as_deref() else { return Expr::Unknown };
            let Some(NB::SelectStmt(s)) = sub.node.as_ref() else { return Expr::Unknown };
            if !is_provably_one_row_select(s) { return Expr::Unknown; }
            let first = s.target_list.first()
                .and_then(|t| match t.node.as_ref()? {
                    NB::ResTarget(rt) => rt.val.as_deref(),
                    _ => None,
                });
            match first {
                Some(node) => lower(node, scope),
                None => Expr::Unknown,
            }
        }
        // Boolean comparisons (= ANY, = ALL, row-compare) can carry
        // NULL through the operator semantics.
        SubLinkType::AnySublink
        | SubLinkType::AllSublink
        | SubLinkType::RowcompareSublink => Expr::Unknown,
        _ => Expr::Unknown,
    }
}

/// True iff the SELECT is guaranteed to return exactly one row — the
/// only shape where the scalar form `(SELECT … )` can't yield NULL by
/// the "zero rows" route. Aggregate-only target lists without a
/// GROUP BY satisfy this; everything else admits zero rows.
fn is_provably_one_row_select(s: &pg_query::protobuf::SelectStmt) -> bool {
    if !s.group_clause.is_empty() { return false; }
    if s.target_list.is_empty() { return false; }
    s.target_list.iter().all(|t| {
        let Some(NB::ResTarget(rt)) = t.node.as_ref() else { return false };
        let Some(val) = rt.val.as_deref() else { return false };
        let Some(NB::FuncCall(fc)) = val.node.as_ref() else { return false };
        let name = fc.funcname.last().and_then(|n| match n.node.as_ref()? {
            NB::String(s) => Some(s.sval.as_str()),
            _ => None,
        });
        matches!(name, Some(n) if NEVER_NULL_FUNCS.contains(&n) || NULLABLE_AGGS.contains(&n))
    })
}

fn lower_args(args: &[Node], scope: &Scope) -> Vec<Expr> {
    args.iter().map(|a| lower(a, scope)).collect()
}

/// Resolve a `<alias>.<col>` (or bare `<col>` when unambiguous) to a
/// `ResolvedCol` with `not_null` reflecting attnotnull AND outer-join
/// widening AND any all-literal source override.
fn lower_column_ref(cr: &pg_query::protobuf::ColumnRef, scope: &Scope) -> Option<ResolvedCol> {
    let parts: Vec<&str> = cr.fields.iter()
        .filter_map(|n| match n.node.as_ref()? {
            NB::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    let (alias, col) = match parts.as_slice() {
        [col] => {
            let resolved = scope.resolve_bare(col)?;
            return Some(ResolvedCol {
                table_ref: TableColRef {
                    schema: resolved.schema.clone(),
                    table: resolved.table.clone(),
                    column: (*col).to_string(),
                },
                alias: resolved.alias,
                not_null: resolved.not_null,
                typoid: resolved.typoid,
            });
        }
        [alias, col] => (*alias, *col),
        [_schema, alias, col] => (*alias, *col),
        _ => return None,
    };
    if let Some(table) = scope.resolve_alias(alias) {
        let base_not_null = table.col_not_null(col).unwrap_or(false);
        let widened = scope.is_nullable_alias(alias);
        let force_non_null = scope.is_non_null_alias(alias);
        let not_null = (base_not_null || force_non_null) && !widened;
        return Some(ResolvedCol {
            table_ref: TableColRef {
                schema: table.schema.clone(),
                table: table.name.clone(),
                column: col.to_string(),
            },
            alias: alias.to_string(),
            not_null,
            typoid: table.col_typoid(col).unwrap_or(0),
        });
    }
    // Derived-table alias (RangeSubselect / CTE) — find the column by
    // name in the pre-lowered derived list and inherit its non-null
    // verdict via `is_non_null(child_expr)`.
    if let Some(derived) = scope.derived(alias) {
        let child = derived.iter().find(|c| c.name == col)?;
        return Some(ResolvedCol {
            table_ref: TableColRef {
                schema: String::new(),
                table: alias.to_string(),
                column: col.to_string(),
            },
            alias: alias.to_string(),
            not_null: is_non_null(&child.expr),
            typoid: 0,
        });
    }
    // Alias isn't a real base table in the plan walk's `alias_to_table`.
    // For literal-source aliases (VALUES, literal unnest) — the plan
    // walk added them to `non_null` — we still want a `ResolvedCol`,
    // since the verdict is "non-null by construction." We don't know
    // the (schema, table) so leave them empty; codegen ignores
    // `table_ref` for these because there's no surrounding interface.
    if scope.is_non_null_alias(alias) && !scope.is_nullable_alias(alias) {
        return Some(ResolvedCol {
            table_ref: TableColRef {
                schema: String::new(),
                table: alias.to_string(),
                column: col.to_string(),
            },
            alias: alias.to_string(),
            not_null: true,
            typoid: 0,
        });
    }
    None
}

/// True iff `e` represents a value provably non-NULL at runtime.
/// Shared by `Coalesce`'s "any arg non-null" check and `Case`'s "ELSE
/// branch non-null" check. For `SetOp`, every branch must be non-null.
///
/// `Cast`: when `is_unsafe` is `false` (the typical case — built-in
/// I/O / binary / `pg_catalog` function casts never return NULL on
/// non-NULL input) we inherit the inner verdict. When `is_unsafe` is
/// `true`, the specific `(source, target)` pair has a user-defined
/// `castmethod='f'` in `pg_cast` that could return NULL even on
/// non-null input — verdict drops to "not provably non-null."
pub fn is_non_null(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) => true,
        Expr::ArrayConstructor => true,
        Expr::Cast { inner, is_unsafe, .. } => !*is_unsafe && is_non_null(inner),
        Expr::Column(c) => c.not_null,
        Expr::Func { kind: FuncKind::NeverNull, .. } => true,
        Expr::Coalesce(args) => args.iter().any(is_non_null),
        Expr::Case { has_else_non_null } => *has_else_non_null,
        Expr::SubQuery(a) => a.outputs.first().is_some_and(|o| is_non_null(&o.expr)),
        Expr::SetOp(branches) => !branches.is_empty() && branches.iter().all(is_non_null),
        _ => false,
    }
}

/// Strong "this column is nullable" verdict — used to override an
/// otherwise-NOT-NULL base column when, e.g., an outer join widens it
/// or a user-defined cast might return NULL.
///
/// `Cast { is_unsafe: true }` widens even a NOT NULL inner — the
/// specific `(source, target)` pair has a user-defined `castmethod='f'`
/// that could return NULL on non-null input.
pub fn is_nullable(e: &Expr) -> bool {
    match e {
        Expr::Null => true,
        Expr::Cast { inner, is_unsafe, .. } => is_nullable(inner) || *is_unsafe,
        Expr::Column(c) => !c.not_null,
        Expr::Func { kind: FuncKind::NullableAgg, .. } => true,
        Expr::Case { has_else_non_null: false } => true,
        Expr::SetOp(branches) => branches.iter().any(is_nullable),
        _ => false,
    }
}

/// Peel any `Cast` wrappers and return the underlying `Lit` iff the
/// expression is structurally a single literal.
pub fn as_literal(e: &Expr) -> Option<&Lit> {
    match e {
        Expr::Literal(l) => Some(l),
        Expr::Cast { inner, .. } => as_literal(inner),
        _ => None,
    }
}

/// Peel any `Cast` wrappers and return the underlying base-column
/// reference iff the expression is structurally a single column ref.
/// Used by FULL JOIN row-variant building to know which side an
/// output column came from via `ResolvedCol.alias`.
pub fn as_column(e: &Expr) -> Option<&ResolvedCol> {
    match e {
        Expr::Column(c) => Some(c),
        Expr::Cast { inner, .. } => as_column(inner),
        _ => None,
    }
}

/// Source-type OID of the expression PG would feed to a wrapping cast.
/// Used to compute `Cast.is_unsafe` at lowering. Returns `None` for
/// shapes whose type isn't trivially derivable from the lowered tree
/// — caller treats unknown source as "default safe" (no widening).
fn inner_type_oid(e: &Expr) -> Option<u32> {
    match e {
        Expr::Column(c) => Some(c.typoid),
        Expr::Cast { target_oid, .. } => (*target_oid != 0).then_some(*target_oid),
        // Literal / ArrayConstructor / Func / Coalesce / Case / SetOp /
        // SubQuery have types we don't carry on the lowered tree;
        // returning None defaults to safe.
        _ => None,
    }
}

/// Resolve a `TypeName` (e.g. `pg_catalog.text`, `mytype`,
/// `myschema.mytype`) to its `pg_type.oid` via the scope's pre-fetched
/// name → oid map. Returns `None` if the name isn't in the catalog.
fn resolve_typename_oid(tn: &TypeName, scope: &Scope) -> Option<u32> {
    let last = tn.names.last()?;
    let name = match last.node.as_ref()? {
        NB::String(s) => s.sval.as_str(),
        _ => return None,
    };
    scope.typname_oid(name)
}
