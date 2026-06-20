//! Tokenisers for Postgres EXPLAIN VERBOSE expression text.
//!
//! PG's planner emits `Output` expressions as semi-textual SQL with
//! synthetic refs (`(SubPlan 1)`, `"*VALUES*".column1`); we can't
//! always re-parse these through pg_query, so we keep tiny scanners
//! here. Shared between the join/refinement logic in `lib.rs` and the
//! per-expression classifier in `nullability.rs`.

pub enum Ref<'a> {
    Qualified { alias: &'a str, col: &'a str },
    Bare(&'a str),
}

/// True iff `s` is `(<balanced>)` with the very first `(` matched by
/// the very last `)`.
fn is_outer_paren_wrapper(s: &str) -> bool {
    s.starts_with('(') && s.ends_with(')')
        && find_matching_close(s.as_bytes(), 0) == Some(s.len() - 1)
}

/// Strip a single layer of outer balanced parens if present.
pub fn peel_outer_parens(s: &str) -> &str {
    let trimmed = s.trim();
    if is_outer_paren_wrapper(trimmed) {
        trimmed[1..trimmed.len() - 1].trim()
    } else {
        trimmed
    }
}

/// Repeatedly peel balanced outer parens. `(((a)))` → `a`.
pub fn peel_all_outer_parens(s: &str) -> &str {
    let mut cur = s.trim();
    loop {
        let next = peel_outer_parens(cur);
        if next.len() == cur.len() { return cur; }
        cur = next;
    }
}

/// Strip a trailing `::cast` suffix from `s` (leaves everything before
/// the first `::`). Trims surrounding whitespace.
pub fn strip_cast(s: &str) -> &str {
    s.split("::").next().unwrap_or(s).trim()
}

/// Find the byte index of the `)` matching `bytes[open]`, skipping
/// content inside single-quoted strings. None if unbalanced.
pub fn find_matching_close(bytes: &[u8], open: usize) -> Option<usize> {
    if bytes.get(open) != Some(&b'(') { return None; }
    let mut depth = 1;
    let mut in_string = false;
    let mut i = open + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') { i += 2; continue; }
                in_string = false;
            }
        } else {
            match b {
                b'\'' => in_string = true,
                b'(' => depth += 1,
                b')' => { depth -= 1; if depth == 0 { return Some(i); } }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Recognise a literal scalar (string / numeric / boolean) and render
/// it as a TS literal. Returns `None` for `NULL` and any non-literal
/// (column ref, function call, …). Peels outer parens and a single
/// `::cast`.
pub fn parse_literal_ts(expr: &str) -> Option<String> {
    let s = peel_all_outer_parens(expr);
    let value = strip_cast(s);
    if value.is_empty() { return None; }
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        let inner = value[1..value.len() - 1].replace("''", "'");
        return Some(format!("\"{}\"", inner.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    let lower = value.to_ascii_lowercase();
    if lower == "true" || lower == "false" { return Some(lower); }
    if value.parse::<f64>().is_ok() { return Some(value.to_string()); }
    None
}

/// True if `expr` is a non-null literal (string / numeric / boolean).
pub fn is_literal_non_null(expr: &str) -> bool {
    parse_literal_ts(expr).is_some()
}

/// Parse an EXPLAIN column reference, peeling `::cast` and a single
/// layer of parens. Returns `None` for arbitrary expressions.
/// Only accepts simple idents (alphanumeric + `_`) for the alias and
/// column — that's enough for the attnotnull lookup paths.
pub fn parse_ref(arg: &str) -> Option<Ref<'_>> {
    let s = strip_cast(arg);
    let s = s.trim_start_matches('(').trim_end_matches(')').trim();
    if let Some(dot) = s.find('.') {
        let alias = &s[..dot];
        let col = &s[dot + 1..];
        if is_simple_ident(alias) && is_simple_ident(col) {
            return Some(Ref::Qualified { alias, col });
        }
        return None;
    }
    if is_simple_ident(s) { Some(Ref::Bare(s)) } else { None }
}

/// Extract the alias prefix from an `<alias>.<col>` reference, handling
/// quoted synthetic aliases (`"*VALUES*"."column1"`). Returns `None`
/// when the prefix isn't a simple ident (and isn't quoted).
pub fn leading_alias(expr: &str) -> Option<&str> {
    let trimmed = expr.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let dot = trimmed.find('.')?;
    let prefix = &trimmed[..dot];
    if prefix.starts_with('"') && prefix.ends_with('"') && prefix.len() >= 2 {
        return Some(&prefix[1..prefix.len() - 1]);
    }
    if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(prefix)
    } else {
        None
    }
}

pub fn is_simple_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.chars().next().unwrap().is_ascii_digit()
}

/// Split `body` at top-level (depth-0) commas, skipping single-quoted
/// string contents.
pub fn split_top_level_args(body: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            cur.push(b as char);
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    cur.push('\''); i += 2; continue;
                }
                in_string = false;
            }
        } else {
            match b {
                b'\'' => { in_string = true; cur.push('\''); }
                b'(' => { depth += 1; cur.push('('); }
                b')' => { depth -= 1; cur.push(')'); }
                b',' if depth == 0 => {
                    args.push(cur.trim().to_string());
                    cur.clear();
                }
                c => cur.push(c as char),
            }
        }
        i += 1;
    }
    if !cur.trim().is_empty() { args.push(cur.trim().to_string()); }
    args
}

/// `head(arg, arg, ...)` → Some(args). Peels outer parens. Match on
/// `head` is case-insensitive (EXPLAIN normalises `COALESCE` to
/// lowercase, but be defensive).
pub fn parse_call_args(expr: &str, head: &str) -> Option<Vec<String>> {
    let s = peel_all_outer_parens(expr);
    let needle = format!("{}(", head);
    let lower = s.to_ascii_lowercase();
    if !lower.starts_with(&needle) { return None; }
    let open = head.len();
    let close = find_matching_close(s.as_bytes(), open)?;
    Some(split_top_level_args(&s[open + 1..close]))
}

/// `users_1` → `users`. PG appends `_N` to disambiguate duplicate scan
/// aliases within a plan.
pub fn strip_suffix_digits(s: &str) -> &str {
    let t = s.trim_end_matches(|c: char| c.is_ascii_digit());
    t.trim_end_matches('_')
}
