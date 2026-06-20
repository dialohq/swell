//! `pg_catalog` enrichment, fetched once at connect.

use crate::ts_types::TypeCatalog;
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio_postgres::Client;

/// `(castsource, casttarget)` pairs for user-defined function-based
/// casts — the only cast shape whose result can be NULL on non-NULL
/// input. `'b'` (binary) and `'i'` (I/O) casts are total; built-in
/// `'f'` casts (oid < 16384) call core functions that don't return
/// NULL either. Looked up per-Cast at lowering for `is_unsafe`.
pub async fn fetch_unsafe_casts(client: &Client) -> HashSet<(u32, u32)> {
    let sql = "SELECT castsource::oid, casttarget::oid \
         FROM pg_cast WHERE castmethod = 'f' AND oid >= 16384";
    client
        .query(sql, &[])
        .await
        .unwrap_or_else(|e| {
            tracing::debug!("fetch_unsafe_casts: {e}");
            Vec::new()
        })
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

/// `pg_type.typname → pg_type.oid` for resolving `TypeCast` targets
/// without a per-query round-trip.
pub async fn fetch_typname_to_oid(client: &Client) -> HashMap<String, u32> {
    client
        .query("SELECT typname, oid::oid FROM pg_type", &[])
        .await
        .unwrap_or_else(|e| {
            tracing::debug!("fetch_typname_to_oid: {e}");
            Vec::new()
        })
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

/// Pull every enum / domain / composite / range / array definition the
/// analyzer needs to render TS types. `pg_catalog` is always included.
pub async fn load_type_catalog(client: &Client, schemas: &[String]) -> anyhow::Result<TypeCatalog> {
    let mut cat = TypeCatalog::default();

    let mut allow: Vec<String> = schemas.to_vec();
    if !allow.iter().any(|s| s == "pg_catalog") {
        allow.push("pg_catalog".into());
    }

    // Enums.
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

    // Domains. Domains can chain — walk parent edges to the ultimate
    // non-domain base in Rust rather than via a recursive CTE.
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

    // Composite types via `pg_class.relkind='c'`.
    let rows = client
        .query(
            r#"
        SELECT t.oid::oid, a.attname, a.atttypid::oid, ft.typname, a.attnum
        FROM pg_type t
        JOIN pg_class c ON c.oid = t.typrelid
        JOIN pg_attribute a ON a.attrelid = c.oid
        JOIN pg_type ft ON ft.oid = a.atttypid
        JOIN pg_namespace n ON n.oid = t.typnamespace
        WHERE t.typtype = 'c' AND c.relkind = 'c'
          AND n.nspname = ANY($1)
          AND a.attnum > 0 AND NOT a.attisdropped
        ORDER BY t.oid, a.attnum
        "#,
            &[&allow],
        )
        .await?;
    for row in &rows {
        let oid: u32 = row.get(0);
        let name: String = row.get(1);
        let field_oid: u32 = row.get(2);
        let field_type_name: String = row.get(3);
        cat.composites
            .entry(oid)
            .or_default()
            .push((name, field_oid, field_type_name));
    }

    // Range and multirange types. Multiranges have rngmultitypid != 0.
    let rows = client
        .query(
            r#"
        SELECT r.rngtypid::oid, r.rngsubtype::oid, st.typname, r.rngmultitypid::oid
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

    // User-defined arrays (`typcategory='A'`). Built-in arrays
    // already flow through tokio-postgres' `Type::Kind::Array(_)`.
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

    cat.safe_builtin_procs = load_safe_builtin_procs(client).await?;

    Ok(cat)
}

/// JSON helpers `json_shape.rs` transforms at AST level. We accept the
/// `pg_catalog` builtin only when no user-defined function in the
/// current `search_path` shadows the name.
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
    // `to_regproc(name)` errors on ambiguous variadic names
    // (`jsonb_build_object`, …), which would kill the whole probe.
    // Shadow-detection via NOT EXISTS is equivalent and safe.
    let names: Vec<String> = SAFE_BUILTIN_CANDIDATES
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = client
        .query(
            r#"
        WITH candidates AS (SELECT unnest($1::text[]) AS name)
        SELECT
            c.name,
            (SELECT p.oid FROM pg_catalog.pg_proc p
             WHERE p.proname = c.name
               AND p.pronamespace = 'pg_catalog'::regnamespace
             LIMIT 1) AS catalog_oid
        FROM candidates c
        WHERE NOT EXISTS (
            SELECT 1 FROM pg_catalog.pg_proc p
            JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
            WHERE p.proname = c.name
              AND n.nspname <> 'pg_catalog'
              AND n.nspname = ANY(current_schemas(false))
        )
        "#,
            &[&names],
        )
        .await?;
    let mut out = BTreeMap::new();
    for row in &rows {
        let name: String = row.get(0);
        if let Ok(oid) = row.try_get::<_, u32>(1) {
            out.insert(name, oid);
        }
    }
    Ok(out)
}

/// Hash over `pg_class.xmin` for the configured schemas — cheap DDL
/// cache invalidator.
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

/// Pick the catalog's lookup for user-defined enum/domain/composite
/// OIDs (postgres-types may not carry full info), otherwise fall back
/// to `cat.render`.
pub fn render_for_oid(
    cat: &TypeCatalog,
    oid: u32,
    ty: &postgres_types::Type,
    dir: crate::ts_types::Direction,
) -> String {
    use postgres_types::Kind;
    if matches!(ty.kind(), Kind::Pseudo)
        || cat.enums.contains_key(&oid)
        || cat.domains.contains_key(&oid)
        || cat.composites.contains_key(&oid)
    {
        return cat.render_oid(oid, ty.name(), dir);
    }
    cat.render(ty, dir)
}
