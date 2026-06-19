//! Comment-based per-column overrides.
//!
//! The alias-suffix form (`AS "col!"` — see `overrides.rs`) is sent
//! verbatim to Postgres, so `!` / `?` end up in the runtime column name
//! and `row.col` becomes `undefined`. Comments sidestep that: Postgres
//! drops them, but swell's analyzer reads them at codegen time and
//! attaches each one to the column it trails in the SELECT / RETURNING
//! list.
//!
//! Syntax — write the hint immediately after the column expression (or
//! its alias):
//!
//!   SELECT created_at::text AS "createdAt" --@swell.nonnullable
//!   FROM users;
//!
//!   SELECT settings /*@swell.type=UserSettings*/ FROM users;
//!
//! Supported attributes:
//!
//!   - `nonnullable` (aliases: `nonnull`, `notnull`) — force NOT NULL
//!   - `nullable`                                    — force nullable
//!   - `type=T` or `type: T`                         — override TS type
//!
//! One attribute per comment. Multiple comments can stack on the same
//! column. Stars (`SELECT *`) expand on Postgres's side, so column-index
//! attribution lines up with `*`-free queries only; hints on a starred
//! column attach to the literal `*` and don't fan out.

use crate::overrides::Override;
use pg_query::protobuf::{self, node::Node as NodeBody, ScanToken, Token};

const MARKER: &str = "@swell.";

/// One hint, already attached to a specific output column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub column_index: usize,
    pub override_: Override,
}

/// Walk the SQL's AST + token stream and return every `@swell.<attr>`
/// hint, paired with the column it trails. An empty result means no
/// hints (or a parse failure — we fall through silently because the
/// analyzer will surface the real Postgres parse error later).
pub fn extract(sql: &str) -> Vec<Hint> {
    let parsed = match pg_query::parse(sql) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let tokens = match pg_query::scan(sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let locations = output_column_locations(&parsed.protobuf);
    if locations.is_empty() {
        return Vec::new();
    }

    let mut hints = Vec::new();
    for tok in &tokens.tokens {
        if !is_comment(tok) {
            continue;
        }
        let body = comment_body(sql, tok);
        let Some(attr) = body.trim().strip_prefix(MARKER) else { continue };
        let Some(ov) = parse_attribute(attr) else { continue };
        let column_index = column_for_position(&locations, tok.start);
        hints.push(Hint { column_index, override_: ov });
    }
    hints
}

fn is_comment(tok: &ScanToken) -> bool {
    matches!(tok.token(), Token::SqlComment | Token::CComment)
}

/// Strip the comment delimiters and return the inner text.
fn comment_body(sql: &str, tok: &ScanToken) -> String {
    let raw = &sql[tok.start as usize..tok.end as usize];
    match tok.token() {
        Token::SqlComment => raw.trim_start_matches("--").to_string(),
        Token::CComment => raw
            .trim_start_matches("/*")
            .trim_end_matches("*/")
            .to_string(),
        _ => raw.to_string(),
    }
}

/// Find the index of the column whose ResTarget starts at-or-before the
/// given byte position. Saturates at 0 if the comment precedes every
/// ResTarget (e.g. a leading header comment).
fn column_for_position(locations: &[i32], pos: i32) -> usize {
    let mut idx = 0;
    for (i, loc) in locations.iter().enumerate() {
        if *loc <= pos {
            idx = i;
        } else {
            break;
        }
    }
    idx
}

/// Parse the attribute text following `@swell.`.
fn parse_attribute(attr: &str) -> Option<Override> {
    let attr = attr.trim();
    // `type=T` / `type: T`
    if let Some(rest) = attr
        .strip_prefix("type=")
        .or_else(|| attr.strip_prefix("type:"))
        .or_else(|| attr.strip_prefix("type "))
    {
        let ts = rest.trim();
        if ts.is_empty() {
            return None;
        }
        return Some(Override {
            clean_name: String::new(),
            force_nullable: None,
            force_ts_type: Some(ts.to_string()),
        });
    }
    let nullable = match attr {
        "nonnullable" | "nonnull" | "notnull" | "not_null" => Some(false),
        "nullable" => Some(true),
        _ => return None,
    };
    Some(Override {
        clean_name: String::new(),
        force_nullable: nullable,
        force_ts_type: None,
    })
}

/// Return the `location` of every ResTarget in the outermost statement's
/// output list (SELECT target_list or DML returning_list), in source
/// order. Order matches the column order in `RowDescription`.
fn output_column_locations(parsed: &protobuf::ParseResult) -> Vec<i32> {
    let Some(raw) = parsed.stmts.first() else { return Vec::new() };
    let Some(stmt) = raw.stmt.as_ref().and_then(|n| n.node.as_ref()) else { return Vec::new() };
    let targets: &[protobuf::Node] = match stmt {
        NodeBody::SelectStmt(s) => &s.target_list,
        NodeBody::InsertStmt(s) => &s.returning_list,
        NodeBody::UpdateStmt(s) => &s.returning_list,
        NodeBody::DeleteStmt(s) => &s.returning_list,
        NodeBody::MergeStmt(s) => &s.returning_list,
        _ => return Vec::new(),
    };
    targets.iter()
        .filter_map(|n| match n.node.as_ref()? {
            NodeBody::ResTarget(rt) => Some(rt.location),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_comment_force_not_null() {
        let h = extract("SELECT a --@swell.nonnullable\nFROM t");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].column_index, 0);
        assert_eq!(h[0].override_.force_nullable, Some(false));
    }

    #[test]
    fn block_comment_force_nullable() {
        let h = extract("SELECT a /*@swell.nullable*/ FROM t");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].column_index, 0);
        assert_eq!(h[0].override_.force_nullable, Some(true));
    }

    #[test]
    fn type_attribute() {
        let h = extract("SELECT a /*@swell.type=Payload*/ FROM t");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].override_.force_ts_type.as_deref(), Some("Payload"));
    }

    #[test]
    fn type_attribute_with_colon() {
        let h = extract("SELECT a /*@swell.type: Payload*/ FROM t");
        assert_eq!(h[0].override_.force_ts_type.as_deref(), Some("Payload"));
    }

    #[test]
    fn hint_attaches_to_preceding_column() {
        let h = extract(
            "SELECT a /*@swell.nonnullable*/, b /*@swell.nullable*/ FROM t",
        );
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].column_index, 0);
        assert_eq!(h[0].override_.force_nullable, Some(false));
        assert_eq!(h[1].column_index, 1);
        assert_eq!(h[1].override_.force_nullable, Some(true));
    }

    #[test]
    fn stacked_hints_on_one_column() {
        let h = extract(
            "SELECT a /*@swell.nonnullable*/ /*@swell.type=Payload*/ FROM t",
        );
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].column_index, 0);
        assert_eq!(h[1].column_index, 0);
    }

    #[test]
    fn ignores_non_swell_comments() {
        let h = extract("SELECT a -- regular note\nFROM t");
        assert!(h.is_empty());
    }

    #[test]
    fn ignores_marker_inside_string_literal() {
        // The tokenizer/parser already distinguishes string literals from
        // comments, so `--@swell.nonnullable` here is a literal, not a hint.
        let h = extract("SELECT '--@swell.nonnullable' FROM t");
        assert!(h.is_empty());
    }

    #[test]
    fn applies_to_returning_list() {
        let h = extract(
            "INSERT INTO t (x) VALUES (1) RETURNING id, x /*@swell.nonnullable*/",
        );
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].column_index, 1);
    }

    #[test]
    fn parse_failure_drops_silently() {
        // The analyzer will surface the real parse error from Postgres —
        // we don't need to.
        let h = extract("SELECT FROM /*@swell.nonnullable*/");
        assert!(h.is_empty());
    }

    #[test]
    fn unrelated_attribute_is_ignored() {
        let h = extract("SELECT a /*@swell.bogus*/ FROM t");
        assert!(h.is_empty());
    }

    #[test]
    fn header_comment_attaches_to_first_column() {
        // A leading hint above the SELECT list still has somewhere to
        // land — the first column. Documented but not the recommended
        // placement.
        let h = extract(
            "/*@swell.nonnullable*/ SELECT a FROM t",
        );
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].column_index, 0);
    }
}
