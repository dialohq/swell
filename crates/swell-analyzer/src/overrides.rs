//! SQLx-style alias overrides.
//!
//! Users can attach a suffix to a quoted alias to override its inferred
//! nullability:
//!
//!   - `as "col!"`        force NOT NULL
//!   - `as "col?"`        force nullable
//!
//! Postgres accepts `!` / `?` inside quoted identifiers, so the driver
//! returns the marker verbatim as the column name in `RowDescription`.
//! The marker stays on the column name end-to-end — what the user
//! wrote in SQL is what they see in the row type — but the analyzer
//! still acts on it to widen / tighten the inferred nullability.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Override {
    /// The column's user-visible name. Preserves the trailing `!` / `?`
    /// marker — Postgres surfaces it as the column name, so the row type
    /// matches the SQL the user wrote.
    pub clean_name: String,
    pub force_nullable: Option<bool>,
}

/// Parse a column-name suffix into an override descriptor.
///
///   "id"            → { name: "id" }
///   "label!"        → { name: "label!", force_nullable: Some(false) }
///   "label?"        → { name: "label?", force_nullable: Some(true) }
pub fn parse(name: &str) -> Override {
    Override {
        clean_name: name.to_string(),
        force_nullable: match name.chars().last() {
            Some('!') => Some(false),
            Some('?') => Some(true),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_override() {
        let o = parse("email");
        assert_eq!(o.clean_name, "email");
        assert_eq!(o.force_nullable, None);
    }

    #[test]
    fn force_not_null_preserves_marker_in_name() {
        let o = parse("label!");
        assert_eq!(o.clean_name, "label!");
        assert_eq!(o.force_nullable, Some(false));
    }

    #[test]
    fn force_nullable_preserves_marker_in_name() {
        let o = parse("label?");
        assert_eq!(o.clean_name, "label?");
        assert_eq!(o.force_nullable, Some(true));
    }
}
