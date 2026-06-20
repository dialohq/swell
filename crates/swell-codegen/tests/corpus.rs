//! End-to-end test corpus driven by markdown files.
//!
//! Each `.md` file under `tests/corpus/` is a **suite** of tests that
//! share a schema. Top-level structure:
//!
//!   # [Setup](./path.sql)   or   # Setup       (with inline ```sql)
//!   # Common types
//!   # Tests
//!     ## <test name>
//!       ```sql ... ```
//!       ```ts  ... ```      (compact form: `$N: type` + `result: …`)
//!
//! The setup SQL is applied once before any test in the file. The
//! `# Common types` block is the rendered table interfaces — the
//! runner asserts it matches and rewrites it on `CORPUS_PROMOTE=1`.
//! Each `## <test>` block has one SQL fence (the query) and one
//! result fence in the compact form documented in
//! `swell_codegen::render_query_compact`.
//!
//! For error-expectation tests, the result fence reads
//!   `error: <substring>`
//! and the runner asserts that `analyze` fails with that substring in
//! the rendered error.
//!
//! Markdown parsing uses pulldown-cmark (CommonMark). The header link
//! form `# [Setup](./_setup.sql)` is the standard markdown way to
//! reference an external file — the runner resolves it relative to
//! the .md.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};
use similar::{ChangeTag, TextDiff};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use swell_analyzer::{Analyzer, AnalyzerOptions};
use swell_codegen::{render_query_compact, render_table_interfaces};

fn promote_enabled() -> bool {
    matches!(
        std::env::var("CORPUS_PROMOTE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[tokio::test(flavor = "current_thread")]
async fn corpus() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    assert!(root.is_dir(), "corpus dir missing at {}", root.display());

    let files = sorted_files_with_ext(&root, "md");
    assert!(!files.is_empty(), "no .md suites under {}", root.display());

    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at a dev Postgres");

    let mut failures: Vec<String> = Vec::new();
    let mut promotions: Vec<PathBuf> = Vec::new();

    for path in files {
        match run_suite(&path, &url).await {
            Ok(SuiteOutcome::Pass) => {
                println!("ok       {}", path.file_name().unwrap().to_string_lossy());
            }
            Ok(SuiteOutcome::Promoted) => {
                println!("promoted {}", path.file_name().unwrap().to_string_lossy());
                promotions.push(path);
            }
            Err(e) => {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                println!("FAIL     {name}");
                failures.push(format!("{name}\n{e}"));
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "\n{} suite(s) failed:\n\n{}",
            failures.len(),
            failures.join("\n\n---\n\n"),
        );
    }
}

enum SuiteOutcome {
    Pass,
    Promoted,
}

async fn run_suite(path: &Path, url: &str) -> Result<SuiteOutcome, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut suite = parse_suite(&text, path)?;

    apply_sql(url, &suite.setup_sql).await
        .map_err(|e| format!("apply setup: {e}"))?;

    let an = Analyzer::connect(AnalyzerOptions {
        database_url: url.to_string(),
        schemas: suite.schemas.clone(),
        type_overrides: BTreeMap::new(),
    })
    .await
    .map_err(|e| format!("connect: {e}"))?;

    // Run every test once. Errors are captured (not propagated) so a
    // single failure doesn't stop the rest of the suite from getting
    // checked / promoted.
    let mut analyzed: Vec<Option<swell_analyzer::InferredQuery>> = vec![None; suite.tests.len()];
    let mut error_text: Vec<Option<String>> = vec![None; suite.tests.len()];
    for (i, test) in suite.tests.iter().enumerate() {
        match an.analyze(&test.sql).await {
            Ok(q) => analyzed[i] = Some(q),
            Err(e) => error_text[i] = Some(format!("{e:#}")),
        }
    }

    // Tables referenced by any successful query.
    let mut pair_set: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    for q in analyzed.iter().flatten() {
        for c in &q.columns {
            if let Some(r) = &c.table_ref {
                pair_set.insert((r.schema.clone(), r.table.clone()));
            }
        }
        for p in &q.params {
            if let Some(r) = &p.table_ref {
                pair_set.insert((r.schema.clone(), r.table.clone()));
            }
        }
    }
    let pairs: Vec<(String, String)> = pair_set.into_iter().collect();
    let tables = an.table_schemas(&pairs).await
        .map_err(|e| format!("table_schemas: {e}"))?;

    let actual_common = render_table_interfaces(&tables);
    let mut actual_per_test: Vec<String> = vec![String::new(); suite.tests.len()];
    for (i, q) in analyzed.iter().enumerate() {
        actual_per_test[i] = match (q, &error_text[i]) {
            (Some(q), _) => render_query_compact(q, &tables),
            (None, Some(msg)) => format!("error: {msg}\n"),
            (None, None) => String::new(),
        };
    }

    // Build diff report; figure out what would be promoted.
    let mut diffs: Vec<String> = Vec::new();
    if !blocks_match(&actual_common, &suite.common_types) {
        diffs.push(format!(
            "# Common types\n\n{}",
            diff_unified(&suite.common_types, &actual_common),
        ));
    }
    for (i, test) in suite.tests.iter().enumerate() {
        if test.expected_is_error() {
            let needle = test.expected
                .trim()
                .strip_prefix("error:")
                .unwrap_or("")
                .trim();
            let actual_err = error_text[i].as_deref().unwrap_or("");
            if actual_err.is_empty() {
                diffs.push(format!(
                    "## {}\n\nexpected analyze to fail with substring {needle:?}, but it succeeded\n",
                    test.name,
                ));
            } else if !actual_err.contains(needle) {
                diffs.push(format!(
                    "## {}\n\nexpected error substring: {needle:?}\nactual error:\n  {actual_err}\n",
                    test.name,
                ));
            }
            continue;
        }
        if !blocks_match(&actual_per_test[i], &test.expected) {
            diffs.push(format!(
                "## {}\n\n{}",
                test.name,
                diff_unified(&test.expected, &actual_per_test[i]),
            ));
        }
    }

    if diffs.is_empty() {
        return Ok(SuiteOutcome::Pass);
    }
    if promote_enabled() {
        suite.common_types = actual_common;
        for (i, t) in suite.tests.iter_mut().enumerate() {
            if !t.expected_is_error() {
                t.expected = actual_per_test[i].clone();
            }
        }
        let rewritten = rewrite_suite(&text, &suite);
        std::fs::write(path, rewritten)
            .map_err(|e| format!("write for promote: {e}"))?;
        return Ok(SuiteOutcome::Promoted);
    }
    Err(format!(
        "{} section(s) mismatch:\n\n{}\n\n(run with CORPUS_PROMOTE=1 to update the suite)",
        diffs.len(),
        diffs.join("\n---\n"),
    ))
}

fn blocks_match(actual: &str, expected: &str) -> bool {
    actual.trim_end() == expected.trim_end()
}

// -------------------- markdown parsing --------------------

struct Suite {
    setup_sql: String,
    schemas: Vec<String>,
    common_types: String,
    tests: Vec<TestCase>,
}

struct TestCase {
    name: String,
    sql: String,
    expected: String,
}

impl TestCase {
    fn expected_is_error(&self) -> bool {
        self.expected.trim_start().starts_with("error:")
    }
}

#[derive(Default)]
struct PartialTest {
    name: Option<String>,
    sqls: Vec<String>,
    expecteds: Vec<String>,
}

impl PartialTest {
    fn flush_into(&mut self, into: &mut Vec<TestCase>) {
        if let Some(name) = self.name.take() {
            let sql = self.sqls.first().cloned().unwrap_or_default().trim().to_string();
            let expected = self.expecteds.first().cloned().unwrap_or_default();
            into.push(TestCase { name, sql, expected });
        }
        self.sqls.clear();
        self.expecteds.clear();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section { None, Setup, CommonTypes, Tests }

/// Parse the suite via pulldown-cmark events. Headings drive section
/// transitions; fenced code blocks attach to whichever section / test
/// is currently active.
fn parse_suite(text: &str, path: &Path) -> Result<Suite, String> {
    let parser = Parser::new(text);
    let mut section = Section::None;
    let mut setup_link: Option<String> = None;
    let mut setup_inline: Vec<String> = Vec::new();
    let mut common_types: Vec<String> = Vec::new();
    let mut tests: Vec<TestCase> = Vec::new();
    let mut cur = PartialTest::default();

    let mut h1_link: Option<String> = None;
    let mut h1_text = String::new();
    let mut h2_text = String::new();
    let mut in_h1 = false;
    let mut in_h2 = false;
    let mut code_lang: Option<String> = None;
    let mut code_body = String::new();

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level: HeadingLevel::H1, .. }) => {
                cur.flush_into(&mut tests);
                in_h1 = true;
                h1_text.clear();
                h1_link = None;
            }
            Event::End(TagEnd::Heading(HeadingLevel::H1)) => {
                in_h1 = false;
                let title = h1_text.trim().to_lowercase();
                section = if title.starts_with("setup") {
                    if let Some(href) = h1_link.take() {
                        setup_link = Some(href);
                    }
                    Section::Setup
                } else if title.starts_with("common types") {
                    Section::CommonTypes
                } else if title.starts_with("tests") {
                    Section::Tests
                } else {
                    Section::None
                };
            }
            Event::Start(Tag::Heading { level: HeadingLevel::H2, .. }) if section == Section::Tests => {
                cur.flush_into(&mut tests);
                in_h2 = true;
                h2_text.clear();
            }
            Event::End(TagEnd::Heading(HeadingLevel::H2)) if section == Section::Tests => {
                in_h2 = false;
                cur.name = Some(h2_text.trim().to_string());
            }
            Event::Start(Tag::Link { dest_url, .. }) if in_h1 => {
                h1_link = Some(dest_url.to_string());
            }
            Event::Text(t) if in_h1 => h1_text.push_str(&t),
            Event::Text(t) if in_h2 => h2_text.push_str(&t),
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) => {
                code_lang = Some(lang.to_string());
                code_body.clear();
            }
            Event::Text(t) if code_lang.is_some() => code_body.push_str(&t),
            Event::End(TagEnd::CodeBlock) => {
                let lang = code_lang.take().unwrap_or_default();
                let body = std::mem::take(&mut code_body);
                match (section, lang.as_str()) {
                    (Section::Setup, "sql") => setup_inline.push(body),
                    (Section::CommonTypes, "ts" | "typescript") => common_types.push(body),
                    (Section::Tests, "sql") => cur.sqls.push(body),
                    (Section::Tests, "ts" | "typescript") => cur.expecteds.push(body),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    cur.flush_into(&mut tests);

    let setup_sql = if let Some(href) = setup_link {
        let resolved = path.parent().unwrap().join(&href);
        std::fs::read_to_string(&resolved)
            .map_err(|e| format!("read linked setup {}: {e}", resolved.display()))?
    } else {
        setup_inline.join("\n")
    };
    let common_types = common_types.join("");
    let schemas = read_schema_list(path.parent().unwrap())
        .unwrap_or_else(|| vec!["public".into()]);
    Ok(Suite { setup_sql, schemas, common_types, tests })
}

fn read_schema_list(dir: &Path) -> Option<Vec<String>> {
    let path = dir.join("_schemas.txt");
    let text = std::fs::read_to_string(path).ok()?;
    let names: Vec<String> = text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

fn sorted_files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some(ext))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

async fn apply_sql(url: &str, sql: &str) -> Result<(), String> {
    if sql.trim().is_empty() {
        return Ok(());
    }
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls).await
        .map_err(|e| format!("connect: {e}"))?;
    let h = tokio::spawn(async move {
        let _ = conn.await;
    });
    let res = client.batch_execute(sql).await
        .map_err(|e| format!("batch_execute: {e}"));
    drop(client);
    let _ = h.await;
    res
}

// -------------------- promotion rewrite --------------------

/// Walk the markdown line-by-line, replacing the body of each `ts`
/// fence under a recognised section. Keeps every byte outside those
/// fences — prose, headings, sql blocks — untouched.
fn rewrite_suite(original: &str, suite: &Suite) -> String {
    let mut out = String::with_capacity(original.len());
    let mut lines = original.lines();
    let mut section = "";
    let mut current_test: Option<String> = None;

    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("# ") {
            let title = rest.trim().to_lowercase();
            section = if title.starts_with("setup") {
                "setup"
            } else if title.starts_with("common types") {
                "common-types"
            } else if title.starts_with("tests") {
                "tests"
            } else {
                ""
            };
            current_test = None;
            out.push_str(line); out.push('\n');
            continue;
        }
        if section == "tests" {
            if let Some(rest) = line.strip_prefix("## ") {
                current_test = Some(rest.trim().to_string());
                out.push_str(line); out.push('\n');
                continue;
            }
        }
        // Look for an opening ts fence we should rewrite.
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            let lang = trimmed.trim_start_matches('`').trim();
            let is_ts = lang == "ts" || lang == "typescript";
            let replacement: Option<&str> = if !is_ts {
                None
            } else if section == "common-types" {
                Some(&suite.common_types)
            } else if section == "tests" {
                current_test.as_ref()
                    .and_then(|n| suite.tests.iter().find(|t| &t.name == n))
                    .filter(|t| !t.expected_is_error())
                    .map(|t| t.expected.as_str())
            } else {
                None
            };
            if let Some(body) = replacement {
                out.push_str(line); out.push('\n');
                for inner in lines.by_ref() {
                    if inner.trim_start().starts_with("```") {
                        out.push_str(body);
                        if !body.ends_with('\n') { out.push('\n'); }
                        out.push_str(inner);
                        out.push('\n');
                        break;
                    }
                }
                continue;
            }
        }
        out.push_str(line); out.push('\n');
    }
    out
}

// -------------------- diff --------------------

fn diff_unified(expected: &str, actual: &str) -> String {
    let mut out = String::new();
    out.push_str("--- expected\n+++ actual\n");
    let diff = TextDiff::from_lines(expected, actual);
    for group in diff.grouped_ops(3) {
        for op in group {
            for change in diff.iter_changes(&op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal  => ' ',
                };
                out.push(sign);
                out.push_str(change.value());
                if !out.ends_with('\n') { out.push('\n'); }
            }
        }
    }
    out
}
