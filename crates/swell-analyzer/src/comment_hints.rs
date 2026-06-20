//! `--@swell.<attr>` / `/*@swell.<attr>*/` per-column overrides.
//!
//! Each hint attaches to the SELECT-target (or RETURNING-target) whose
//! `ResTarget.location` is the largest position less-or-equal to the
//! comment's start. Multiple hints stack on one column.

use crate::pg_util::select_stmts;
use pg_query::protobuf::{node::Node as NB, Token};

/// Per-column override produced from one `--@swell.<attr>` directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
    /// `nullable` → false, `nonnullable` / `nonnull` / `notnull` → true.
    ForceNotNull(bool),
    /// `type=T` / `type: T` — verbatim TS type.
    Type(String),
}

/// Hints per output column, indexed by position in the target / returning
/// list. Outer `None` means we couldn't classify the statement; inner
/// empty vec means no hints for that column.
pub fn collect(sql: &str, n_outputs: usize) -> Option<Vec<Vec<Hint>>> {
    let targets = target_list_starts(sql)?;
    if targets.is_empty() {
        return None;
    }
    let mut out: Vec<Vec<Hint>> = (0..n_outputs).map(|_| Vec::new()).collect();
    for (start, end) in scan_comments(sql) {
        let Some(text) = sql.get(start..end) else {
            continue;
        };
        let body = strip_comment(text);
        let Some(rest) = body.trim_start().strip_prefix("@swell.") else {
            continue;
        };
        let Some(idx) = column_for(&targets, start, n_outputs) else {
            continue;
        };
        for piece in rest.split(',') {
            if let Some(h) = parse_attr(piece.trim()) {
                out[idx].push(h);
            }
        }
    }
    Some(out)
}

/// Byte offsets of every comment token. SqlComment covers `-- …`,
/// CComment covers `/* … */`.
fn scan_comments(sql: &str) -> Vec<(usize, usize)> {
    pg_query::scan(sql)
        .ok()
        .map(|sr| {
            sr.tokens
                .iter()
                .filter(|t| {
                    matches!(
                        Token::try_from(t.token),
                        Ok(Token::SqlComment | Token::CComment)
                    )
                })
                .map(|t| (t.start as usize, t.end as usize))
                .collect()
        })
        .unwrap_or_default()
}

/// `ResTarget.location` for every entry in the first SELECT's target
/// list or the first DML's returning list, in column order.
fn target_list_starts(sql: &str) -> Option<Vec<i32>> {
    let parsed = pg_query::parse(sql).ok()?;
    // Try a SELECT first.
    if let Some(s) = select_stmts(&parsed.protobuf).next() {
        return Some(restarget_locations(&s.target_list));
    }
    // Then a DML with RETURNING.
    let raw = parsed.protobuf.stmts.into_iter().next()?;
    let node = raw.stmt?.node?;
    let returning = match node {
        NB::InsertStmt(ins) => ins.returning_list,
        NB::UpdateStmt(upd) => upd.returning_list,
        NB::DeleteStmt(del) => del.returning_list,
        _ => return None,
    };
    Some(restarget_locations(&returning))
}

fn restarget_locations(targets: &[pg_query::protobuf::Node]) -> Vec<i32> {
    targets
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            NB::ResTarget(rt) => Some(rt.location),
            _ => None,
        })
        .collect()
}

/// Index of the target whose `location` is the largest one less-or-equal
/// to `pos`. `None` when `pos` precedes every target.
fn column_for(targets: &[i32], pos: usize, n_outputs: usize) -> Option<usize> {
    let mut chosen: Option<(usize, i32)> = None;
    for (i, &loc) in targets.iter().enumerate() {
        if loc < 0 || loc as usize > pos {
            continue;
        }
        if chosen.is_none_or(|(_, prev)| loc > prev) {
            chosen = Some((i, loc));
        }
    }
    chosen.map(|(i, _)| i).filter(|&i| i < n_outputs)
}

/// `--<body>` → `<body>` ; `/*<body>*/` → `<body>`.
fn strip_comment(token: &str) -> &str {
    if let Some(rest) = token.strip_prefix("--") {
        return rest.trim_end_matches(|c: char| c == '\r' || c == '\n');
    }
    if let Some(rest) = token.strip_prefix("/*") {
        return rest.strip_suffix("*/").unwrap_or(rest);
    }
    token
}

fn parse_attr(s: &str) -> Option<Hint> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((k, v)) = s.split_once('=').or_else(|| s.split_once(':')) {
        if k.trim().eq_ignore_ascii_case("type") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(Hint::Type(v.to_string()));
            }
        }
        return None;
    }
    match s.to_ascii_lowercase().as_str() {
        "nullable" => Some(Hint::ForceNotNull(false)),
        "nonnullable" | "nonnull" | "notnull" => Some(Hint::ForceNotNull(true)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(sql: &str, n: usize) -> Vec<Vec<Hint>> {
        collect(sql, n).expect("classified statement")
    }

    #[test]
    fn line_comment_after_column() {
        let v = h("SELECT a, b --@swell.notnull\nFROM t", 2);
        assert_eq!(v[0], vec![]);
        assert_eq!(v[1], vec![Hint::ForceNotNull(true)]);
    }

    #[test]
    fn block_comment_after_column() {
        let v = h("SELECT a /*@swell.nullable*/ FROM t", 1);
        assert_eq!(v[0], vec![Hint::ForceNotNull(false)]);
    }

    #[test]
    fn type_override() {
        let v = h("SELECT a /*@swell.type=Foo*/ FROM t", 1);
        assert_eq!(v[0], vec![Hint::Type("Foo".into())]);
    }

    #[test]
    fn type_override_colon_form() {
        let v = h("SELECT a /*@swell.type: Bar*/ FROM t", 1);
        assert_eq!(v[0], vec![Hint::Type("Bar".into())]);
    }

    #[test]
    fn stacked_hints_one_column() {
        let v = h("SELECT a /*@swell.notnull*/ /*@swell.type=Foo*/ FROM t", 1);
        assert_eq!(
            v[0],
            vec![Hint::ForceNotNull(true), Hint::Type("Foo".into())]
        );
    }

    #[test]
    fn comma_separated_attrs() {
        let v = h("SELECT a /*@swell.notnull, type=Foo*/ FROM t", 1);
        assert_eq!(
            v[0],
            vec![Hint::ForceNotNull(true), Hint::Type("Foo".into())]
        );
    }

    #[test]
    fn hint_after_a_does_not_touch_b() {
        let v = h("SELECT a --@swell.notnull\n, b FROM t", 2);
        assert_eq!(v[0], vec![Hint::ForceNotNull(true)]);
        assert_eq!(v[1], vec![]);
    }

    #[test]
    fn hint_in_string_literal_is_ignored() {
        let v = h("SELECT '--@swell.notnull' AS a FROM t", 1);
        assert_eq!(v[0], vec![]);
    }

    #[test]
    fn returning_list_attaches() {
        let v = h(
            "INSERT INTO t (a) VALUES ($1) RETURNING a /*@swell.notnull*/",
            1,
        );
        assert_eq!(v[0], vec![Hint::ForceNotNull(true)]);
    }

    #[test]
    fn parse_failure_returns_none() {
        assert!(collect("SELECT FROM WHERE", 0).is_none());
    }

    #[test]
    fn comment_before_first_column_is_ignored() {
        let v = h("SELECT /*@swell.notnull*/ a FROM t", 1);
        assert_eq!(v[0], vec![]);
    }
}
