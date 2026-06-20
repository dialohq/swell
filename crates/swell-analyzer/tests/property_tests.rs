//! Schema-grounded property tests.
//!
//! For each generated schema we apply DDL, then run a fixed set of
//! canonical queries through `Analyzer::analyze` and assert properties
//! of the result against the schema itself — the "oracle". The schema
//! is the source of truth: it knows each column's type and
//! nullability, and Postgres enforces both at runtime, so any analyzer
//! output that disagrees is a real bug.
//!
//! Properties asserted (per generated schema):
//!
//!   SELECT col1, col2, … FROM t
//!     - one inferred column per selected column, in order
//!     - column.name matches schema column name
//!     - column.ts_type matches the schema's mapped TS type
//!     - column.nullable == !schema_column.not_null
//!     - column.table_ref points back to (this schema, this table, this column)
//!
//!   INSERT INTO t (col1, col2) VALUES ($1, $2) RETURNING *
//!     - one param per VALUES position
//!     - param.nullable == !target_column.not_null  (tightening rule)
//!     - param.table_ref points back at the target column
//!     - RETURNING columns mirror the SELECT * property set
//!
//!   UPDATE t SET col1 = $1 WHERE col2 = $2
//!     - $1 nullability mirrors target column attnotnull
//!     - $2 stays nullable (WHERE — null compares are well-defined)
//!     - $2 has no table_ref (WHERE isn't a direct binding)
//!
//!   SELECT a.col, b.col FROM t a LEFT JOIN t b ON a.id = b.id
//!     - rhs (b.*) columns become nullable in the inferred row even
//!       though their base attnotnull is set — outer-join widening.
//!
//!   SELECT f($1)  for each generated function f
//!     - param type matches the function's argument type, including
//!       custom enum types (param.ts_type contains the enum labels).
//!
//! Each proptest case runs in its own per-schema namespace (`CREATE
//! SCHEMA prop_<rand>`) so cases are independent without
//! database-creation overhead.

use proptest::prelude::*;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use swell_analyzer::{Analyzer, AnalyzerOptions, InferredColumn, InferredParam};
use tokio::runtime::Runtime;
use tokio::sync::OnceCell;

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .expect("property tests require DATABASE_URL pointing at a dev Postgres")
}

/// One shared runtime + one shared analyzer across all proptest cases.
/// proptest runs cases serially inside a single #[test], so a static
/// is safe. The analyzer holds a multiplexed `tokio_postgres::Client`
/// whose connection task is spawned onto this runtime; re-entering
/// `block_on` while a connection task is live is a runtime-within-a-
/// runtime panic, so the analyzer is built lazily via tokio's async
/// `OnceCell` — only one block_on per case, not nested.
fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime")
    })
}

static ANALYZER: OnceCell<Analyzer> = OnceCell::const_new();

async fn analyzer() -> &'static Analyzer {
    ANALYZER
        .get_or_init(|| async {
            Analyzer::connect(AnalyzerOptions {
                database_url: database_url(),
                schemas: vec!["public".into()],
                type_overrides: BTreeMap::new(),
            })
            .await
            .expect("connect")
        })
        .await
}

// ---------- generated schema model ----------

#[derive(Debug, Clone, PartialEq, Eq)]
enum PgType {
    Int,
    BigInt,
    Text,
    Bool,
    Uuid,
    TimestampTz,
    Enum(String), // refs `GenEnum.name`
}

impl PgType {
    fn ddl(&self, namespace: &str) -> String {
        match self {
            PgType::Int => "integer".into(),
            PgType::BigInt => "bigint".into(),
            PgType::Text => "text".into(),
            PgType::Bool => "boolean".into(),
            PgType::Uuid => "uuid".into(),
            PgType::TimestampTz => "timestamptz".into(),
            PgType::Enum(name) => format!("{namespace}.{name}"),
        }
    }

    /// Expected ts_type as rendered by swell's catalog. For enums, the
    /// labels are joined with " | ", in `enumsortorder` (insertion) order.
    fn expected_ts(&self, enums: &[GenEnum]) -> String {
        match self {
            PgType::Int => "number".into(),
            PgType::BigInt => "string".into(), // bigint → string in node-pg
            PgType::Text => "string".into(),
            PgType::Bool => "boolean".into(),
            PgType::Uuid => "string".into(),
            PgType::TimestampTz => "Date".into(),
            PgType::Enum(name) => {
                let e = enums
                    .iter()
                    .find(|e| &e.name == name)
                    .expect("enum ref resolves");
                e.labels
                    .iter()
                    .map(|l| format!("\"{l}\""))
                    .collect::<Vec<_>>()
                    .join(" | ")
            }
        }
    }
}

#[derive(Debug, Clone)]
struct GenColumn {
    name: String,
    ty: PgType,
    not_null: bool,
}

#[derive(Debug, Clone)]
struct GenTable {
    name: String,
    columns: Vec<GenColumn>,
}

#[derive(Debug, Clone)]
struct GenEnum {
    name: String,
    labels: Vec<String>,
}

#[derive(Debug, Clone)]
struct GenFunction {
    name: String,
    arg_type: PgType,
    return_type: PgType,
}

#[derive(Debug, Clone)]
struct GenSchema {
    namespace: String,
    enums: Vec<GenEnum>,
    tables: Vec<GenTable>,
    functions: Vec<GenFunction>,
}

impl GenSchema {
    fn ddl(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE;\n",
            self.namespace
        ));
        out.push_str(&format!("CREATE SCHEMA {};\n", self.namespace));
        for e in &self.enums {
            let labels = e
                .labels
                .iter()
                .map(|l| format!("'{l}'"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "CREATE TYPE {}.{} AS ENUM ({});\n",
                self.namespace, e.name, labels
            ));
        }
        for t in &self.tables {
            let cols = t
                .columns
                .iter()
                .map(|c| {
                    let nn = if c.not_null { " NOT NULL" } else { "" };
                    format!("  {} {}{}", c.name, c.ty.ddl(&self.namespace), nn)
                })
                .collect::<Vec<_>>()
                .join(",\n");
            out.push_str(&format!(
                "CREATE TABLE {}.{} (\n{}\n);\n",
                self.namespace, t.name, cols,
            ));
        }
        for f in &self.functions {
            // IMMUTABLE SQL function so PG can describe call sites
            // without needing actual rows. Returns its argument cast
            // through any expression we like; here we just echo.
            out.push_str(&format!(
                "CREATE FUNCTION {ns}.{name}(x {arg}) RETURNS {ret} \
                 LANGUAGE sql IMMUTABLE AS $$ SELECT NULL::{ret} $$;\n",
                ns = self.namespace,
                name = f.name,
                arg = f.arg_type.ddl(&self.namespace),
                ret = f.return_type.ddl(&self.namespace),
            ));
        }
        out
    }
}

// ---------- proptest strategies ----------

fn enum_strategy(idx: usize) -> impl Strategy<Value = GenEnum> {
    let name = format!("e_{idx}");
    proptest::collection::vec(prop::string::string_regex("[a-z]{1,5}").unwrap(), 2..=4).prop_map(
        move |labels| {
            // Dedup (proptest may shrink to repeated values) and guarantee ≥2 labels.
            let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            let labels: Vec<String> = labels
                .into_iter()
                .filter(|l| seen.insert(l.clone()))
                .collect();
            let labels = if labels.len() >= 2 {
                labels
            } else {
                vec!["a".into(), "b".into()]
            };
            GenEnum {
                name: name.clone(),
                labels,
            }
        },
    )
}

fn type_strategy(enum_names: Vec<String>) -> impl Strategy<Value = PgType> {
    let base = prop_oneof![
        Just(PgType::Int),
        Just(PgType::BigInt),
        Just(PgType::Text),
        Just(PgType::Bool),
        Just(PgType::Uuid),
        Just(PgType::TimestampTz),
    ];
    if enum_names.is_empty() {
        base.boxed()
    } else {
        prop_oneof![
            6 => base,
            1 => prop::sample::select(enum_names).prop_map(PgType::Enum),
        ]
        .boxed()
    }
}

fn column_strategy(idx: usize, enum_names: Vec<String>) -> impl Strategy<Value = GenColumn> {
    let name = format!("c_{idx}");
    (type_strategy(enum_names), any::<bool>()).prop_map(move |(ty, not_null)| GenColumn {
        name: name.clone(),
        ty,
        not_null,
    })
}

fn table_strategy(idx: usize, enum_names: Vec<String>) -> impl Strategy<Value = GenTable> {
    let name = format!("t_{idx}");
    proptest::collection::vec(0usize..6, 2..=5)
        .prop_flat_map(move |col_idxs| {
            let enum_names = enum_names.clone();
            col_idxs
                .into_iter()
                .enumerate()
                .map(|(i, _)| column_strategy(i, enum_names.clone()))
                .collect::<Vec<_>>()
        })
        .prop_map(move |columns| {
            // Dedup column names — proptest may shrink to duplicates.
            let mut seen = std::collections::HashSet::new();
            let columns: Vec<GenColumn> = columns
                .into_iter()
                .filter(|c| seen.insert(c.name.clone()))
                .collect();
            GenTable {
                name: name.clone(),
                columns: if columns.is_empty() {
                    // Always at least one column.
                    vec![GenColumn {
                        name: "c_0".into(),
                        ty: PgType::Int,
                        not_null: true,
                    }]
                } else {
                    columns
                },
            }
        })
}

fn function_strategy(idx: usize, enum_names: Vec<String>) -> impl Strategy<Value = GenFunction> {
    let name = format!("f_{idx}");
    let arg_ty = type_strategy(enum_names.clone());
    let ret_ty = type_strategy(enum_names);
    (arg_ty, ret_ty).prop_map(move |(a, r)| GenFunction {
        name: name.clone(),
        arg_type: a,
        return_type: r,
    })
}

fn schema_strategy() -> impl Strategy<Value = GenSchema> {
    // 0-2 enums, 1-2 tables, 0-2 functions.
    (
        proptest::collection::vec(enum_strategy(0), 0..=2),
        proptest::collection::vec(0..2usize, 1..=2),
        proptest::collection::vec(0..2usize, 0..=2),
    )
        .prop_flat_map(|(enums_seed, table_idxs, fn_idxs)| {
            let enums: Vec<GenEnum> = enums_seed
                .into_iter()
                .enumerate()
                .map(|(i, e)| GenEnum {
                    name: format!("e_{i}"),
                    ..e
                })
                .collect();
            let enum_names: Vec<String> = enums.iter().map(|e| e.name.clone()).collect();
            let tables = table_idxs
                .into_iter()
                .enumerate()
                .map(|(i, _)| table_strategy(i, enum_names.clone()))
                .collect::<Vec<_>>();
            let functions = fn_idxs
                .into_iter()
                .enumerate()
                .map(|(i, _)| function_strategy(i, enum_names.clone()))
                .collect::<Vec<_>>();
            (Just(enums), tables, functions)
        })
        .prop_map(|(enums, tables, functions)| GenSchema {
            // Random-ish namespace, but stable per-case via the table count + first enum's labels
            // count: proptest shrinkability matters here. Use a fixed prefix
            // and let the test driver pick a unique suffix.
            namespace: "prop_test_ns".into(),
            enums,
            tables,
            functions,
        })
}

// ---------- property checks ----------

fn assert_column_matches_schema(
    qname: &str,
    idx: usize,
    inferred: &InferredColumn,
    schema_col: &GenColumn,
    enums: &[GenEnum],
    namespace: &str,
    table: &str,
) {
    let expected_ts = schema_col.ty.expected_ts(enums);
    assert_eq!(
        inferred.name, schema_col.name,
        "[{qname}] column {idx} name"
    );
    assert_eq!(
        inferred.ts_type, expected_ts,
        "[{qname}] column {idx} ts_type — expected `{expected_ts}` for schema type {:?}",
        schema_col.ty
    );
    assert_eq!(
        inferred.nullable, !schema_col.not_null,
        "[{qname}] column {idx} `{}` nullability — schema NOT NULL={}, swell.nullable={}",
        schema_col.name, schema_col.not_null, inferred.nullable
    );
    let r = inferred.table_ref.as_ref().unwrap_or_else(|| {
        panic!(
            "[{qname}] column {idx} `{}` should carry table_ref",
            schema_col.name
        )
    });
    assert_eq!(
        r.schema, namespace,
        "[{qname}] column {idx} table_ref schema"
    );
    assert_eq!(r.table, table, "[{qname}] column {idx} table_ref table");
    assert_eq!(
        r.column, schema_col.name,
        "[{qname}] column {idx} table_ref column"
    );
}

fn assert_param_matches_target(
    qname: &str,
    idx: usize,
    inferred: &InferredParam,
    target: &GenColumn,
    enums: &[GenEnum],
    namespace: &str,
    table: &str,
) {
    let expected_ts = target.ty.expected_ts(enums);
    assert_eq!(
        inferred.ts_type,
        expected_ts,
        "[{qname}] param ${} ts_type — expected `{expected_ts}` for target {:?}",
        idx + 1,
        target.ty
    );
    assert_eq!(
        inferred.nullable,
        !target.not_null,
        "[{qname}] param ${} nullable should mirror target column NOT NULL (target {}={})",
        idx + 1,
        target.name,
        target.not_null
    );
    let r = inferred.table_ref.as_ref().unwrap_or_else(|| {
        panic!(
            "[{qname}] param ${} should carry table_ref to {table}.{}",
            idx + 1,
            target.name
        )
    });
    assert_eq!(r.schema, namespace);
    assert_eq!(r.table, table);
    assert_eq!(r.column, target.name);
}

async fn run_one(schema: GenSchema, case_id: u32) {
    let an = analyzer().await;
    let namespace = format!("prop_{case_id}");
    let mut schema = schema;
    schema.namespace = namespace.clone();

    // Apply DDL.
    an.client
        .simple_query(&schema.ddl())
        .await
        .unwrap_or_else(|e| panic!("apply DDL: {e}\n--- DDL ---\n{}", schema.ddl()));

    // Run the property checks. Drop the schema only on success — on
    // failure we leave it behind so the failed DDL is inspectable from
    // psql for debugging. The next case's `DROP SCHEMA IF EXISTS` in
    // its own DDL covers cleanup of stale schemas anyway.
    check_all(&schema).await;
    let _ = an
        .client
        .simple_query(&format!("DROP SCHEMA IF EXISTS {namespace} CASCADE"))
        .await;
}

async fn check_all(schema: &GenSchema) {
    for table in &schema.tables {
        check_select_star(schema, table).await;
        check_select_subset(schema, table).await;
        check_insert_returning(schema, table).await;
        check_insert_multi_row(schema, table).await;
        check_insert_coalesce_keeps_param_nullable(schema, table).await;
        check_update_with_where(schema, table).await;
    }
    if schema.tables.len() >= 2 {
        check_left_join(schema, &schema.tables[0], &schema.tables[1]).await;
    }
    for f in &schema.functions {
        check_function_call(schema, f).await;
    }
}

async fn check_select_star(schema: &GenSchema, table: &GenTable) {
    let an = analyzer().await;
    let sql = format!("SELECT * FROM {}.{}", schema.namespace, table.name);
    let q = an
        .analyze(&sql)
        .await
        .unwrap_or_else(|e| panic!("SELECT *: {e}\nSQL: {sql}"));
    assert_eq!(
        q.columns.len(),
        table.columns.len(),
        "SELECT * returned wrong column count"
    );
    for (i, (inferred, expected)) in q.columns.iter().zip(&table.columns).enumerate() {
        assert_column_matches_schema(
            "SELECT *",
            i,
            inferred,
            expected,
            &schema.enums,
            &schema.namespace,
            &table.name,
        );
    }
}

async fn check_select_subset(schema: &GenSchema, table: &GenTable) {
    // Take the first half of columns. Easier than randomizing.
    let n = (table.columns.len() / 2).max(1);
    let subset: Vec<&GenColumn> = table.columns.iter().take(n).collect();
    let list = subset
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let an = analyzer().await;
    let sql = format!("SELECT {list} FROM {}.{}", schema.namespace, table.name);
    let q = an
        .analyze(&sql)
        .await
        .unwrap_or_else(|e| panic!("SELECT subset: {e}\nSQL: {sql}"));
    assert_eq!(q.columns.len(), subset.len());
    for (i, (inferred, expected)) in q.columns.iter().zip(&subset).enumerate() {
        assert_column_matches_schema(
            "SELECT subset",
            i,
            inferred,
            expected,
            &schema.enums,
            &schema.namespace,
            &table.name,
        );
    }
}

async fn check_insert_returning(schema: &GenSchema, table: &GenTable) {
    let cols = &table.columns;
    let col_list = cols
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let values = (1..=cols.len())
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let an = analyzer().await;
    let sql = format!(
        "INSERT INTO {}.{} ({col_list}) VALUES ({values}) RETURNING *",
        schema.namespace, table.name,
    );
    let q = an
        .analyze(&sql)
        .await
        .unwrap_or_else(|e| panic!("INSERT … RETURNING *: {e}\nSQL: {sql}"));
    // params
    assert_eq!(
        q.params.len(),
        cols.len(),
        "param count mismatch on INSERT VALUES"
    );
    for (i, (param, target)) in q.params.iter().zip(cols).enumerate() {
        assert_param_matches_target(
            "INSERT VALUES",
            i,
            param,
            target,
            &schema.enums,
            &schema.namespace,
            &table.name,
        );
    }
    // RETURNING * columns
    assert_eq!(
        q.columns.len(),
        cols.len(),
        "RETURNING * column count mismatch"
    );
    for (i, (inferred, expected)) in q.columns.iter().zip(cols).enumerate() {
        assert_column_matches_schema(
            "INSERT … RETURNING *",
            i,
            inferred,
            expected,
            &schema.enums,
            &schema.namespace,
            &table.name,
        );
    }
}

/// Multi-row INSERT: each row contributes one param per column, and
/// every `$N` should still bind to its target column.
///
///   INSERT INTO t (a, b) VALUES ($1, $2), ($3, $4)
///   -- $1, $3 → a;  $2, $4 → b
async fn check_insert_multi_row(schema: &GenSchema, table: &GenTable) {
    let cols = &table.columns;
    let col_list = cols
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    // Build two rows.
    let row1 = (1..=cols.len())
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let row2 = ((cols.len() + 1)..=(2 * cols.len()))
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let an = analyzer().await;
    let sql = format!(
        "INSERT INTO {}.{} ({col_list}) VALUES ({row1}), ({row2})",
        schema.namespace, table.name,
    );
    let q = an
        .analyze(&sql)
        .await
        .unwrap_or_else(|e| panic!("multi-row INSERT: {e}\nSQL: {sql}"));
    assert_eq!(
        q.params.len(),
        2 * cols.len(),
        "multi-row INSERT param count mismatch"
    );
    // Both rows bind to the same column list, so $i and $(i+ncols) both
    // mirror column[i].
    for i in 0..cols.len() {
        assert_param_matches_target(
            "multi-row INSERT row1",
            i,
            &q.params[i],
            &cols[i],
            &schema.enums,
            &schema.namespace,
            &table.name,
        );
        assert_param_matches_target(
            "multi-row INSERT row2",
            i,
            &q.params[i + cols.len()],
            &cols[i],
            &schema.enums,
            &schema.namespace,
            &table.name,
        );
    }
}

/// `INSERT INTO t (col) VALUES (coalesce($1, 'lit'))` — even though
/// `col` may be NOT NULL, `$1` should stay NULLABLE because the
/// coalesce wraps it. swell's `param_nullability` documents this
/// exception explicitly; this property pins it.
///
/// Only runs for tables that have a NOT NULL text/int column we can
/// supply a literal default for — the property is moot when the
/// target column already accepts null.
async fn check_insert_coalesce_keeps_param_nullable(schema: &GenSchema, table: &GenTable) {
    // Find a NOT NULL column whose type we can produce a literal for.
    let Some(target) = table.columns.iter().find(|c| {
        c.not_null
            && matches!(
                c.ty,
                PgType::Int | PgType::BigInt | PgType::Text | PgType::Bool,
            )
    }) else {
        return;
    };
    let literal: &str = match target.ty {
        PgType::Int | PgType::BigInt => "0",
        PgType::Text => "''",
        PgType::Bool => "false",
        _ => unreachable!(),
    };
    let an = analyzer().await;
    let sql = format!(
        "INSERT INTO {}.{} ({}) VALUES (coalesce($1, {literal}))",
        schema.namespace, table.name, target.name,
    );
    let q = an
        .analyze(&sql)
        .await
        .unwrap_or_else(|e| panic!("coalesce INSERT: {e}\nSQL: {sql}"));
    assert_eq!(q.params.len(), 1);
    assert!(
        q.params[0].nullable,
        "coalesce($1, lit): $1 should stay nullable even when target ({}.{}) is NOT NULL — \
         coalesce substitutes the literal at runtime",
        table.name, target.name
    );
    assert!(
        q.params[0].table_ref.is_none(),
        "coalesce($1, lit): $1 is not a direct column binding — should have no table_ref"
    );
}

async fn check_update_with_where(schema: &GenSchema, table: &GenTable) {
    if table.columns.len() < 2 {
        return;
    }
    let set_col = &table.columns[0];
    let where_col = &table.columns[1];
    let an = analyzer().await;
    let sql = format!(
        "UPDATE {}.{} SET {} = $1 WHERE {} = $2",
        schema.namespace, table.name, set_col.name, where_col.name,
    );
    let q = an
        .analyze(&sql)
        .await
        .unwrap_or_else(|e| panic!("UPDATE SET … WHERE: {e}\nSQL: {sql}"));
    assert_eq!(q.params.len(), 2);
    // $1 mirrors SET-target column.
    assert_param_matches_target(
        "UPDATE SET",
        0,
        &q.params[0],
        set_col,
        &schema.enums,
        &schema.namespace,
        &table.name,
    );
    // $2 is in WHERE — stays nullable, no table_ref.
    assert_eq!(
        q.params[1].ts_type,
        where_col.ty.expected_ts(&schema.enums),
        "UPDATE WHERE: $2 ts_type"
    );
    assert!(
        q.params[1].nullable,
        "UPDATE WHERE: $2 should stay nullable"
    );
    assert!(
        q.params[1].table_ref.is_none(),
        "UPDATE WHERE: $2 should have no table_ref"
    );
}

async fn check_left_join(schema: &GenSchema, lhs: &GenTable, rhs: &GenTable) {
    // Use first column of each table (always exists) as the join key.
    let lhs_key = &lhs.columns[0];
    let rhs_key = &rhs.columns[0];
    // The result selects one rhs NOT-NULL column — if the rhs has one;
    // otherwise skip (we only assert the widening property when there's
    // actually a NOT NULL column to widen).
    let Some(rhs_nn) = rhs.columns.iter().find(|c| c.not_null) else {
        return;
    };
    let an = analyzer().await;
    let sql = format!(
        "SELECT b.{rhs_col} FROM {ns}.{lhs} a LEFT JOIN {ns}.{rhs} b \
         ON a.{lhs_key} = b.{rhs_key}",
        ns = schema.namespace,
        lhs = lhs.name,
        rhs = rhs.name,
        lhs_key = lhs_key.name,
        rhs_key = rhs_key.name,
        rhs_col = rhs_nn.name,
    );
    let q = match an.analyze(&sql).await {
        Ok(q) => q,
        // LEFT JOIN can fail if the join keys' types don't match.
        // proptest may generate that; just skip.
        Err(_) => return,
    };
    assert_eq!(q.columns.len(), 1);
    assert!(
        q.columns[0].nullable,
        "LEFT JOIN: rhs.{} was NOT NULL in base table but should be nullable in the result",
        rhs_nn.name
    );
}

async fn check_function_call(schema: &GenSchema, f: &GenFunction) {
    let an = analyzer().await;
    let sql = format!("SELECT {}.{}($1)", schema.namespace, f.name);
    let q = an
        .analyze(&sql)
        .await
        .unwrap_or_else(|e| panic!("function call: {e}\nSQL: {sql}"));
    assert_eq!(q.params.len(), 1, "function takes one arg");
    // Param type should match the function's arg type.
    let expected_arg = f.arg_type.expected_ts(&schema.enums);
    assert_eq!(
        q.params[0].ts_type, expected_arg,
        "function call param ts_type — expected `{expected_arg}` for {:?}",
        f.arg_type
    );
    // Param is not bound to any column (it's a function arg), so no table_ref.
    assert!(
        q.params[0].table_ref.is_none(),
        "function call param shouldn't have a table_ref"
    );
    // Result type should match the function's return type.
    assert_eq!(q.columns.len(), 1);
    let expected_ret = f.return_type.expected_ts(&schema.enums);
    assert_eq!(
        q.columns[0].ts_type, expected_ret,
        "function call result ts_type — expected `{expected_ret}` for {:?}",
        f.return_type
    );
    // Result column has no table_ref (computed).
    assert!(
        q.columns[0].table_ref.is_none(),
        "function call result shouldn't have a table_ref"
    );
}

// ---------- proptest driver ----------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The big one: random schema → canonical queries → property
    /// assertions all the way through. 32 cases keeps the suite under
    /// ~10s on dev PG; bump for more thorough fuzzing.
    #[test]
    fn schema_grounded_canonical_queries(schema in schema_strategy()) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let case_id = COUNTER.fetch_add(1, Ordering::SeqCst);
        rt().block_on(run_one(schema, case_id));
    }
}
