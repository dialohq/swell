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

// -------- CHECK constraint refinements (issue #22) --------
//
// Tier 1: literal unions. Tier 2: JSON object shapes. Tier 3 (column-
// level): discriminated unions. Each test creates a small fixture
// table, then analyzes a SELECT against it. The fixtures are dropped
// at the end so they don't pollute other tests.

async fn with_table<F>(an: &Analyzer, ddl: &str, drop_stmt: &str, body: F)
where
    F: std::future::Future<Output = ()>,
{
    an.client.batch_execute(ddl).await.expect("create fixture");
    body.await;
    an.client.batch_execute(drop_stmt).await.expect("drop fixture");
}

#[tokio::test(flavor = "current_thread")]
async fn check_literal_union_narrows_string_to_union() {
    let an = fresh_db().await;
    with_table(
        &an,
        "DROP TABLE IF EXISTS ck_color;
         CREATE TABLE ck_color (
             id int PRIMARY KEY,
             color text NOT NULL CHECK (color IN ('red','green','blue'))
         );",
        "DROP TABLE ck_color;",
        async {
            let q = an.analyze("SELECT color FROM ck_color WHERE id = $1")
                .await.expect("analyze");
            assert_eq!(q.columns[0].name, "color");
            assert_eq!(
                q.columns[0].ts_type,
                r#""red" | "green" | "blue""#,
                "CHECK IN should narrow `string` to a literal union",
            );
            assert!(!q.columns[0].nullable);
        },
    ).await;
}

#[tokio::test(flavor = "current_thread")]
async fn check_single_string_literal_narrows_to_one() {
    let an = fresh_db().await;
    with_table(
        &an,
        "DROP TABLE IF EXISTS ck_kind;
         CREATE TABLE ck_kind (
             id int PRIMARY KEY,
             kind text NOT NULL CHECK (kind = 'invoice')
         );",
        "DROP TABLE ck_kind;",
        async {
            let q = an.analyze("SELECT kind FROM ck_kind WHERE id = $1")
                .await.expect("analyze");
            assert_eq!(q.columns[0].ts_type, r#""invoice""#);
        },
    ).await;
}

#[tokio::test(flavor = "current_thread")]
async fn check_nullable_or_literal_set() {
    let an = fresh_db().await;
    with_table(
        &an,
        "DROP TABLE IF EXISTS ck_priority;
         CREATE TABLE ck_priority (
             id int PRIMARY KEY,
             priority text CHECK (priority IS NULL OR priority IN ('low','high'))
         );",
        "DROP TABLE ck_priority;",
        async {
            let q = an.analyze("SELECT priority FROM ck_priority WHERE id = $1")
                .await.expect("analyze");
            // Column-level refinement carries `| null` from the CHECK.
            // The column is also nullable on the table, so codegen would
            // append `| null` again — the analyzer only ensures the
            // *narrowed* ts_type already includes null.
            assert!(q.columns[0].ts_type.contains("\"low\""));
            assert!(q.columns[0].ts_type.contains("\"high\""));
            assert!(q.columns[0].ts_type.contains("null"));
        },
    ).await;
}

#[tokio::test(flavor = "current_thread")]
async fn check_jsonb_object_shape() {
    let an = fresh_db().await;
    with_table(
        &an,
        "DROP TABLE IF EXISTS ck_meta;
         CREATE TABLE ck_meta (
             id int PRIMARY KEY,
             meta jsonb NOT NULL CHECK (
                 jsonb_typeof(meta) = 'object'
                 AND meta ?& array['width','height']
                 AND jsonb_typeof(meta->'width') = 'number'
                 AND jsonb_typeof(meta->'height') = 'number'
             )
         );",
        "DROP TABLE ck_meta;",
        async {
            let q = an.analyze("SELECT meta FROM ck_meta WHERE id = $1")
                .await.expect("analyze");
            let ts = &q.columns[0].ts_type;
            assert!(ts.contains("width: number"), "got {ts}");
            assert!(ts.contains("height: number"), "got {ts}");
            assert!(ts.contains("Record<string, Json>"), "open object form expected: {ts}");
        },
    ).await;
}

#[tokio::test(flavor = "current_thread")]
async fn check_jsonb_discriminated_union() {
    let an = fresh_db().await;
    with_table(
        &an,
        "DROP TABLE IF EXISTS ck_payload;
         CREATE TABLE ck_payload (
             id int PRIMARY KEY,
             payload jsonb NOT NULL CHECK (
                  payload->>'kind' = 'text' AND jsonb_typeof(payload->'body') = 'string'
               OR payload->>'kind' = 'image' AND jsonb_typeof(payload->'url') = 'string'
                                             AND jsonb_typeof(payload->'alt') = 'string'
             )
         );",
        "DROP TABLE ck_payload;",
        async {
            let q = an.analyze("SELECT payload FROM ck_payload WHERE id = $1")
                .await.expect("analyze");
            let ts = &q.columns[0].ts_type;
            assert!(ts.contains("kind: \"text\""), "got {ts}");
            assert!(ts.contains("kind: \"image\""), "got {ts}");
            assert!(ts.contains("body: string"), "got {ts}");
            assert!(ts.contains("url: string"), "got {ts}");
            assert!(ts.contains(" | "), "expected union, got {ts}");
        },
    ).await;
}

#[tokio::test(flavor = "current_thread")]
async fn check_row_level_num_nonnulls() {
    // Tier 3 row-level: `num_nonnulls(email, phone) = 1` is reflected
    // as a row-variant union on the table schema.
    let an = fresh_db().await;
    an.client.batch_execute(
        "DROP TABLE IF EXISTS ck_contact;
         CREATE TABLE ck_contact (
             id int PRIMARY KEY,
             email text,
             phone text,
             CHECK (num_nonnulls(email, phone) = 1)
         );",
    ).await.unwrap();
    let schemas = an.table_schemas(&[("public".into(), "ck_contact".into())])
        .await.expect("table_schemas");
    let t = schemas.iter().find(|t| t.table == "ck_contact").expect("ck_contact");
    assert_eq!(t.row_variants.len(), 2, "expected 2 variants, got {:?}", t.row_variants);
    let any_has = |key: &str, val: &str|
        t.row_variants.iter().any(|v| v.columns.get(key).map(|s| s.as_str()) == Some(val));
    assert!(any_has("email", "string"), "no variant with email: string");
    assert!(any_has("phone", "null"), "no variant with phone: null");
    assert!(any_has("email", "null"), "no variant with email: null");
    assert!(any_has("phone", "string"), "no variant with phone: string");
    an.client.batch_execute("DROP TABLE ck_contact;").await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn check_row_level_case_discriminated_union() {
    let an = fresh_db().await;
    an.client.batch_execute(
        "DROP TABLE IF EXISTS ck_field;
         DROP TYPE IF EXISTS ck_field_type;
         CREATE TYPE ck_field_type AS ENUM ('text', 'select');
         CREATE TABLE ck_field (
             id int PRIMARY KEY,
             field_type ck_field_type NOT NULL,
             config jsonb NOT NULL,
             CHECK (CASE
                 WHEN field_type = 'text'   THEN jsonb_typeof(config->'maxLength') = 'number'
                 WHEN field_type = 'select' THEN jsonb_typeof(config->'options')   = 'array'
                 ELSE false END)
         );",
    ).await.unwrap();
    let schemas = an.table_schemas(&[("public".into(), "ck_field".into())])
        .await.expect("table_schemas");
    let t = schemas.iter().find(|t| t.table == "ck_field").expect("ck_field");
    assert_eq!(t.row_variants.len(), 2);
    // First variant pins field_type to "text" and config to a maxLength shape.
    assert!(t.row_variants[0].columns["field_type"].contains("text"));
    assert!(t.row_variants[0].columns["config"].contains("maxLength: number"));
    assert!(t.row_variants[1].columns["field_type"].contains("select"));
    assert!(t.row_variants[1].columns["config"].contains("options: Json[]"));
    an.client.batch_execute(
        "DROP TABLE ck_field; DROP TYPE ck_field_type;",
    ).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn check_arbitrary_predicate_leaves_base_type_unchanged() {
    let an = fresh_db().await;
    with_table(
        &an,
        "DROP TABLE IF EXISTS ck_slug;
         CREATE TABLE ck_slug (
             id int PRIMARY KEY,
             slug text NOT NULL CHECK (length(slug) > 0)
         );",
        "DROP TABLE ck_slug;",
        async {
            let q = an.analyze("SELECT slug FROM ck_slug WHERE id = $1")
                .await.expect("analyze");
            assert_eq!(q.columns[0].ts_type, "string",
                "arithmetic / function predicates must bail and keep base type");
        },
    ).await;
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

// ----- Param nullability inference -----

#[tokio::test(flavor = "current_thread")]
async fn insert_values_param_to_not_null_column_is_not_nullable() {
    let an = fresh_db().await;
    // `orgs.id` and `orgs.name` are both NOT NULL.
    let q = an.analyze("INSERT INTO orgs (id, name) VALUES ($1, $2)")
        .await.expect("analyze");
    assert_eq!(q.params.len(), 2);
    assert!(!q.params[0].nullable, "$1 → orgs.id (NOT NULL); got nullable");
    assert!(!q.params[1].nullable, "$2 → orgs.name (NOT NULL); got nullable");
}

#[tokio::test(flavor = "current_thread")]
async fn insert_values_param_to_nullable_column_stays_nullable() {
    let an = fresh_db().await;
    // `users.display_name` is nullable.
    let q = an.analyze(
        "INSERT INTO users (id, org_id, email, role, display_name, settings) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    ).await.expect("analyze");
    assert_eq!(q.params.len(), 6);
    assert!(!q.params[0].nullable, "$1 → users.id (NOT NULL)");
    assert!(!q.params[1].nullable, "$2 → users.org_id (NOT NULL)");
    assert!(!q.params[2].nullable, "$3 → users.email (NOT NULL)");
    assert!(!q.params[3].nullable, "$4 → users.role (NOT NULL)");
    assert!(q.params[4].nullable, "$5 → users.display_name (nullable)");
    assert!(!q.params[5].nullable, "$6 → users.settings (NOT NULL)");
}

#[tokio::test(flavor = "current_thread")]
async fn update_set_param_to_not_null_column_is_not_nullable() {
    let an = fresh_db().await;
    // `posts.body` is NOT NULL; the WHERE param stays nullable.
    let q = an.analyze("UPDATE posts SET body = $1 WHERE id = $2")
        .await.expect("analyze");
    assert_eq!(q.params.len(), 2);
    assert!(!q.params[0].nullable, "$1 → posts.body (NOT NULL)");
    assert!(q.params[1].nullable, "$2 in WHERE — stays nullable");
}

#[tokio::test(flavor = "current_thread")]
async fn select_where_param_stays_nullable() {
    let an = fresh_db().await;
    // Reading via WHERE never tightens — null is a valid value to test against.
    let q = an.analyze("SELECT id FROM users WHERE id = $1").await.expect("analyze");
    assert_eq!(q.params.len(), 1);
    assert!(q.params[0].nullable, "WHERE-only param should stay nullable");
}

#[tokio::test(flavor = "current_thread")]
async fn insert_values_wrapped_in_coalesce_stays_nullable() {
    let an = fresh_db().await;
    // Even though users.role is NOT NULL, $4 is wrapped — caller may pass null
    // and coalesce will substitute the literal.
    let q = an.analyze(
        "INSERT INTO users (id, org_id, email, role, settings) \
         VALUES ($1, $2, $3, coalesce($4, 'member'::user_role), $5)",
    ).await.expect("analyze");
    assert_eq!(q.params.len(), 5);
    assert!(!q.params[0].nullable);
    assert!(!q.params[1].nullable);
    assert!(!q.params[2].nullable);
    assert!(q.params[3].nullable, "$4 inside coalesce(...) stays nullable");
    assert!(!q.params[4].nullable);
}

// ----- Table-typed column + param references -----

#[tokio::test(flavor = "current_thread")]
async fn select_column_carries_table_ref() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT id, email FROM users WHERE id = $1").await.expect("analyze");
    let id_ref = q.columns[0].table_ref.as_ref().expect("id column should carry table_ref");
    assert_eq!(id_ref.schema, "public");
    assert_eq!(id_ref.table, "users");
    assert_eq!(id_ref.column, "id");
    let email_ref = q.columns[1].table_ref.as_ref().expect("email column should carry table_ref");
    assert_eq!(email_ref.column, "email");
}

#[tokio::test(flavor = "current_thread")]
async fn count_star_has_no_table_ref() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT count(*) AS n FROM users").await.expect("analyze");
    assert!(q.columns[0].table_ref.is_none(),
        "count(*) has no underlying base column; should not carry a table_ref");
}

#[tokio::test(flavor = "current_thread")]
async fn cast_column_has_no_table_ref() {
    let an = fresh_db().await;
    // Casting an existing column to a different type drops the column-ref
    // metadata in Postgres's RowDescription — `table_oid`/`attnum` are 0
    // and our resolver can't link back to the base table.
    let q = an.analyze("SELECT id::text AS id_text FROM users").await.expect("analyze");
    assert!(q.columns[0].table_ref.is_none(),
        "casted column shouldn't carry a table_ref");
}

#[tokio::test(flavor = "current_thread")]
async fn insert_values_param_carries_table_ref() {
    let an = fresh_db().await;
    let q = an.analyze("INSERT INTO orgs (id, name) VALUES ($1, $2)")
        .await.expect("analyze");
    let r0 = q.params[0].table_ref.as_ref().expect("$1 → orgs.id");
    assert_eq!(r0.schema, "public");
    assert_eq!(r0.table, "orgs");
    assert_eq!(r0.column, "id");
    let r1 = q.params[1].table_ref.as_ref().expect("$2 → orgs.name");
    assert_eq!(r1.column, "name");
}

#[tokio::test(flavor = "current_thread")]
async fn where_param_has_no_table_ref() {
    let an = fresh_db().await;
    let q = an.analyze("SELECT id FROM users WHERE id = $1").await.expect("analyze");
    assert!(q.params[0].table_ref.is_none(),
        "WHERE-clause params don't bind to a target column; no table_ref");
}

#[tokio::test(flavor = "current_thread")]
async fn table_schemas_returns_full_column_list() {
    let an = fresh_db().await;
    let result = an.table_schemas(&[("public".into(), "users".into())]).await
        .expect("query ok");
    assert_eq!(result.len(), 1);
    let t = &result[0];
    assert_eq!(t.schema, "public");
    assert_eq!(t.table, "users");
    let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"id"));
    assert!(names.contains(&"display_name"));
    let id_col = t.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id_col.not_null, "users.id is NOT NULL");
    let dn_col = t.columns.iter().find(|c| c.name == "display_name").unwrap();
    assert!(!dn_col.not_null, "users.display_name is nullable");
}

#[tokio::test(flavor = "current_thread")]
async fn table_schemas_skips_missing_tables() {
    let an = fresh_db().await;
    let result = an.table_schemas(&[
        ("public".into(), "users".into()),
        ("public".into(), "no_such_table".into()),
    ]).await.expect("query ok");
    // Missing table silently dropped; existing one comes through.
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].table, "users");
}
