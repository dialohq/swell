//! `pg_catalog` enrichment.
//!
//! Two passes:
//! 1. **Per-connection bootstrap** (`load_type_catalog`): one round-trip
//!    pulls every enum's labels, every domain's base type, and every
//!    composite-type's field list for the schemas we care about. The result
//!    is cached on the analyzer so subsequent query analysis is allocation-
//!    only.
//! 2. **Per-query nullability** (`fetch_attnotnull`): for each described
//!    column with a real (table_oid, attnum), fetch `pg_attribute.attnotnull`
//!    to refine the default-pessimistic nullability assumption.

use crate::ts_types::TypeCatalog;
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio_postgres::Client;

/// Pull every enum / domain / composite definition the analyzer needs in
/// order to render TS types correctly. Restricted to the supplied schemas
/// to keep the working set tight.
///
/// Always also includes the `pg_catalog` schema so that built-in types are
/// covered (e.g. you can put a `pg_catalog` enum in a query and we'll cope).
pub async fn load_type_catalog(client: &Client, schemas: &[String]) -> anyhow::Result<TypeCatalog> {
    let mut cat = TypeCatalog::default();

    let mut allow: Vec<String> = schemas.to_vec();
    if !allow.iter().any(|s| s == "pg_catalog") {
        allow.push("pg_catalog".into());
    }

    // ------------ Enums ------------
    let rows = client
        .query(
            r#"
            SELECT t.oid::oid AS oid, e.enumlabel
            FROM pg_type t
            JOIN pg_enum e ON e.enumtypid = t.oid
            JOIN pg_namespace n ON n.oid = t.typnamespace
            WHERE t.typtype = 'e' AND n.nspname = ANY($1)
            ORDER BY t.oid, e.enumsortorder
            "#,
            &[&allow],
        )
        .await?;
    for row in &rows {
        let oid: u32 = row.get(0);
        let label: String = row.get(1);
        cat.enums.entry(oid).or_default().push(label);
    }

    // ------------ Domains ------------
    // Pull every domain's *immediate* parent. Domains can chain (domain
    // over domain), so we walk the parent edges in Rust to find the
    // ultimate non-domain base — simpler than a recursive CTE.
    let rows = client
        .query(
            r#"
            SELECT t.oid::oid, t.typbasetype::oid, t.typtype, b.typname
            FROM pg_type t
            JOIN pg_type b ON b.oid = t.typbasetype
            JOIN pg_namespace n ON n.oid = t.typnamespace
            WHERE t.typtype = 'd' AND n.nspname = ANY($1)
            "#,
            &[&allow],
        )
        .await?;
    let mut parent: HashMap<u32, (u32, String)> = HashMap::new();
    for row in &rows {
        let oid: u32 = row.get(0);
        let base: u32 = row.get(1);
        let base_name: String = row.get(3);
        parent.insert(oid, (base, base_name));
    }
    for &start in parent.keys() {
        let mut cur = start;
        let (final_oid, final_name) = loop {
            match parent.get(&cur) {
                Some((next_oid, _)) if parent.contains_key(next_oid) => cur = *next_oid,
                Some((next_oid, next_name)) => break (*next_oid, next_name.clone()),
                None => break (cur, String::new()),
            }
        };
        cat.domains.insert(start, (final_oid, final_name));
    }

    // ------------ Composite types ------------
    // pg_type.typtype='c' → relation in pg_class with relkind='c'.
    // Each composite has a row in pg_class whose oid we use to enumerate
    // pg_attribute. Skip dropped attributes and system columns (attnum > 0).
    let rows = client
        .query(
            r#"
            SELECT t.oid::oid AS type_oid, a.attname, a.atttypid::oid, a.attnum
            FROM pg_type t
            JOIN pg_class c ON c.oid = t.typrelid
            JOIN pg_attribute a ON a.attrelid = c.oid
            JOIN pg_namespace n ON n.oid = t.typnamespace
            WHERE t.typtype = 'c'
              AND c.relkind = 'c'
              AND n.nspname = ANY($1)
              AND a.attnum > 0
              AND NOT a.attisdropped
            ORDER BY t.oid, a.attnum
            "#,
            &[&allow],
        )
        .await?;
    for row in &rows {
        let oid: u32 = row.get(0);
        let name: String = row.get(1);
        let field_oid: u32 = row.get(2);
        cat.composites.entry(oid).or_default().push((name, field_oid));
    }

    // ------------ Range and multirange types ------------
    // Both share `pg_range`; multiranges have rngmultitypid != 0.
    let rows = client
        .query(
            r#"
            SELECT r.rngtypid::oid, r.rngsubtype::oid, st.typname,
                   r.rngmultitypid::oid
            FROM pg_range r
            JOIN pg_type rt ON rt.oid = r.rngtypid
            JOIN pg_type st ON st.oid = r.rngsubtype
            JOIN pg_namespace n ON n.oid = rt.typnamespace
            WHERE n.nspname = ANY($1)
            "#,
            &[&allow],
        )
        .await?;
    for row in &rows {
        let rng_oid: u32 = row.get(0);
        let elem_oid: u32 = row.get(1);
        let elem_name: String = row.get(2);
        let multi_oid: u32 = row.get(3);
        cat.ranges.insert(rng_oid, (elem_oid, elem_name.clone()));
        if multi_oid != 0 {
            cat.ranges.insert(multi_oid, (elem_oid, elem_name));
        }
    }

    // ------------ User-defined arrays ------------
    // pg_type.typcategory = 'A' for arrays. We resolve the element via
    // `typelem` and store the element's typname so render_oid can map it
    // through the catalog. Built-in arrays (e.g. text[]) flow through
    // tokio-postgres' `Type::Kind::Array(_)` already; this entry only
    // matters for user-type arrays the driver hasn't seen.
    let rows = client
        .query(
            r#"
            SELECT t.oid::oid, e.oid::oid, e.typname
            FROM pg_type t
            JOIN pg_type e ON e.oid = t.typelem
            JOIN pg_namespace n ON n.oid = t.typnamespace
            WHERE t.typcategory = 'A' AND t.typelem <> 0
              AND n.nspname = ANY($1)
            "#,
            &[&allow],
        )
        .await?;
    for row in &rows {
        let arr_oid: u32 = row.get(0);
        let elem_oid: u32 = row.get(1);
        let elem_name: String = row.get(2);
        cat.arrays.insert(arr_oid, (elem_oid, elem_name));
    }

    // ------------ Safe built-in JSON helpers ------------
    // Verify that each unqualified name we transform in `json_shape.rs`
    // resolves under the live `search_path` to the *real* `pg_catalog`
    // function — i.e. no user-defined shadow exists. We compare the
    // OID returned by `to_regproc(name)` (which respects search_path) to
    // the OID stored in `pg_catalog.pg_proc` for the catalog version. If
    // they match, the name is "safe" to use unqualified in inference. If
    // they don't match (or to_regproc returns NULL, meaning ambiguous or
    // missing), we drop the unqualified form — `pg_catalog.X` still works
    // because it's explicitly qualified at the call site.
    cat.safe_builtin_procs = load_safe_builtin_procs(client).await?;

    Ok(cat)
}

/// Names of the `pg_catalog` JSON helpers that `json_shape.rs` transforms
/// at AST level. Each is verified to be the canonical built-in before the
/// transform applies; if a user-defined function shadows the name, the
/// inference falls through to the default (Json) for safety.
const SAFE_BUILTIN_CANDIDATES: &[&str] = &[
    "jsonb_build_object",
    "json_build_object",
    "jsonb_agg",
    "json_agg",
    "to_jsonb",
    "row_to_json",
    "jsonb_object_agg",
    "json_object_agg",
];

async fn load_safe_builtin_procs(client: &Client) -> anyhow::Result<BTreeMap<String, u32>> {
    // For each candidate name, pair: (resolved OID via to_regproc, canonical
    // pg_catalog OID via pg_proc lookup). Keep the name iff both are present
    // and equal. Run as one round-trip via `unnest` to avoid N queries.
    let names: Vec<String> =
        SAFE_BUILTIN_CANDIDATES.iter().map(|s| s.to_string()).collect();

    let rows = client
        .query(
            r#"
            WITH candidates AS (
                SELECT unnest($1::text[]) AS name
            )
            SELECT
                c.name,
                to_regproc(c.name)::oid AS resolved_oid,
                (
                    SELECT p.oid
                    FROM pg_catalog.pg_proc p
                    WHERE p.proname = c.name
                      AND p.pronamespace = 'pg_catalog'::regnamespace
                    LIMIT 1
                ) AS catalog_oid
            FROM candidates c
            "#,
            &[&names],
        )
        .await?;

    let mut out = BTreeMap::new();
    for row in &rows {
        let name: String = row.get(0);
        let resolved: Option<u32> = row.try_get(1).ok();
        let canonical: Option<u32> = row.try_get(2).ok();
        if let (Some(r), Some(c)) = (resolved, canonical) {
            if r == c {
                out.insert(name, c);
            }
        }
    }
    Ok(out)
}

/// Look up `attnotnull` for each (table_oid, attnum) pair we have. Returns a
/// map keyed by (table_oid, attnum). Pairs we can't resolve are absent.
pub async fn fetch_attnotnull(
    client: &Client,
    pairs: &[(u32, i16)],
) -> anyhow::Result<HashMap<(u32, i16), bool>> {
    if pairs.is_empty() {
        return Ok(HashMap::new());
    }
    // Deduplicate to reduce round-trip size and let Postgres plan-cache help.
    let unique: HashSet<&(u32, i16)> = pairs.iter().collect();
    let mut tables: Vec<i64> = Vec::with_capacity(unique.len());
    let mut attnums: Vec<i32> = Vec::with_capacity(unique.len());
    for (t, a) in &unique {
        tables.push(*t as i64);
        attnums.push(*a as i32);
    }

    let rows = client
        .query(
            r#"
            WITH ask(table_oid, attnum) AS (
                SELECT * FROM unnest($1::bigint[], $2::int[])
            )
            SELECT a.attrelid::bigint, a.attnum, a.attnotnull
            FROM ask
            JOIN pg_attribute a
              ON a.attrelid::bigint = ask.table_oid
             AND a.attnum = ask.attnum::smallint
            WHERE a.attnum > 0 AND NOT a.attisdropped
            "#,
            &[&tables, &attnums],
        )
        .await?;

    let mut out = HashMap::with_capacity(rows.len());
    for row in &rows {
        let t: i64 = row.get(0);
        let a: i16 = row.get(1);
        let nn: bool = row.get(2);
        out.insert((t as u32, a), nn);
    }
    Ok(out)
}

/// Sample the schema "version" — a cheap hash over `pg_class.xmin` for the
/// configured schemas. Used to invalidate the cache when DDL has happened.
pub async fn schema_fingerprint(client: &Client, schemas: &[String]) -> anyhow::Result<String> {
    let row = client
        .query_one(
            r#"
            SELECT coalesce(md5(string_agg(c.oid::text || ':' || c.xmin::text, ',' ORDER BY c.oid)), '')
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = ANY($1)
            "#,
            &[&schemas],
        )
        .await?;
    Ok(row.get::<_, String>(0))
}

/// Bring the rendered TS column type up to date once per query: take the
/// `Type` provided by `RowDescription` and prefer the catalog's lookup
/// (because postgres-types' `Kind` may not include enum labels for OIDs the
/// driver hasn't seen before).
pub fn render_for_oid(cat: &TypeCatalog, oid: u32, ty: &postgres_types::Type) -> String {
    use postgres_types::Kind;
    // Catalog wins for user-defined enum/domain/composite (struct-driven).
    if matches!(ty.kind(), Kind::Pseudo) || cat.enums.contains_key(&oid)
        || cat.domains.contains_key(&oid) || cat.composites.contains_key(&oid)
    {
        return cat.render_oid(oid, ty.name());
    }
    cat.render(ty)
}

// Small helper for tests
#[allow(dead_code)]
pub fn _used_in_tests(c: BTreeMap<u32, Vec<String>>) -> usize { c.len() }
