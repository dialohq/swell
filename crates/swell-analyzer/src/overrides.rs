//! SQLx-style alias overrides.
//!
//! Users can attach a suffix to a column alias to override its inferred
//! type or nullability:
//!
//!   - `as "col!"`        force NOT NULL
//!   - `as "col?"`        force nullable
//!   - `as "col: T"`      force TS type to `T`
//!   - `as "col!: T"`     force NOT NULL and override type
//!
//! Postgres accepts these unusual characters because they're inside a
//! quoted identifier. The driver returns them verbatim as the column name
//! in `RowDescription`. We post-process column names to extract the
//! override and rewrite the `name` / `nullable` / `ts_type` fields.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Override {
    pub clean_name: String,
    pub force_nullable: Option<bool>,
    pub force_ts_type: Option<String>,
}

/// Parse a column-name suffix into an override descriptor.
///
/// Returns the bare-name and any flags found:
///   "id"            → { name: "id" }
///   "label!"        → { name: "label", force_nullable: Some(false) }
///   "label?"        → { name: "label", force_nullable: Some(true) }
///   "label: Foo"    → { name: "label", force_ts_type: Some("Foo") }
///   "label!: Foo"   → { name: "label", force_nullable: Some(false),
///                       force_ts_type: Some("Foo") }
pub fn parse(name: &str) -> Override {
    let mut o = Override::default();

    // Split off `: T` first.
    let (head, ts) = match name.find(": ") {
        Some(i) => (&name[..i], Some(name[i + 2..].trim().to_string())),
        None => (name, None),
    };
    o.force_ts_type = ts.filter(|s| !s.is_empty());

    // Look for trailing ! or ? on the head.
    if let Some(stripped) = head.strip_suffix('!') {
        o.force_nullable = Some(false);
        o.clean_name = stripped.to_string();
    } else if let Some(stripped) = head.strip_suffix('?') {
        o.force_nullable = Some(true);
        o.clean_name = stripped.to_string();
    } else {
        o.clean_name = head.to_string();
    }

    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_override() {
        let o = parse("email");
        assert_eq!(o.clean_name, "email");
        assert_eq!(o.force_nullable, None);
        assert_eq!(o.force_ts_type, None);
    }

    #[test]
    fn force_not_null() {
        let o = parse("label!");
        assert_eq!(o.clean_name, "label");
        assert_eq!(o.force_nullable, Some(false));
    }

    #[test]
    fn force_nullable() {
        let o = parse("label?");
        assert_eq!(o.clean_name, "label");
        assert_eq!(o.force_nullable, Some(true));
    }

    #[test]
    fn type_override() {
        let o = parse("settings: UserSettings");
        assert_eq!(o.clean_name, "settings");
        assert_eq!(o.force_ts_type.as_deref(), Some("UserSettings"));
    }

    #[test]
    fn type_and_not_null() {
        let o = parse("payload!: { kind: \"x\" }");
        assert_eq!(o.clean_name, "payload");
        assert_eq!(o.force_nullable, Some(false));
        assert_eq!(o.force_ts_type.as_deref(), Some("{ kind: \"x\" }"));
    }
}
