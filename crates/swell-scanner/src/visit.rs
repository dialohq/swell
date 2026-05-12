//! AST walker that finds `sql(...)` call expressions whose tag identifier
//! was imported from the configured runtime module.
//!
//! For each match the first argument must be a string literal (or a template
//! literal with no interpolation). Anything else (concatenation, variable
//! reference) gets silently skipped — that's a runtime-only query the daemon
//! can't analyse without dynamic context.

use crate::{ScanOptions, ScannedQuery};
use std::collections::HashSet;
use std::path::Path;
use swc_core::common::SourceMap;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

pub(crate) fn collect(
    module: &Module,
    cm: &SourceMap,
    path: &Path,
    opts: &ScanOptions<'_>,
    out: &mut Vec<ScannedQuery>,
) {
    let mut binders = ImportBinders::new(opts);
    module.visit_with(&mut binders);

    // Second pass: extend the locals with `const x = createSql(...)` bindings.
    // The `createSql` name itself comes from a tracked module, so its local
    // alias is already known after the first pass.
    if !binders.factory_locals.is_empty() {
        let mut factory = FactoryBinders {
            factory_locals: &binders.factory_locals,
            sql_locals: &mut binders.locals,
        };
        module.visit_with(&mut factory);
    }

    if binders.locals.is_empty() {
        return;
    }

    let mut collector = CallCollector {
        cm,
        path,
        locals: &binders.locals,
        out,
    };
    module.visit_with(&mut collector);
}

struct ImportBinders<'a> {
    opts: &'a ScanOptions<'a>,
    /// Names that resolve to a TypedSql instance (`sql` imported directly,
    /// or one produced by a `createSql(...)` call we discover in a second pass).
    locals: HashSet<String>,
    /// Local aliases for `createSql` imported from a tracked module. Used to
    /// find `const x = createSql(...)` declarations that produce a new sql
    /// instance in the same file.
    factory_locals: HashSet<String>,
}

impl<'a> ImportBinders<'a> {
    fn new(opts: &'a ScanOptions<'a>) -> Self {
        Self {
            opts,
            locals: HashSet::new(),
            factory_locals: HashSet::new(),
        }
    }
}

impl<'a> Visit for ImportBinders<'a> {
    fn visit_import_decl(&mut self, n: &ImportDecl) {
        let raw = String::from_utf8_lossy(n.src.value.as_bytes());
        let matches = self.opts.db_modules.iter().any(|m| *m == raw.as_ref());
        if !matches {
            return;
        }
        for spec in &n.specifiers {
            match spec {
                ImportSpecifier::Named(named) => {
                    let imported_name = match &named.imported {
                        Some(ModuleExportName::Ident(id)) => id.sym.to_string(),
                        Some(ModuleExportName::Str(s)) => String::from_utf8_lossy(s.value.as_bytes()).into_owned(),
                        None => named.local.sym.to_string(),
                    };
                    // Pre-bound typed-sql handles arrive under one of the
                    // configured export names (default `["sql"]`).
                    if self.opts.db_exports.iter().any(|e| *e == imported_name.as_str()) {
                        self.locals.insert(named.local.sym.to_string());
                    }
                    // `import { createSql } from "./swell.generated"` — track
                    // the local alias so we can recognise
                    // `const X = createSql(...)` bindings in the same file.
                    if imported_name == "createSql" {
                        self.factory_locals.insert(named.local.sym.to_string());
                    }
                }
                ImportSpecifier::Default(_) => {
                    // Default imports of `sql` aren't standard; ignore.
                }
                ImportSpecifier::Namespace(_) => {
                    // `import * as M from "./db"` — calls would be `M.sql(...)`;
                    // not currently supported. Skip silently.
                }
            }
        }
    }
}

/// Walks variable declarations and pulls `const x = createSql(...)` (or any
/// `const x = <factory_local>(...)`) into the set of tracked sql locals.
/// `const [a, b] = ...` and patterns are ignored — only plain identifier
/// bindings count.
struct FactoryBinders<'a> {
    factory_locals: &'a HashSet<String>,
    sql_locals: &'a mut HashSet<String>,
}

impl<'a> Visit for FactoryBinders<'a> {
    fn visit_var_declarator(&mut self, n: &VarDeclarator) {
        let init = match &n.init {
            Some(e) => e,
            None => return,
        };
        // Strip `await` / `(...)` wrappers around the call.
        let call = match unwrap_to_call(init) {
            Some(c) => c,
            None => return,
        };
        // Callee must be a plain identifier we recognise as a factory.
        let callee_name = match &call.callee {
            Callee::Expr(e) => match &**e {
                Expr::Ident(id) => id.sym.to_string(),
                _ => return,
            },
            _ => return,
        };
        if !self.factory_locals.contains(&callee_name) {
            return;
        }
        // Binding must be a plain identifier.
        if let Pat::Ident(id) = &n.name {
            self.sql_locals.insert(id.id.sym.to_string());
        }
    }
}

fn unwrap_to_call(e: &Expr) -> Option<&CallExpr> {
    match e {
        Expr::Call(c) => Some(c),
        Expr::Paren(p) => unwrap_to_call(&p.expr),
        Expr::Await(a) => unwrap_to_call(&a.arg),
        Expr::TsAs(c) => unwrap_to_call(&c.expr),
        Expr::TsNonNull(n) => unwrap_to_call(&n.expr),
        Expr::TsTypeAssertion(c) => unwrap_to_call(&c.expr),
        _ => None,
    }
}

struct CallCollector<'a> {
    cm: &'a SourceMap,
    path: &'a Path,
    locals: &'a HashSet<String>,
    out: &'a mut Vec<ScannedQuery>,
}

impl<'a> Visit for CallCollector<'a> {
    fn visit_call_expr(&mut self, n: &CallExpr) {
        if let Callee::Expr(callee) = &n.callee {
            // Three callee shapes carry queries:
            //   sql("...", ...)        — Expr::Ident
            //   sql.many("...", ...)   — Expr::Member with row-cardinality method
            //   sql.begin(async (tx) => { tx.many("..."); })
            //                          — recurse into the callback with `tx`
            //                            also tracked as a sql local.
            if let Some((local_name, method)) = tracked_callee(callee, self.locals) {
                match method.as_deref() {
                    Some("begin") | Some("savepoint") => {
                        // The callback is either the only arg (begin) or the
                        // second (savepoint). Pick whichever arg looks like a
                        // function whose first param is the transaction sql.
                        if let Some(tx_local) = find_tx_callback_param(&n.args) {
                            // Recurse into the callback body with `tx_local`
                            // added to the tracked-locals set.
                            let mut extended = self.locals.clone();
                            extended.insert(tx_local);
                            let mut inner = CallCollector {
                                cm: self.cm,
                                path: self.path,
                                locals: &extended,
                                out: self.out,
                            };
                            n.visit_children_with(&mut inner);
                            return;
                        }
                    }
                    _ => {
                        if let Some(sql_text) = first_arg_as_static_string(n.args.first()) {
                            let loc = self.cm.lookup_char_pos(n.span.lo);
                            self.out.push(ScannedQuery {
                                file: self.path.to_string_lossy().to_string(),
                                line: loc.line as u32,
                                col: (loc.col_display + 1) as u32,
                                static_parts: vec![sql_text],
                                tag_local_name: local_name,
                            });
                        }
                    }
                }
            }
        }
        n.visit_children_with(self);
    }
}

/// `sql.begin(async (tx) => ...)` or `sql.savepoint("name", async (tx) => ...)`.
/// Find the function-typed argument and return the local name bound to its
/// first parameter — that's the transaction-scoped sql instance.
fn find_tx_callback_param(args: &[ExprOrSpread]) -> Option<String> {
    for arg in args {
        if arg.spread.is_some() {
            continue;
        }
        let fn_expr = match &*arg.expr {
            Expr::Arrow(a) => return first_param_ident(&a.params),
            Expr::Fn(f) => f.function.params.iter().map(|p| &p.pat).next().and_then(pat_to_ident),
            _ => continue,
        };
        if let Some(name) = fn_expr {
            return Some(name);
        }
    }
    None
}

fn first_param_ident(pats: &[Pat]) -> Option<String> {
    pats.first().and_then(pat_to_ident)
}

fn pat_to_ident(p: &Pat) -> Option<String> {
    match p {
        Pat::Ident(id) => Some(id.id.sym.to_string()),
        _ => None,
    }
}

/// Method names on TypedSql that take SQL as their first argument.
const SQL_QUERY_METHODS: &[&str] = &["many", "one", "maybe", "exec", "unsafe", "cursor"];

/// Transaction-opening methods. The first function-typed argument's first
/// param is a TypedSql scoped to the transaction; we recognise it so calls
/// inside the callback body get the same scanner coverage.
const SQL_TX_METHODS: &[&str] = &["begin", "savepoint"];

/// Recognise the two callee shapes that carry queries:
///   `sql("...", ...)`       → tracked ident
///   `sql.many("...", ...)`  → member access on a tracked ident, using a
///                              row-cardinality method
/// Returns the local name and the method (None for the bare-call form).
fn tracked_callee(expr: &Expr, locals: &HashSet<String>) -> Option<(String, Option<String>)> {
    match expr {
        Expr::Ident(id) => {
            let name = id.sym.to_string();
            locals.contains(name.as_str()).then_some((name, None))
        }
        Expr::Member(m) => {
            let obj_name = match &*m.obj {
                Expr::Ident(id) => id.sym.to_string(),
                _ => return None,
            };
            if !locals.contains(obj_name.as_str()) {
                return None;
            }
            let method = match &m.prop {
                MemberProp::Ident(id) => id.sym.to_string(),
                _ => return None,
            };
            let is_query = SQL_QUERY_METHODS.iter().any(|m| *m == method.as_str());
            let is_tx = SQL_TX_METHODS.iter().any(|m| *m == method.as_str());
            if is_query || is_tx {
                Some((obj_name, Some(method)))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Pull a static string out of a function argument. Accepts both string
/// literals (`"..."`, `'...'`) and template literals with no interpolation
/// (`` `...` ``). Anything else returns None.
fn first_arg_as_static_string(arg: Option<&ExprOrSpread>) -> Option<String> {
    let arg = arg?;
    if arg.spread.is_some() { return None; } // `sql(...spread, ...)` → can't analyse
    match &*arg.expr {
        Expr::Lit(Lit::Str(s)) => Some(String::from_utf8_lossy(s.value.as_bytes()).into_owned()),
        Expr::Tpl(t) if t.exprs.is_empty() && t.quasis.len() == 1 => {
            let q = &t.quasis[0];
            let s = if let Some(c) = q.cooked.as_ref() {
                String::from_utf8_lossy(c.as_bytes()).into_owned()
            } else {
                String::from_utf8_lossy(q.raw.as_bytes()).into_owned()
            };
            Some(s)
        }
        _ => None,
    }
}
