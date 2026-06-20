//! Find `q("…")` calls whose `q` was imported from a tracked module
//! (`@dialo/swell` or a re-export). First arg must be a string literal
//! or template literal with no interpolation; dynamic args are skipped.

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
    let mut binders = ImportBinders { opts, locals: HashSet::new() };
    module.visit_with(&mut binders);

    if binders.locals.is_empty() {
        return;
    }

    let mut collector = CallCollector { cm, path, locals: &binders.locals, out };
    module.visit_with(&mut collector);
}

struct ImportBinders<'a> {
    opts: &'a ScanOptions<'a>,
    /// Local-alias names that resolve to the `q` SQL-marker function.
    locals: HashSet<String>,
}

impl<'a> Visit for ImportBinders<'a> {
    fn visit_import_decl(&mut self, n: &ImportDecl) {
        let raw = String::from_utf8_lossy(n.src.value.as_bytes());
        let from_q_source = raw == "@dialo/swell"
            || self.opts.q_modules.iter().any(|m| *m == raw.as_ref());
        if !from_q_source { return; }
        for spec in &n.specifiers {
            let ImportSpecifier::Named(named) = spec else { continue };
            let imported = match &named.imported {
                Some(ModuleExportName::Ident(id)) => id.sym.to_string(),
                Some(ModuleExportName::Str(s)) =>
                    String::from_utf8_lossy(s.value.as_bytes()).into_owned(),
                None => named.local.sym.to_string(),
            };
            if imported == "q" {
                self.locals.insert(named.local.sym.to_string());
            }
        }
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
            if let Expr::Ident(id) = &**callee {
                let name = id.sym.to_string();
                if self.locals.contains(&name) {
                    if let Some(sql_text) = first_arg_as_static_string(n.args.first()) {
                        let loc = self.cm.lookup_char_pos(n.span.lo);
                        self.out.push(ScannedQuery {
                            file: self.path.to_string_lossy().to_string(),
                            line: loc.line as u32,
                            col: (loc.col_display + 1) as u32,
                            static_parts: vec![sql_text],
                            tag_local_name: name,
                        });
                    }
                }
            }
        }
        n.visit_children_with(self);
    }
}

fn first_arg_as_static_string(arg: Option<&ExprOrSpread>) -> Option<String> {
    let arg = arg?;
    if arg.spread.is_some() { return None; }
    match &*arg.expr {
        Expr::Lit(Lit::Str(s)) => Some(String::from_utf8_lossy(s.value.as_bytes()).into_owned()),
        Expr::Tpl(t) if t.exprs.is_empty() && t.quasis.len() == 1 => {
            let q = &t.quasis[0];
            let bytes = match q.cooked.as_ref() {
                Some(c) => c.as_bytes(),
                None => q.raw.as_bytes(),
            };
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        _ => None,
    }
}
