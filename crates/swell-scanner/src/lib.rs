//! TS/TSX source scanning.
//!
//! Walks files with `swc_ecma_parser`, locates `sql\`...\`` tagged-template
//! call sites whose tag identifier was imported from the configured runtime
//! module, and extracts:
//!   - the static template parts (the registry key)
//!   - the source location for diagnostics
//!
//! What "imported from the configured runtime module" means in practice:
//! we scan the file's `ImportDeclaration` nodes for any specifier whose
//! source matches `runtime_module` and whose imported name matches
//! `runtime_export` (or whose default-import name is `runtime_export`). The
//! local binding name is whatever alias the user chose. Tagged templates
//! whose tag identifier matches that local binding are emitted.

mod visit;

use serde::{Deserialize, Serialize};
use std::path::Path;
use swc_core::common::{sync::Lrc, FileName, SourceMap, GLOBALS, Globals};
use swc_core::ecma::ast::{EsVersion, Module};
use swc_core::ecma::parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedQuery {
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// Static template parts. Number of params = parts.len() - 1.
    /// For a non-parameterised query like `sql\`SELECT 1\``, this has one
    /// element.
    pub static_parts: Vec<String>,
    /// The local-binding name of the sql tag at this call site (always
    /// equal to `runtime_export` unless the user aliased the import).
    pub tag_local_name: String,
}

#[derive(Debug, Clone)]
pub struct ScanOptions<'a> {
    /// Module specifiers from which `sql` (or `createSql`) is imported. Covers
    /// both the codegen output (`./swell.generated`) and any per-package
    /// re-export modules like `./db`, `../db`, etc.
    pub db_modules: &'a [&'a str],
    /// Named exports from `db_modules` that bind a TypedSql instance. Default
    /// `["sql"]` — extend if a package keeps multiple typed handles.
    pub db_exports: &'a [&'a str],
}

/// Scan a single TS/TSX file. Returns one `ScannedQuery` per call site.
pub fn scan_file(path: &Path, src: &str, opts: ScanOptions<'_>) -> anyhow::Result<Vec<ScannedQuery>> {
    let cm: Lrc<SourceMap> = Default::default();
    let file = cm.new_source_file(
        Lrc::new(FileName::Real(path.to_path_buf())),
        src.to_string(),
    );

    let is_tsx = path.extension().map(|e| e == "tsx").unwrap_or(false);
    let syntax = Syntax::Typescript(TsSyntax {
        tsx: is_tsx,
        decorators: true,
        ..Default::default()
    });
    let lexer = Lexer::new(syntax, EsVersion::EsNext, StringInput::from(&*file), None);
    let mut parser = Parser::new_from(lexer);

    let module: Module = GLOBALS.set(&Globals::new(), || parser.parse_module())
        .map_err(|e| anyhow::anyhow!("parse error in {}: {:?}", path.display(), e))?;

    let mut out = Vec::new();
    visit::collect(&module, &cm, path, &opts, &mut out);
    Ok(out)
}
