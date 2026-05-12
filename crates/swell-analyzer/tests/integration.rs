//! Integration tests that hit a real Postgres.
//!
//! Require `DATABASE_URL` to be set. If it isn't, the tests fail loudly
//! rather than passing silently — the analyzer's whole job is to talk to a
//! live database, so an unconfigured DB is a CI misconfiguration, not a
//! reason to call the test green.

use swell_analyzer::{Analyzer, AnalyzerOptions};
use std::collections::BTreeMap;

fn database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "swell-analyzer integration tests require DATABASE_URL — \
         point it at a dev Postgres (the Nix dev shell + scripts/dev-pg.sh \
         do this for local dev; CI uses the postgres service container)",
    )
}

fn opts(url: String) -> AnalyzerOptions {
    AnalyzerOptions {
        database_url: url,
        schemas: vec!["public".into()],
        type_overrides: BTreeMap::new(),
    }
}

async fn fresh_db() -> Analyzer {
    Analyzer::connect(opts(database_url())).await.expect("connect")
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_select_with_param() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT id, email FROM users WHERE id = $1").await.expect("analyze");

    assert_eq!(q.params.len(), 1);
    assert_eq!(q.params[0].ts_type, "string"); // uuid -> string

    assert_eq!(q.columns.len(), 2);
    assert_eq!(q.columns[0].name, "id");
    assert_eq!(q.columns[0].ts_type, "string");
    assert!(!q.columns[0].nullable, "users.id is NOT NULL");
    assert_eq!(q.columns[1].name, "email");
    assert_eq!(q.columns[1].ts_type, "string");
    assert!(!q.columns[1].nullable, "users.email is NOT NULL");
}

#[tokio::test(flavor = "current_thread")]
async fn nullable_base_column() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT display_name FROM users WHERE id = $1").await.expect("analyze");
    assert_eq!(q.columns.len(), 1);
    assert_eq!(q.columns[0].name, "display_name");
    assert!(q.columns[0].nullable, "users.display_name is nullable");
    assert_eq!(q.columns[0].ts_type, "string");
}

#[tokio::test(flavor = "current_thread")]
async fn enum_column() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT role FROM users WHERE id = $1").await.expect("analyze");
    assert_eq!(q.columns.len(), 1);
    let ts = &q.columns[0].ts_type;
    // Enum labels: 'admin' | 'member' (order: enumsortorder)
    assert!(ts.contains("\"admin\""), "got {ts}");
    assert!(ts.contains("\"member\""), "got {ts}");
}

#[tokio::test(flavor = "current_thread")]
async fn count_aggregate_is_bigint() {
    // M3 will refine count(*) → non-null; M1 just verifies the type mapping.
    let an = fresh_db().await;
    let q = an.analyze("SELECT count(*) AS n FROM users").await.expect("analyze");
    assert_eq!(q.columns.len(), 1);
    assert_eq!(q.columns[0].ts_type, "string"); // bigint → string in postgres.js
}

#[tokio::test(flavor = "current_thread")]
async fn timestamp_and_uuid_arrays() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT ARRAY[gen_random_uuid()] AS u, NOW() AS t").await.expect("analyze");
    let names: Vec<&str> = q.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["u", "t"]);
    assert_eq!(q.columns[0].ts_type, "string[]");
    assert_eq!(q.columns[1].ts_type, "Date");
}

// -------- M3: EXPLAIN-driven nullability refinement --------

#[tokio::test(flavor = "current_thread")]
async fn count_star_is_not_nullable() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT count(*) AS n FROM users").await.expect("analyze");
    assert_eq!(q.columns.len(), 1);
    assert!(!q.columns[0].nullable, "count(*) is never null");
}

#[tokio::test(flavor = "current_thread")]
async fn sum_is_nullable() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT sum(1) AS s FROM users").await.expect("analyze");
    assert!(q.columns[0].nullable, "sum() is null on empty input");
}

#[tokio::test(flavor = "current_thread")]
async fn coalesce_with_literal_is_not_nullable() {
    let an = fresh_db().await;
    let q = an.analyze(
        "SELECT coalesce(display_name, 'unknown') AS label FROM users WHERE id = $1"
    ).await.expect("analyze");
    assert!(!q.columns[0].nullable, "coalesce(x, literal) is not nullable");
}

#[tokio::test(flavor = "current_thread")]
async fn left_join_makes_rhs_nullable() {
    let an = fresh_db().await;
    let q = an.analyze(r#"
        SELECT u.email, p.body
        FROM users u
        LEFT JOIN posts p ON p.author_id = u.id
        WHERE u.id = $1
    "#).await.expect("analyze");
    let by_name: std::collections::HashMap<&str, &swell_analyzer::InferredColumn> =
        q.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    assert!(!by_name["email"].nullable, "u.email is NOT NULL on the preserved side");
    assert!(by_name["body"].nullable, "p.body is on the LEFT JOIN nullable side");
}

#[tokio::test(flavor = "current_thread")]
async fn inner_join_preserves_not_null() {
    let an = fresh_db().await;
    let q = an.analyze(r#"
        SELECT u.email, o.name
        FROM users u
        JOIN orgs o ON o.id = u.org_id
        WHERE u.id = $1
    "#).await.expect("analyze");
    let by_name: std::collections::HashMap<&str, &swell_analyzer::InferredColumn> =
        q.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    assert!(!by_name["email"].nullable, "INNER JOIN preserves NOT NULL on both sides");
    assert!(!by_name["name"].nullable, "INNER JOIN preserves NOT NULL on both sides");
}

#[tokio::test(flavor = "current_thread")]
async fn jsonb_column_is_unknown_until_m7() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT settings FROM users WHERE id = $1").await.expect("analyze");
    assert_eq!(q.columns[0].ts_type, "unknown");
    assert!(!q.columns[0].nullable, "users.settings is NOT NULL in fixture");
}

// -------- M4: SQLx-style alias overrides --------

#[tokio::test(flavor = "current_thread")]
async fn override_force_not_null() {
    let an = fresh_db().await;
    // display_name is normally nullable; force NOT NULL via `!`.
    let q = an.analyze(
        r#"SELECT coalesce(display_name, email) AS "label!" FROM users WHERE id = $1"#,
    ).await.expect("analyze");
    assert_eq!(q.columns.len(), 1);
    assert_eq!(q.columns[0].name, "label");
    assert!(!q.columns[0].nullable, "force-NOT-NULL via !");
}

#[tokio::test(flavor = "current_thread")]
async fn override_force_nullable() {
    let an = fresh_db().await;
    let q = an.analyze(
        r#"SELECT email AS "email_maybe?" FROM users WHERE id = $1"#,
    ).await.expect("analyze");
    assert_eq!(q.columns[0].name, "email_maybe");
    assert!(q.columns[0].nullable, "force-nullable via ?");
}

#[tokio::test(flavor = "current_thread")]
async fn override_type() {
    let an = fresh_db().await;
    let q = an.analyze(
        r#"SELECT settings AS "settings: UserSettings" FROM users WHERE id = $1"#,
    ).await.expect("analyze");
    assert_eq!(q.columns[0].name, "settings");
    assert_eq!(q.columns[0].ts_type, "UserSettings");
}

#[tokio::test(flavor = "current_thread")]
async fn override_type_and_not_null() {
    let an = fresh_db().await;
    let q = an.analyze(
        r#"SELECT settings AS "settings!: UserSettings" FROM users WHERE id = $1"#,
    ).await.expect("analyze");
    assert_eq!(q.columns[0].name, "settings");
    assert_eq!(q.columns[0].ts_type, "UserSettings");
    assert!(!q.columns[0].nullable);
}

// -------- M7: JSON shape inference --------

#[tokio::test(flavor = "current_thread")]
async fn jsonb_build_object_simple() {
    let an = fresh_db().await;
    let q = an.analyze(r#"
        SELECT jsonb_build_object(
            'id', u.id,
            'email', u.email,
            'name', u.display_name
        ) AS profile
        FROM users u WHERE u.id = $1
    "#).await.expect("analyze");
    assert_eq!(q.columns.len(), 1);
    let ts = &q.columns[0].ts_type;
    assert!(ts.contains("id: string"), "got {ts}");
    assert!(ts.contains("email: string"), "got {ts}");
    assert!(ts.contains("name: string | null"), "got {ts}");
}

#[tokio::test(flavor = "current_thread")]
async fn jsonb_agg_with_jsonb_build_object() {
    let an = fresh_db().await;
    let q = an.analyze(r#"
        SELECT o.name,
               jsonb_agg(jsonb_build_object('id', u.id, 'email', u.email)) AS members
        FROM orgs o JOIN users u ON u.org_id = o.id
        WHERE o.id = $1
        GROUP BY o.id, o.name
    "#).await.expect("analyze");
    let by_name: std::collections::HashMap<&str, &swell_analyzer::InferredColumn> =
        q.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let members_ts = &by_name["members"].ts_type;
    assert!(members_ts.contains("id: string"), "got {members_ts}");
    assert!(members_ts.contains("email: string"), "got {members_ts}");
    assert!(members_ts.ends_with("[]"), "expected array, got {members_ts}");
}

#[tokio::test(flavor = "current_thread")]
async fn json_build_object_nested() {
    let an = fresh_db().await;
    let q = an.analyze(r#"
        SELECT jsonb_build_object(
            'user', jsonb_build_object('id', u.id, 'role', u.role),
            'meta', jsonb_build_object('email', u.email)
        ) AS payload
        FROM users u WHERE u.id = $1
    "#).await.expect("analyze");
    let ts = &q.columns[0].ts_type;
    assert!(ts.contains("user: {"), "got {ts}");
    assert!(ts.contains("\"admin\""), "expected enum, got {ts}");
    assert!(ts.contains("meta: {"), "got {ts}");
}

#[tokio::test(flavor = "current_thread")]
async fn to_jsonb_table_alias_enumerates_columns() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT to_jsonb(o) AS row FROM orgs o WHERE o.id = $1").await.expect("analyze");
    let ts = &q.columns[0].ts_type;
    assert!(ts.contains("id: string"), "got {ts}");
    assert!(ts.contains("name: string"), "got {ts}");
}

// -------- Custom user-defined types --------

#[tokio::test(flavor = "current_thread")]
async fn custom_domain_renders_as_base_type() {
    // email_address is `DOMAIN email_address AS text`. PostgreSQL describes
    // it as the domain OID; we should walk typbasetype to text.
    let an = fresh_db().await;
    let q = an.analyze("SELECT email FROM users WHERE id = $1").await.expect("analyze");
    assert_eq!(q.columns[0].ts_type, "string");
    assert!(!q.columns[0].nullable, "users.email is NOT NULL");
}

#[tokio::test(flavor = "current_thread")]
async fn custom_enum_renders_as_string_union() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT role FROM users WHERE id = $1").await.expect("analyze");
    let ts = &q.columns[0].ts_type;
    assert!(ts.contains("\"admin\""), "got {ts}");
    assert!(ts.contains("\"member\""), "got {ts}");
}

#[tokio::test(flavor = "current_thread")]
async fn custom_composite_type_renders_as_object() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT home_address FROM users WHERE id = $1").await.expect("analyze");
    let ts = &q.columns[0].ts_type;
    // Composite fields nullable in TS because Postgres composite fields
    // are independently nullable (no NOT NULL on composite attributes).
    assert!(ts.contains("street:"), "got {ts}");
    assert!(ts.contains("city:"), "got {ts}");
    assert!(ts.contains("zip:"), "got {ts}");
    // home_address itself is nullable on the table.
    assert!(q.columns[0].nullable);
}

#[tokio::test(flavor = "current_thread")]
async fn jsonb_build_object_with_dynamic_key() {
    let an = fresh_db().await;
    let q = an.analyze(r#"
        SELECT jsonb_build_object(
            u.email, u.id,
            'static_key', u.role
        ) AS payload
        FROM users u WHERE u.id = $1
    "#).await.expect("analyze");
    let ts = &q.columns[0].ts_type;
    // Mixed dynamic + literal keys → Record<string, union>.
    assert!(ts.starts_with("Record<string,"), "got {ts}");
    assert!(ts.contains("\"admin\""), "value union should include enum, got {ts}");
    assert!(ts.contains("string"), "value union should include uuid string, got {ts}");
}

#[tokio::test(flavor = "current_thread")]
async fn enum_inside_jsonb_build_object() {
    let an = fresh_db().await;
    let q = an.analyze(r#"
        SELECT jsonb_build_object('role', u.role) AS payload
        FROM users u WHERE u.id = $1
    "#).await.expect("analyze");
    let ts = &q.columns[0].ts_type;
    assert!(ts.contains("\"admin\""), "enum should expand inside JSON shape, got {ts}");
}
