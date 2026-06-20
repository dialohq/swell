//! Port of sqlc's end-to-end test corpus to swell's analyzer.
//!
//! For each test case we apply the upstream `schema.sql`, split
//! `query.sql` on sqlc's `-- name: Foo :many` markers, then run
//! `Analyzer::analyze` on each query. The smoke test asserts that
//! swell can describe + type every query that sqlc handles — any
//! analyzer panic / error here is a gap to fix.
//!
//! Source: github.com/sqlc-dev/sqlc/internal/endtoend/testdata/<name>/
//! postgresql/pgx/v5/{schema,query}.sql. Files copied verbatim under
//! `tests/sqlc/<name>/`.
//!
//! Isolation: each test creates `CREATE DATABASE swell_sqlc_<name>`,
//! applies schema, runs analyzer, then drops the DB. Parallel-safe
//! by database name; serialise on the same dev PG.

use std::collections::BTreeMap;
use swell_analyzer::{Analyzer, AnalyzerOptions};
use tokio_postgres::NoTls;

fn root_url() -> String {
    std::env::var("DATABASE_URL")
        .expect("sqlc-corpus tests require DATABASE_URL pointing at a dev Postgres")
}

/// Sqlc's `-- name: Foo :many` block separator. Returns one (name, sql)
/// per block; the leading marker line is stripped. The query body is
/// passed to swell verbatim — if it contains sqlc-specific magic
/// (`sqlc.arg`, `sqlc.narg`, `@named`, etc.) the analyzer will fail
/// loudly. Fixtures that can't be rewritten to vanilla Postgres
/// belong in upstream sqlc's lint suite, not here.
fn split_named_queries(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut cur_body: String = String::new();
    let flush = |out: &mut Vec<(String, String)>, name: &mut Option<String>, body: &mut String| {
        if let Some(n) = name.take() {
            let sql = body.trim().to_string();
            if !sql.is_empty() {
                out.push((n, sql));
            }
            body.clear();
        }
    };
    for line in text.lines() {
        let trimmed = line.trim_start();
        // Sqlc accepts either `-- name: Foo :many` (line comment) or
        // `/* name: Foo :many */` (block comment) as the per-query
        // header. The body is everything up to the next header.
        let header = trimmed
            .strip_prefix("-- name:")
            .or_else(|| trimmed.strip_prefix("/* name:"));
        if let Some(rest) = header {
            flush(&mut out, &mut cur_name, &mut cur_body);
            let name = rest
                .trim_start()
                .split(|c: char| c.is_whitespace() || c == '*')
                .next()
                .unwrap_or("anon")
                .to_string();
            cur_name = Some(name);
        } else if cur_name.is_some() {
            cur_body.push_str(line);
            cur_body.push('\n');
        }
    }
    flush(&mut out, &mut cur_name, &mut cur_body);
    out
}

struct TestDb {
    url: String,
}

impl TestDb {
    async fn new(test_name: &str) -> Self {
        let root = root_url();
        let db_name = format!("swell_sqlc_{}", test_name.replace('-', "_"));
        let (client, conn) = tokio_postgres::connect(&root, NoTls)
            .await
            .expect("connect to root db");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        // DROP IF EXISTS so a previous aborted run can't block this one.
        let _ = client
            .execute(
                &format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"),
                &[],
            )
            .await;
        client
            .execute(&format!("CREATE DATABASE \"{db_name}\""), &[])
            .await
            .expect("create test db");
        drop(client);
        // Swap the dbname segment in the URL. The query string (`?host=…`)
        // sits after the path so we preserve it verbatim.
        let (path, query) = match root.split_once('?') {
            Some((p, q)) => (p, format!("?{q}")),
            None => (root.as_str(), String::new()),
        };
        let base = path.rsplit_once('/').map(|(b, _)| b).unwrap_or(path);
        Self {
            url: format!("{base}/{db_name}{query}"),
        }
    }

    fn url(&self) -> &str {
        &self.url
    }
}
// No Drop impl: leaving the DB behind is cheap (file-system reused on the
// next run's DROP IF EXISTS), and the alternative — spawning a fresh
// tokio runtime per test for one `DROP DATABASE` — was the largest
// fixed cost in the suite.

async fn run_case(name: &str, schema: &str, queries: &str) {
    let db = TestDb::new(name).await;

    // Apply schema in a single round-trip (uses simple_query for
    // multi-statement support).
    let (client, conn) = tokio_postgres::connect(db.url(), NoTls)
        .await
        .unwrap_or_else(|e| panic!("[{name}] connect to test db ({}): {e}", db.url()));
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .simple_query(schema)
        .await
        .unwrap_or_else(|e| panic!("[{name}] apply schema: {e}"));
    drop(client);

    let an = Analyzer::connect(AnalyzerOptions {
        database_url: db.url().to_string(),
        schemas: vec!["public".into()],
        type_overrides: BTreeMap::new(),
    })
    .await
    .unwrap_or_else(|e| panic!("[{name}] analyzer connect: {e}"));

    let queries = split_named_queries(queries);
    assert!(
        !queries.is_empty(),
        "[{name}] no `-- name:` blocks found in query.sql"
    );

    for (qname, sql) in &queries {
        match an.analyze(sql).await {
            Ok(q) => {
                // Sanity: every described column has a name. Catches
                // a future regression where analyzer returns Ok with a
                // stub / empty InferredQuery.
                for col in &q.columns {
                    assert!(
                        !col.name.is_empty(),
                        "[{name}/{qname}] analyzer returned a column with no name"
                    );
                }
            }
            Err(e) => panic!("[{name}/{qname}] analyzer failed:\n  SQL: {sql}\n  Err: {e:#}"),
        }
    }
}

// Generate one #[test] per ported sqlc directory. Each case is named
// `sqlc_<dir>` so test-runner output reads `sqlc_corpus::sqlc_alias`.
macro_rules! sqlc_case {
    ($name:ident, $dir:literal) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            run_case(
                $dir,
                include_str!(concat!("sqlc/", $dir, "/schema.sql")),
                include_str!(concat!("sqlc/", $dir, "/query.sql")),
            )
            .await;
        }
    };
}

sqlc_case!(sqlc_accurate_cte, "accurate_cte");
sqlc_case!(sqlc_accurate_enum, "accurate_enum");
sqlc_case!(sqlc_accurate_star_expansion, "accurate_star_expansion");
sqlc_case!(sqlc_alias, "alias");
sqlc_case!(sqlc_batch, "batch");
sqlc_case!(sqlc_builtins, "builtins");
sqlc_case!(sqlc_coalesce, "coalesce");
sqlc_case!(sqlc_coalesce_as, "coalesce_as");
sqlc_case!(sqlc_coalesce_join, "coalesce_join");
sqlc_case!(sqlc_column_as, "column_as");
sqlc_case!(sqlc_comparisons, "comparisons");
sqlc_case!(sqlc_count_star, "count_star");
sqlc_case!(sqlc_create_table_as, "create_table_as");
sqlc_case!(sqlc_cte_join_self, "cte_join_self");
sqlc_case!(sqlc_cte_left_join, "cte_left_join");
sqlc_case!(sqlc_cte_multiple_alias, "cte_multiple_alias");
sqlc_case!(sqlc_cte_nested_with, "cte_nested_with");
sqlc_case!(sqlc_cte_recursive_star, "cte_recursive_star");
sqlc_case!(sqlc_cte_recursive_subquery, "cte_recursive_subquery");
sqlc_case!(sqlc_cte_select_one, "cte_select_one");
sqlc_case!(sqlc_cte_with_in, "cte_with_in");
sqlc_case!(sqlc_data_type_boolean, "data_type_boolean");
sqlc_case!(sqlc_delete_from, "delete_from");
sqlc_case!(sqlc_delete_using, "delete_using");
sqlc_case!(sqlc_do, "do");
sqlc_case!(sqlc_enum, "enum");
sqlc_case!(sqlc_enum_ordering, "enum_ordering");
sqlc_case!(sqlc_exec_no_return_struct, "exec_no_return_struct");
sqlc_case!(sqlc_func_aggregate, "func_aggregate");
sqlc_case!(sqlc_func_call_cast, "func_call_cast");
sqlc_case!(sqlc_func_match_types, "func_match_types");
sqlc_case!(sqlc_func_return_date, "func_return_date");
sqlc_case!(sqlc_func_return_record, "func_return_record");
sqlc_case!(sqlc_func_return_series, "func_return_series");
sqlc_case!(sqlc_func_return_table, "func_return_table");
sqlc_case!(sqlc_func_return_table_columns, "func_return_table_columns");
sqlc_case!(sqlc_func_star_expansion, "func_star_expansion");
sqlc_case!(sqlc_func_variadic, "func_variadic");
sqlc_case!(sqlc_having, "having");
sqlc_case!(sqlc_insert_select, "insert_select");
sqlc_case!(sqlc_insert_values, "insert_values");
sqlc_case!(sqlc_insert_values_only, "insert_values_only");
sqlc_case!(sqlc_insert_values_public, "insert_values_public");
sqlc_case!(sqlc_join_alias, "join_alias");
sqlc_case!(sqlc_join_clauses_order, "join_clauses_order");
sqlc_case!(sqlc_join_from, "join_from");
sqlc_case!(sqlc_join_full, "join_full");
sqlc_case!(sqlc_join_group_by_alias, "join_group_by_alias");
sqlc_case!(sqlc_join_inner, "join_inner");
sqlc_case!(sqlc_join_left_table_alias, "join_left_table_alias");
sqlc_case!(sqlc_join_order_by, "join_order_by");
sqlc_case!(sqlc_join_order_by_alias, "join_order_by_alias");
sqlc_case!(sqlc_join_right, "join_right");
sqlc_case!(sqlc_join_table_name, "join_table_name");
sqlc_case!(sqlc_join_two_tables, "join_two_tables");
sqlc_case!(sqlc_join_update, "join_update");
sqlc_case!(sqlc_join_using, "join_using");
sqlc_case!(sqlc_join_where_clause, "join_where_clause");
sqlc_case!(sqlc_json, "json");
sqlc_case!(sqlc_json_build, "json_build");
sqlc_case!(sqlc_json_param_type, "json_param_type");
sqlc_case!(sqlc_min_max_date, "min_max_date");
sqlc_case!(sqlc_nextval, "nextval");
sqlc_case!(sqlc_null_if_type, "null_if_type");
sqlc_case!(sqlc_order_by_union, "order_by_union");
sqlc_case!(sqlc_params_two, "params_two");
sqlc_case!(sqlc_pattern_matching, "pattern_matching");
sqlc_case!(sqlc_pg_advisory_xact_lock, "pg_advisory_xact_lock");
sqlc_case!(sqlc_pg_ext_ltree, "pg_ext_ltree");
sqlc_case!(sqlc_pg_generate_series, "pg_generate_series");
sqlc_case!(sqlc_pg_user_table, "pg_user_table");
sqlc_case!(sqlc_returning, "returning");
sqlc_case!(sqlc_schema_scoped_create, "schema_scoped_create");
sqlc_case!(sqlc_schema_scoped_delete, "schema_scoped_delete");
sqlc_case!(sqlc_schema_scoped_filter, "schema_scoped_filter");
sqlc_case!(sqlc_schema_scoped_list, "schema_scoped_list");
sqlc_case!(sqlc_schema_scoped_update, "schema_scoped_update");
sqlc_case!(sqlc_schema_table_column_ref, "schema_table_column_ref");
sqlc_case!(sqlc_select_column_cast, "select_column_cast");
sqlc_case!(sqlc_select_empty_column_list, "select_empty_column_list");
sqlc_case!(sqlc_select_limit, "select_limit");
sqlc_case!(sqlc_select_nested_count, "select_nested_count");
sqlc_case!(sqlc_select_sequence, "select_sequence");
sqlc_case!(sqlc_select_star, "select_star");
sqlc_case!(sqlc_select_star_quoted, "select_star_quoted");
sqlc_case!(sqlc_select_subquery, "select_subquery");
sqlc_case!(sqlc_select_subquery_alias, "select_subquery_alias");
sqlc_case!(sqlc_select_union_subquery, "select_union_subquery");
sqlc_case!(sqlc_single_param_conflict, "single_param_conflict");
sqlc_case!(sqlc_sql_syntax_calling_funcs, "sql_syntax_calling_funcs");
sqlc_case!(sqlc_star_expansion, "star_expansion");
sqlc_case!(sqlc_star_expansion_join, "star_expansion_join");
sqlc_case!(sqlc_star_expansion_reserved, "star_expansion_reserved");
sqlc_case!(sqlc_star_expansion_series, "star_expansion_series");
sqlc_case!(sqlc_star_expansion_subquery, "star_expansion_subquery");
sqlc_case!(
    sqlc_subquery_calculated_column,
    "subquery_calculated_column"
);
sqlc_case!(sqlc_sum_type, "sum_type");
sqlc_case!(sqlc_truncate, "truncate");
sqlc_case!(sqlc_types_uuid, "types_uuid");
sqlc_case!(sqlc_unnest_with_ordinality, "unnest_with_ordinality");
sqlc_case!(sqlc_update_array_index, "update_array_index");
sqlc_case!(sqlc_update_join, "update_join");
sqlc_case!(sqlc_update_set, "update_set");
sqlc_case!(sqlc_update_set_multiple, "update_set_multiple");
sqlc_case!(sqlc_valid_group_by_reference, "valid_group_by_reference");

// ----- Spot-check assertions on selected sqlc cases -----
//
// The smoke tests above prove swell *handles* every sqlc fixture; the
// blocks below pick a handful and assert the exact inferred shape
// matches sqlc's expected row type. Goal: catch regressions where the
// analyzer keeps describing the query but the inferred ts_type drifts.

async fn analyze_named(
    name: &str,
    schema: &str,
    queries: &str,
    qname: &str,
) -> swell_analyzer::InferredQuery {
    let db = TestDb::new(&format!("{name}_assert")).await;
    let (client, conn) = tokio_postgres::connect(db.url(), NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client.simple_query(schema).await.unwrap();
    drop(client);
    let an = Analyzer::connect(AnalyzerOptions {
        database_url: db.url().to_string(),
        schemas: vec!["public".into()],
        type_overrides: BTreeMap::new(),
    })
    .await
    .unwrap();
    let qs = split_named_queries(queries);
    let (_, sql) = qs
        .into_iter()
        .find(|(n, _)| n == qname)
        .unwrap_or_else(|| panic!("query `{qname}` not found in {name}"));
    an.analyze(&sql)
        .await
        .unwrap_or_else(|e| panic!("[{name}/{qname}] {e:#}"))
}

#[tokio::test(flavor = "current_thread")]
async fn assert_coalesce_alias_propagates() {
    // `coalesce(bar, '') as login` — assert the alias survives, the
    // ts_type is `string`, and table_ref is unset (coalesce is a
    // computed expression, not a base-column ref). Nullability under
    // a FROM-only (no WHERE) plan is left to swell's EXPLAIN heuristic;
    // see integration.rs::coalesce_with_literal_is_not_nullable for the
    // tighter case with a WHERE clause.
    let q = analyze_named(
        "coalesce",
        include_str!("sqlc/coalesce/schema.sql"),
        include_str!("sqlc/coalesce/query.sql"),
        "CoalesceString",
    )
    .await;
    assert_eq!(q.columns.len(), 1);
    assert_eq!(q.columns[0].name, "login");
    assert_eq!(q.columns[0].ts_type, "string");
    assert!(
        q.columns[0].table_ref.is_none(),
        "coalesce(...) is a computed expression, not a base-column ref"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn assert_count_star_is_bigint_not_null() {
    let q = analyze_named(
        "count_star",
        include_str!("sqlc/count_star/schema.sql"),
        include_str!("sqlc/count_star/query.sql"),
        "CountStarLower",
    )
    .await;
    assert!(
        q.columns[0].ts_type.contains("string"),
        "count(*) is bigint → string in node-pg, got {}",
        q.columns[0].ts_type
    );
    assert!(!q.columns[0].nullable, "count(*) is never null");
}

#[tokio::test(flavor = "current_thread")]
async fn assert_returning_id_carries_table_ref() {
    let q = analyze_named(
        "returning",
        include_str!("sqlc/returning/schema.sql"),
        include_str!("sqlc/returning/query.sql"),
        "InsertUserAndReturnID",
    )
    .await;
    assert_eq!(q.columns.len(), 1);
    let r = q.columns[0]
        .table_ref
        .as_ref()
        .expect("RETURNING id should carry table_ref back to users");
    assert_eq!(r.table, "users");
    assert_eq!(r.column, "id");
}
