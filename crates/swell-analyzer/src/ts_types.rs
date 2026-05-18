//! Postgres `Type` → TypeScript type-string mapping.
//!
//! The TS type strings produced here are emitted verbatim into the generated
//! `.d.ts`. They reflect what the `postgres` (postgres.js) driver actually
//! returns for each PG type by default:
//!   - bigint (int8) → `string` (postgres.js stringifies to avoid JS number loss)
//!   - timestamp / timestamptz → `Date`
//!   - bytea → `Uint8Array`
//!   - jsonb / json → `Json` (recursive structural alias). Accessing fields
//!     requires runtime narrowing; M7's AST-driven shape inference will
//!     produce concrete row shapes for jsonb literals where possible.
//!   - enum → `"a" | "b" | ...`
//!   - array → `T[]`
//!   - domain → underlying base type
//!   - composite → `{ field: T; ... }`
//!   - unknown / pseudo → `unknown`
//!
//! User overrides (config, alias `as "col: T"`) plug in higher up the pipeline.

use postgres_types::{Kind, Type};
use std::collections::BTreeMap;

/// Per-database lookups gathered from `pg_catalog` ahead of type rendering.
/// Built once per analyzer connection then reused for every query.
#[derive(Debug, Clone, Default)]
pub struct TypeCatalog {
    /// enum OID → ordered labels
    pub enums: BTreeMap<u32, Vec<String>>,
    /// domain OID → (base type OID, base type name) — keep the name so that
    /// recursing through a domain resolves to the right TS scalar.
    pub domains: BTreeMap<u32, (u32, String)>,
    /// composite type OID → list of (field_name, field_type_oid)
    pub composites: BTreeMap<u32, Vec<(String, u32)>>,
    /// range / multirange OID → element type OID (and its name)
    pub ranges: BTreeMap<u32, (u32, String)>,
    /// array OID → element type OID (and its name) — covers user-defined
    /// arrays we wouldn't otherwise resolve via postgres-types.
    pub arrays: BTreeMap<u32, (u32, String)>,
    /// User-supplied per-PG-type overrides keyed by PG type name (e.g. "jsonb").
    pub by_name: BTreeMap<String, String>,
    /// Unqualified built-in proname → canonical `pg_catalog` proc OID, *only*
    /// populated when the analyzer has verified at connect-time that the
    /// name resolves to the catalog version under the current `search_path`
    /// (i.e. no user-defined shadow). Used by the JSON shape inference to
    /// avoid treating a user-shadowed `jsonb_build_object` as the built-in.
    pub safe_builtin_procs: BTreeMap<String, u32>,
}

impl TypeCatalog {
    /// Render a Postgres type as a TypeScript type-string.
    pub fn render(&self, t: &Type) -> String {
        // 1. user override by PG type name wins
        if let Some(over) = self.by_name.get(t.name()) {
            return over.clone();
        }
        // 2. structural cases. For user types the postgres-types crate
        //    sees opaque OIDs, so route through render_oid to pick up the
        //    catalog (enum labels, domain bases, composite fields,
        //    range elements, custom arrays).
        match t.kind() {
            Kind::Simple => {
                if self.enums.contains_key(&t.oid())
                    || self.domains.contains_key(&t.oid())
                    || self.composites.contains_key(&t.oid())
                    || self.ranges.contains_key(&t.oid())
                {
                    self.render_oid(t.oid(), t.name())
                } else {
                    self.render_simple(t)
                }
            }
            Kind::Array(inner) => {
                let inner_ts = self.render(inner);
                format!("{}[]", maybe_paren_for_array(&inner_ts))
            }
            Kind::Range(inner) => {
                let inner_ts = self.render(inner);
                format!("{{ lower: {} | null; upper: {} | null }}", inner_ts, inner_ts)
            }
            Kind::Domain(base) => self.render(base),
            Kind::Composite(fields) => {
                let inner: Vec<String> = fields.iter()
                    .map(|f| format!("{}: {}", quote_field(f.name()), self.render(f.type_())))
                    .collect();
                format!("{{ {} }}", inner.join("; "))
            }
            Kind::Enum(labels) => {
                if labels.is_empty() { "string".into() } else { enum_union(labels) }
            }
            Kind::Pseudo => "unknown".to_string(),
            _ => "unknown".to_string(),
        }
    }

    /// Lookup by raw OID, going through the catalog for user-defined types
    /// that the postgres-types crate doesn't carry full info on. Falls back to
    /// the bare name if all else fails.
    pub fn render_oid(&self, oid: u32, name: &str) -> String {
        // user override
        if let Some(over) = self.by_name.get(name) {
            return over.clone();
        }
        if let Some(labels) = self.enums.get(&oid) {
            if labels.is_empty() {
                return "string".into();
            }
            return enum_union(labels);
        }
        if let Some((base_oid, base_name)) = self.domains.get(&oid) {
            return self.render_oid(*base_oid, base_name);
        }
        if let Some(fields) = self.composites.get(&oid) {
            let inner: Vec<String> = fields.iter()
                .map(|(n, t_oid)| format!("{}: {}", quote_field(n), self.render_oid(*t_oid, "")))
                .collect();
            return format!("{{ {} }}", inner.join("; "));
        }
        if let Some((elem_oid, elem_name)) = self.ranges.get(&oid) {
            let elem = self.render_oid(*elem_oid, elem_name);
            return format!("{{ lower: {} | null; upper: {} | null }}", elem, elem);
        }
        if let Some((elem_oid, elem_name)) = self.arrays.get(&oid) {
            let elem = self.render_oid(*elem_oid, elem_name);
            return format!("{}[]", maybe_paren_for_array(&elem));
        }
        simple_name_to_ts(name).to_string()
    }

    fn render_simple(&self, t: &Type) -> String {
        simple_name_to_ts(t.name()).to_string()
    }
}

fn simple_name_to_ts(name: &str) -> &'static str {
    // Mapping mirrors postgres.js default decoding behaviour.
    match name {
        "bool" => "boolean",
        "int2" | "int4" | "float4" | "float8" => "number",
        "int8" | "numeric" => "string",
        "text" | "varchar" | "bpchar" | "char" | "name" | "uuid" | "cidr" | "inet" | "macaddr" | "citext" => "string",
        "bytea" => "Uint8Array",
        "date" | "timestamp" | "timestamptz" => "Date",
        "time" | "timetz" | "interval" => "string",
        "json" | "jsonb" => "Json",
        "void" => "void",
        // Vector / extension types we don't auto-detect
        _ => "unknown",
    }
}

fn enum_union(labels: &[String]) -> String {
    labels.iter()
        .map(|l| format!("\"{}\"", l.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// `T | null` becomes `(T | null)[]` when written as an array — without
/// parens TS would parse it as `T | null[]`.
fn maybe_paren_for_array(inner: &str) -> String {
    let needs_paren = inner.contains(" | ")
        || inner.contains(" & ")
        || inner.contains(" => ");
    if needs_paren { format!("({})", inner) } else { inner.to_string() }
}

fn quote_field(name: &str) -> String {
    let simple = !name.is_empty()
        && name.chars().next().unwrap().is_ascii_alphabetic()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if simple { name.to_string() } else { format!("\"{}\"", name.replace('"', "\\\"")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        let c = TypeCatalog::default();
        assert_eq!(c.render(&Type::INT4), "number");
        assert_eq!(c.render(&Type::TEXT), "string");
        assert_eq!(c.render(&Type::BOOL), "boolean");
        assert_eq!(c.render(&Type::INT8), "string");
        assert_eq!(c.render(&Type::TIMESTAMPTZ), "Date");
        assert_eq!(c.render(&Type::JSONB), "Json");
        assert_eq!(c.render(&Type::UUID), "string");
        assert_eq!(c.render(&Type::BYTEA), "Uint8Array");
    }

    #[test]
    fn arrays() {
        let c = TypeCatalog::default();
        assert_eq!(c.render(&Type::INT4_ARRAY), "number[]");
        assert_eq!(c.render(&Type::TEXT_ARRAY), "string[]");
    }

    #[test]
    fn override_by_name() {
        let mut c = TypeCatalog::default();
        c.by_name.insert("jsonb".into(), "Json".into());
        assert_eq!(c.render(&Type::JSONB), "Json");
        assert_eq!(c.render(&Type::JSON), "Json"); // json/jsonb both default to Json now
    }
}
