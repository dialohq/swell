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
    std::env::var("DATABASE_URL").expect(
        "sqlc-corpus tests require DATABASE_URL pointing at a dev Postgres",
    )
}

/// Sqlc's `-- name: Foo :many` block separator. Returns one (name, sql)
/// per block; the leading marker line is stripped, sqlc-specific
/// `sqlc.arg`/`sqlc.narg`/`sqlc.embed`/`@var`/`:foo`-style references
/// are NOT translated — tests using them won't analyze cleanly and
/// are skipped at the harness level.
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
            let name = rest.trim_start()
                .split(|c: char| c.is_whitespace() || c == '*')
                .next().unwrap_or("anon").to_string();
            cur_name = Some(name);
        } else if cur_name.is_some() {
            cur_body.push_str(line);
            cur_body.push('\n');
        }
    }
    flush(&mut out, &mut cur_name, &mut cur_body);
    out
}

/// True for queries we deliberately can't analyze without rewriting:
/// sqlc-specific magic (`sqlc.arg`, `sqlc.narg`, `sqlc.embed`,
/// `sqlc.slice`, named `@param` references). Real Postgres
/// `PREPARE` / PARSE only accepts `$N`, so we filter rather than fail.
fn skip_query(sql: &str) -> bool {
    if sql.contains("sqlc.arg(")
        || sql.contains("sqlc.narg(")
        || sql.contains("sqlc.embed(")
        || sql.contains("sqlc.slice(")
    {
        return true;
    }
    // Match any `@ident` token outside a string literal (sqlc's named-
    // parameter syntax). Conservative: any standalone `@<word>` triggers
    // the skip — false positives only delete tests we'd otherwise run.
    let bytes = sql.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'@' {
            // Skip email-style and array-operator usage (`x@y` or `@>` /
            // `@@`): the preceding byte must be whitespace or punctuation
            // and the following byte must be an identifier start.
            let prev_ok = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r' | b'(' | b',' | b':');
            let next_ok = bytes.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_');
            if prev_ok && next_ok {
                return true;
            }
        }
    }
    false
}

/// Connect once, create a per-test database, return an analyzer wired
/// to that DB. Drops the DB on completion via `TestDb::Drop`.
struct TestDb {
    db_name: String,
    root_url: String,
}

impl TestDb {
    async fn new(test_name: &str) -> Self {
        let root = root_url();
        let db_name = format!("swell_sqlc_{}", test_name.replace('-', "_"));
        let (client, conn) = tokio_postgres::connect(&root, NoTls)
            .await.expect("connect to root db");
        tokio::spawn(async move { let _ = conn.await; });
        // Reset any leftover DB from a previous run.
        let _ = client.execute(&format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"), &[]).await;
        client.execute(&format!("CREATE DATABASE \"{db_name}\""), &[])
            .await.expect("create test db");
        drop(client);
        Self { db_name, root_url: root }
    }

    fn url(&self) -> String {
        // Replace the dbname in the URL. Naive but enough for our shape
        // (`postgres://…/swell_test?host=…`).
        let url = &self.root_url;
        if let Some((before, _)) = url.split_once('/').and_then(|(scheme, rest)| {
            // Skip "postgres://" — locate the path component.
            let after_scheme = rest.strip_prefix('/').unwrap_or(rest);
            let host_and_db = after_scheme;
            let (host, after_db) = host_and_db.split_once('/')?;
            let (_db, query) = after_db.split_once('?').unwrap_or((after_db, ""));
            let new_path = format!("{scheme}//{host}/{}", self.db_name);
            let q = if query.is_empty() { String::new() } else { format!("?{query}") };
            Some((new_path, q))
        }) {
            let q = url.split('?').nth(1).unwrap_or("");
            let qs = if q.is_empty() { String::new() } else { format!("?{q}") };
            return format!("{before}{qs}");
        }
        // Fallback — rewrite manually.
        let q = url.split('?').nth(1).unwrap_or("");
        let qs = if q.is_empty() { String::new() } else { format!("?{q}") };
        // Strip everything after the host's `/dbname` segment.
        let head: String = url.chars()
            .scan(0u8, |slashes, c| {
                if c == '/' { *slashes += 1; }
                if *slashes >= 3 && c == '/' { return None; }
                Some(c)
            })
            .collect();
        format!("{head}/{}{qs}", self.db_name)
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        // Best-effort cleanup. tokio-postgres connections are async;
        // spawn a synchronous fire-and-forget on a thread-local runtime.
        let url = self.root_url.clone();
        let name = self.db_name.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            rt.block_on(async move {
                if let Ok((client, conn)) = tokio_postgres::connect(&url, NoTls).await {
                    tokio::spawn(async move { let _ = conn.await; });
                    let _ = client.execute(
                        &format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"),
                        &[],
                    ).await;
                }
            });
        });
    }
}

async fn run_case(name: &str, schema: &str, queries: &str) {
    let db = TestDb::new(name).await;
    let url = db.url();

    // Apply schema in a single round-trip (uses simple_query for
    // multi-statement support).
    let (client, conn) = tokio_postgres::connect(&url, NoTls)
        .await.unwrap_or_else(|e| panic!("[{name}] connect to test db ({url}): {e}"));
    tokio::spawn(async move { let _ = conn.await; });
    client.simple_query(schema)
        .await.unwrap_or_else(|e| panic!("[{name}] apply schema: {e}"));
    drop(client);

    let an = Analyzer::connect(AnalyzerOptions {
        database_url: url,
        schemas: vec!["public".into()],
        type_overrides: BTreeMap::new(),
    }).await.unwrap_or_else(|e| panic!("[{name}] analyzer connect: {e}"));

    let queries = split_named_queries(queries);
    assert!(!queries.is_empty(), "[{name}] no `-- name:` blocks found in query.sql");

    let mut analyzed = 0;
    let mut skipped = 0;
    for (qname, sql) in &queries {
        if skip_query(sql) {
            skipped += 1;
            continue;
        }
        match an.analyze(sql).await {
            Ok(_) => { analyzed += 1; }
            Err(e) => panic!("[{name}/{qname}] analyzer failed:\n  SQL: {sql}\n  Err: {e:#}"),
        }
    }
    assert!(
        analyzed + skipped == queries.len(),
        "[{name}] analyzed + skipped != total ({analyzed} + {skipped} vs {})",
        queries.len(),
    );
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
            ).await;
        }
    };
}

sqlc_case!(sqlc_accurate_cte, "accurate_cte");
sqlc_case!(sqlc_accurate_enum, "accurate_enum");
sqlc_case!(sqlc_accurate_star_expansion, "accurate_star_expansion");
sqlc_case!(sqlc_alias, "alias");
sqlc_case!(sqlc_batch, "batch");
sqlc_case!(sqlc_builtins, "builtins");
sqlc_case!(sqlc_case_named_params, "case_named_params");
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
sqlc_case!(sqlc_cte_recursive_employees, "cte_recursive_employees");
sqlc_case!(sqlc_cte_recursive_star, "cte_recursive_star");
sqlc_case!(sqlc_cte_recursive_subquery, "cte_recursive_subquery");
sqlc_case!(sqlc_cte_recursive_union, "cte_recursive_union");
sqlc_case!(sqlc_cte_select_one, "cte_select_one");
sqlc_case!(sqlc_cte_update, "cte_update");
sqlc_case!(sqlc_cte_update_multiple, "cte_update_multiple");
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
sqlc_case!(sqlc_insert_select_case, "insert_select_case");
sqlc_case!(sqlc_insert_select_param, "insert_select_param");
sqlc_case!(sqlc_insert_values, "insert_values");
sqlc_case!(sqlc_insert_values_only, "insert_values_only");
sqlc_case!(sqlc_insert_values_public, "insert_values_public");
sqlc_case!(sqlc_join_alias, "join_alias");
sqlc_case!(sqlc_join_clauses_order, "join_clauses_order");
sqlc_case!(sqlc_join_from, "join_from");
sqlc_case!(sqlc_join_full, "join_full");
sqlc_case!(sqlc_join_group_by_alias, "join_group_by_alias");
sqlc_case!(sqlc_join_inner, "join_inner");
sqlc_case!(sqlc_join_left, "join_left");
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
sqlc_case!(sqlc_json_array_elements, "json_array_elements");
sqlc_case!(sqlc_json_build, "json_build");
sqlc_case!(sqlc_json_param_type, "json_param_type");
sqlc_case!(sqlc_min_max_date, "min_max_date");
sqlc_case!(sqlc_nested_select, "nested_select");
sqlc_case!(sqlc_nextval, "nextval");
sqlc_case!(sqlc_null_if_type, "null_if_type");
sqlc_case!(sqlc_on_duplicate_key_update, "on_duplicate_key_update");
sqlc_case!(sqlc_operator_string_concat, "operator_string_concat");
sqlc_case!(sqlc_order_by_binds, "order_by_binds");
sqlc_case!(sqlc_order_by_union, "order_by_union");
sqlc_case!(sqlc_params_duplicate, "params_duplicate");
sqlc_case!(sqlc_params_in_nested_func, "params_in_nested_func");
sqlc_case!(sqlc_params_location, "params_location");
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
sqlc_case!(sqlc_subquery_calculated_column, "subquery_calculated_column");
sqlc_case!(sqlc_sum_type, "sum_type");
sqlc_case!(sqlc_table_function, "table_function");
sqlc_case!(sqlc_truncate, "truncate");
sqlc_case!(sqlc_types_uuid, "types_uuid");
sqlc_case!(sqlc_unnest, "unnest");
sqlc_case!(sqlc_unnest_star, "unnest_star");
sqlc_case!(sqlc_unnest_with_ordinality, "unnest_with_ordinality");
sqlc_case!(sqlc_update_array_index, "update_array_index");
sqlc_case!(sqlc_update_join, "update_join");
sqlc_case!(sqlc_update_set, "update_set");
sqlc_case!(sqlc_update_set_multiple, "update_set_multiple");
sqlc_case!(sqlc_valid_group_by_reference, "valid_group_by_reference");
