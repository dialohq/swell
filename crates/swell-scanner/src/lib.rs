//! TS/TSX source scanning.
//!
//! Walks files with `swc_ecma_parser` and finds `q("…")` call sites
//! whose `q` was imported from the swell runtime or any configured
//! re-export module (per-package `swell.generated.ts`, custom `./db`,
//! etc). The first argument must be a static string — anything dynamic
//! is silently skipped.

mod visit;

use serde::{Deserialize, Serialize};
use std::path::Path;
use swc_core::common::{sync::Lrc, FileName, Globals, SourceMap, GLOBALS};
use swc_core::ecma::ast::{EsVersion, Module};
use swc_core::ecma::parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedQuery {
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// The static SQL text. Stored as a single-element `Vec` for
    /// forward compatibility with future template-literal parts.
    pub static_parts: Vec<String>,
    /// The local-binding name `q` is imported under (always equal to
    /// `"q"` unless aliased via `import { q as foo }`).
    pub tag_local_name: String,
}

#[derive(Debug, Clone)]
pub struct ScanOptions<'a> {
    /// Modules (in addition to `"swell"`) that re-export `q`. Defaults
    /// cover the per-package codegen output (`./swell.generated`,
    /// `../swell.generated`, etc).
    pub q_modules: &'a [&'a str],
}

/// Scan a single TS/TSX file. Returns one `ScannedQuery` per call site.
pub fn scan_file(
    path: &Path,
    src: &str,
    opts: ScanOptions<'_>,
) -> anyhow::Result<Vec<ScannedQuery>> {
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

    let module: Module = GLOBALS
        .set(&Globals::new(), || parser.parse_module())
        .map_err(|e| anyhow::anyhow!("parse error in {}: {:?}", path.display(), e))?;

    let mut out = Vec::new();
    visit::collect(&module, &cm, path, &opts, &mut out);
    Ok(out)
}
