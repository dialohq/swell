//! Postgres `Type` → TypeScript type-string mapping. Mirrors what
//! postgres.js returns by default: int8/numeric/text → `string`,
//! timestamp{tz} → `Date`, bytea → `Uint8Array`, json{b} → `Json`,
//! enum → `"a" | "b" | ...`, domain → base, composite → struct,
//! array → `T[]`, pseudo → `unknown`.

use postgres_types::{Kind, Type};
use std::collections::BTreeMap;

/// Read (row column, table interface, json shape) vs write (query
/// param). Drivers can register parse and serialize side independently
/// — e.g. `date` reads as `Spacetime` but writes as `Date | string`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Direction { Read, Write }

/// Per-type override. Bare string in TOML deserializes both fields
/// equal; `{ parse, serialize }` lets them diverge.
#[derive(Debug, Clone)]
pub struct TypeOverride {
    pub parse: String,
    pub serialize: String,
}

/// `pg_catalog` info gathered at connect time, reused per query.
#[derive(Debug, Clone, Default)]
pub struct TypeCatalog {
    pub enums: BTreeMap<u32, Vec<String>>,
    /// domain OID → (base OID, base typname). Keeping the typname lets
    /// us resolve the right TS scalar when recursing through a domain.
    pub domains: BTreeMap<u32, (u32, String)>,
    /// composite OID → `[(name, field_typoid, field_typname), …]`.
    pub composites: BTreeMap<u32, Vec<(String, u32, String)>>,
    /// range / multirange OID → (element OID, name).
    pub ranges: BTreeMap<u32, (u32, String)>,
    /// User-defined array OID → (element OID, name). Built-ins flow
    /// through `Type::Kind::Array(_)` automatically.
    pub arrays: BTreeMap<u32, (u32, String)>,
    /// Per-PG-typname overrides (`"jsonb" → …`).
    pub by_name: BTreeMap<String, TypeOverride>,
    /// Unqualified builtin proname → canonical `pg_catalog` OID, only
    /// when the connect-time probe confirmed no user-defined shadow in
    /// the current `search_path`. Consumed by json_shape inference.
    pub safe_builtin_procs: BTreeMap<String, u32>,
}

impl TypeCatalog {
    fn override_for(&self, name: &str, dir: Direction) -> Option<&str> {
        self.by_name.get(name).map(|o| match dir {
            Direction::Read => o.parse.as_str(),
            Direction::Write => o.serialize.as_str(),
        })
    }

    pub fn render(&self, t: &Type, dir: Direction) -> String {
        if let Some(over) = self.override_for(t.name(), dir) { return over.to_string(); }
        // For user types `postgres-types` sees opaque OIDs — route
        // through `render_oid` to pick up catalog info.
        match t.kind() {
            Kind::Simple => {
                if self.enums.contains_key(&t.oid())
                    || self.domains.contains_key(&t.oid())
                    || self.composites.contains_key(&t.oid())
                    || self.ranges.contains_key(&t.oid())
                {
                    self.render_oid(t.oid(), t.name(), dir)
                } else {
                    simple_name_to_ts(t.name()).to_string()
                }
            }
            Kind::Array(inner) => {
                let inner_ts = self.render(inner, dir);
                format!("{}[]", maybe_paren_for_array(&inner_ts))
            }
            Kind::Range(inner) => {
                let inner_ts = self.render(inner, dir);
                format!("{{ lower: {} | null; upper: {} | null }}", inner_ts, inner_ts)
            }
            Kind::Domain(base) => self.render(base, dir),
            Kind::Composite(fields) => {
                let inner: Vec<String> = fields.iter()
                    .map(|f| format!("{}: {} | null", quote_field(f.name()), self.render(f.type_(), dir)))
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

    /// OID lookup through the catalog for user-defined types.
    pub fn render_oid(&self, oid: u32, name: &str, dir: Direction) -> String {
        if let Some(over) = self.override_for(name, dir) { return over.to_string(); }
        if let Some(labels) = self.enums.get(&oid) {
            return if labels.is_empty() { "string".into() } else { enum_union(labels) };
        }
        if let Some((base_oid, base_name)) = self.domains.get(&oid) {
            return self.render_oid(*base_oid, base_name, dir);
        }
        if let Some(fields) = self.composites.get(&oid) {
            // PG composite attributes are always nullable.
            let inner: Vec<String> = fields.iter()
                .map(|(n, t_oid, t_name)| {
                    let ts = self.render_oid(*t_oid, t_name, dir);
                    format!("{}: {} | null", quote_field(n), ts)
                })
                .collect();
            return format!("{{ {} }}", inner.join("; "));
        }
        if let Some((elem_oid, elem_name)) = self.ranges.get(&oid) {
            let elem = self.render_oid(*elem_oid, elem_name, dir);
            return format!("{{ lower: {} | null; upper: {} | null }}", elem, elem);
        }
        if let Some((elem_oid, elem_name)) = self.arrays.get(&oid) {
            let elem = self.render_oid(*elem_oid, elem_name, dir);
            return format!("{}[]", maybe_paren_for_array(&elem));
        }
        simple_name_to_ts(name).to_string()
    }
}

/// Mirrors postgres.js default decoding.
fn simple_name_to_ts(name: &str) -> &'static str {
    match name {
        "bool" => "boolean",
        "int2" | "int4" | "float4" | "float8" => "number",
        "int8" | "numeric" | "text" | "varchar" | "bpchar" | "char" | "name"
            | "uuid" | "cidr" | "inet" | "macaddr" | "citext"
            | "time" | "timetz" | "interval" => "string",
        "bytea" => "Uint8Array",
        "date" | "timestamp" | "timestamptz" => "Date",
        "json" | "jsonb" => "Json",
        "void" => "void",
        _ => "unknown",
    }
}

fn enum_union(labels: &[String]) -> String {
    labels.iter()
        .map(|l| format!("\"{}\"", l.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// `T | null` arrays need parens: `(T | null)[]` not `T | null[]`.
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

    fn over(s: &str) -> TypeOverride {
        TypeOverride { parse: s.into(), serialize: s.into() }
    }

    #[test]
    fn scalars() {
        let c = TypeCatalog::default();
        let r = Direction::Read;
        assert_eq!(c.render(&Type::INT4, r), "number");
        assert_eq!(c.render(&Type::TEXT, r), "string");
        assert_eq!(c.render(&Type::BOOL, r), "boolean");
        assert_eq!(c.render(&Type::INT8, r), "string");
        assert_eq!(c.render(&Type::TIMESTAMPTZ, r), "Date");
        assert_eq!(c.render(&Type::JSONB, r), "Json");
        assert_eq!(c.render(&Type::UUID, r), "string");
        assert_eq!(c.render(&Type::BYTEA, r), "Uint8Array");
    }

    #[test]
    fn arrays() {
        let c = TypeCatalog::default();
        let r = Direction::Read;
        assert_eq!(c.render(&Type::INT4_ARRAY, r), "number[]");
        assert_eq!(c.render(&Type::TEXT_ARRAY, r), "string[]");
    }

    #[test]
    fn override_by_name() {
        let mut c = TypeCatalog::default();
        c.by_name.insert("jsonb".into(), over("Json"));
        assert_eq!(c.render(&Type::JSONB, Direction::Read), "Json");
        assert_eq!(c.render(&Type::JSON, Direction::Read), "Json");
    }

    #[test]
    fn override_split_parse_vs_serialize() {
        let mut c = TypeCatalog::default();
        c.by_name.insert("date".into(), TypeOverride {
            parse: "Spacetime".into(),
            serialize: "Spacetime | Date | string".into(),
        });
        assert_eq!(c.render(&Type::DATE, Direction::Read), "Spacetime");
        assert_eq!(c.render(&Type::DATE, Direction::Write), "Spacetime | Date | string");
        // Arrays carry direction through.
        assert_eq!(c.render(&Type::DATE_ARRAY, Direction::Read), "Spacetime[]");
        assert_eq!(c.render(&Type::DATE_ARRAY, Direction::Write), "(Spacetime | Date | string)[]");
    }
}
