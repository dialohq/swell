use std::path::Path;
use swell_scanner::{scan_file, ScanOptions};

fn scan(src: &str) -> Vec<swell_scanner::ScannedQuery> {
    scan_file(
        Path::new("test.ts"),
        src,
        ScanOptions {
            q_modules: &["./swell.generated"],
        },
    )
    .expect("scan ok")
}

#[test]
fn picks_up_q_from_swell() {
    let src = r#"
        import { q } from "@dialo/swell";
        const stmt = q("SELECT id FROM users WHERE id = $1");
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].static_parts[0], "SELECT id FROM users WHERE id = $1");
    assert_eq!(qs[0].tag_local_name, "q");
}

#[test]
fn picks_up_q_from_swell_generated() {
    let src = r#"
        import { q } from "./swell.generated";
        const stmt = q("SELECT 1");
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].static_parts[0], "SELECT 1");
}

#[test]
fn picks_up_q_aliased() {
    let src = r#"
        import { q as marker } from "@dialo/swell";
        const stmt = marker("SELECT 1");
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].static_parts[0], "SELECT 1");
    assert_eq!(qs[0].tag_local_name, "marker");
}

#[test]
fn ignores_unrelated_modules() {
    let src = r#"
        import { q } from "other-pkg";
        q("SELECT not-from-our-pkg");
    "#;
    let qs = scan(src);
    assert!(qs.is_empty(), "got {:?}", qs);
}

#[test]
fn ignores_non_literal_first_arg() {
    let src = r#"
        import { q } from "@dialo/swell";
        const tbl = "users";
        q("SELECT * FROM " + tbl);
        const x = "SELECT 1";
        q(x);
    "#;
    let qs = scan(src);
    assert!(qs.is_empty(), "expected no static queries, got {:?}", qs);
}

#[test]
fn extracts_multiple_call_sites() {
    let src = r#"
        import { q } from "@dialo/swell";
        await pool.query(q("SELECT 1"));
        await pool.query(q("SELECT 2 WHERE x=$1"), [y]);
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 2);
    assert_eq!(qs[0].static_parts[0], "SELECT 1");
    assert_eq!(qs[1].static_parts[0], "SELECT 2 WHERE x=$1");
}

#[test]
fn line_col_reported() {
    let src = "import { q } from \"@dialo/swell\";\nq(\"SELECT 1\");";
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].line, 2);
}

#[test]
fn template_literal_with_no_interpolation_works() {
    let src = r#"
        import { q } from "@dialo/swell";
        q(`SELECT 42`);
    "#;
    let qs = scan(src);
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].static_parts[0], "SELECT 42");
}

#[test]
fn template_literal_with_interpolation_skipped() {
    let src = r#"
        import { q } from "@dialo/swell";
        const id = "x";
        q(`SELECT ${id} FROM t`);
    "#;
    let qs = scan(src);
    assert!(
        qs.is_empty(),
        "interpolated template literal should be skipped, got {:?}",
        qs
    );
}

#[test]
fn handles_namespace_imports_silently() {
    let src = r#"
        import * as M from "@dialo/swell";
        M.q("SELECT 1");
    "#;
    let qs = scan(src);
    assert!(qs.is_empty());
}
