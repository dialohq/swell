use swell_scanner::{scan_file, ScanOptions};
use std::path::Path;

// The default test fixture treats `./db` as the source of `sql`. This mirrors
// the per-package layout: `db.ts` exports `sql = createSql(driver)`.
fn scan(src: &str) -> Vec<swell_scanner::ScannedQuery> {
    scan_file(
        Path::new("test.ts"),
        src,
        ScanOptions { db_modules: &["./db", "./swell.generated"], db_exports: &["sql"] },
    )
    .expect("scan ok")
}

#[test]
fn finds_simple_named_import() {
    let src = r#"
        import { sql } from "./db";
        async function f(id: string) {
          return await sql("SELECT id FROM users WHERE id = $1", id);
        }
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].static_parts[0], "SELECT id FROM users WHERE id = $1");
    assert_eq!(qs[0].tag_local_name, "sql");
}

#[test]
fn picks_up_aliased_import() {
    let src = r#"
        import { sql as q } from "./db";
        const x = q("SELECT 1");
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].static_parts[0], "SELECT 1");
    assert_eq!(qs[0].tag_local_name, "q");
}

#[test]
fn ignores_unrelated_calls_and_modules() {
    let src = r#"
        import { sql } from "other-pkg";
        sql("SELECT not-from-our-pkg");
        const css = (s: string) => s;
        css("color: red");
    "#;
    let qs = scan(src);
    assert!(qs.is_empty(), "got {:?}", qs);
}

#[test]
fn ignores_non_literal_first_arg() {
    let src = r#"
        import { sql } from "./db";
        const tbl = "users";
        sql("SELECT * FROM " + tbl);  // dynamic — should be skipped
        const x = "SELECT 1";
        sql(x);                         // also dynamic
    "#;
    let qs = scan(src);
    assert!(qs.is_empty(), "expected no static queries, got {:?}", qs);
}

#[test]
fn extracts_multiple_call_sites() {
    let src = r#"
        import { sql } from "./db";
        await sql("SELECT 1");
        await sql("SELECT 2 WHERE x=$1", y);
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 2);
    assert_eq!(qs[0].static_parts[0], "SELECT 1");
    assert_eq!(qs[1].static_parts[0], "SELECT 2 WHERE x=$1");
}

#[test]
fn line_col_reported() {
    let src = "import { sql } from \"./db\";\nawait sql(\"SELECT 1\");";
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].line, 2);
}

#[test]
fn template_literal_with_no_interpolation_works() {
    let src = r#"
        import { sql } from "./db";
        sql(`SELECT 42`);
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].static_parts[0], "SELECT 42");
}

#[test]
fn template_literal_with_interpolation_skipped() {
    let src = r#"
        import { sql } from "./db";
        const id = "x";
        sql(`SELECT ${id} FROM t`);
    "#;
    let qs = scan(src);
    assert!(qs.is_empty(), "interpolated template literal should be skipped, got {:?}", qs);
}

#[test]
fn handles_namespace_imports_silently() {
    let src = r#"
        import * as M from "./db";
        M.sql("SELECT 1");
    "#;
    let qs = scan(src);
    assert!(qs.is_empty());
}

#[test]
fn picks_up_create_sql_factory_binding() {
    // The per-package db.ts pattern: import { createSql } from the codegen
    // output, bind it locally with `const sql = createSql(...)`. The scanner
    // should then track every `sql.X(...)` call in the same file.
    let src = r#"
        import postgres from "postgres";
        import { createSql } from "./swell.generated";
        const sql = createSql(postgres());
        async function f(id: string) {
          return await sql.one("SELECT id FROM users WHERE id = $1", id);
        }
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].static_parts[0], "SELECT id FROM users WHERE id = $1");
}

#[test]
fn picks_up_extra_module_local_re_export() {
    // Per-package `db.ts` re-export pattern: each package binds `sql` to its
    // own connection and exports it. Call sites import from `./db`, not the
    // codegen output directly.
    let src = r#"
        import { sql } from "./db";
        async function f(id: string) {
          return await sql("SELECT id FROM users WHERE id = $1", id);
        }
    "#;
    let qs = scan_file(
        Path::new("test.ts"),
        src,
        ScanOptions { db_modules: &["./db", "../db", "../../db"], db_exports: &["sql"] },
    )
    .expect("scan ok");
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].static_parts[0], "SELECT id FROM users WHERE id = $1");
}
