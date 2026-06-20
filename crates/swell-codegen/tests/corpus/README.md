# End-to-end test corpus

Each `.md` file in a category subdirectory is **one test case**. The
runner lives in `tests/corpus.rs` and exposes a single `cargo test`
case (`corpus`) that walks every case, runs the full pipeline
(Postgres → analyzer → codegen), and compares against the expected
output embedded in the markdown.

## Layout

```
tests/corpus/
├── README.md
├── analyzer/
│   ├── _setup.sql              ← shared fixture, applied once
│   ├── scalar_select_with_param.md
│   └── …
└── billing/
    ├── _setup.sql
    ├── _schemas.txt            ← non-public schemas the analyzer needs
    └── …
```

`_setup.sql` (optional) is applied once per category before any case
in it runs. It must be idempotent — `DROP … IF EXISTS … CASCADE`
followed by `CREATE …`. Cases reuse the tables it creates.

`_schemas.txt` (optional) is one schema name per line. Defaults to
`public`. Used so the analyzer's type catalog picks up enums /
domains / composite types defined in non-`public` schemas.

## Case format

A case is markdown with one or more fenced code blocks. Free-form
prose between fences documents the case — it doesn't affect what
runs.

* **Zero or one** ` ```sql ` block for an **ad-hoc schema** the case
  needs in addition to the shared `_setup.sql`. The runner snapshots
  `pg_class` before/after and drops exactly what this block created.
* **Zero or one** ` ```sql ` block for the **query** to analyze
  (must follow the schema block if both are present).
* **Exactly one** of:
  * ` ```ts ` — the full expected codegen output. The auto-gen banner
    is stripped before comparison.
  * ` ```err ` — substring that must appear in the analyzer's error.
    Use for "this SQL should fail" tests.

## Promotion (cram-style)

When the actual output drifts:

```
CORPUS_PROMOTE=1 cargo test -p swell-codegen --test corpus
```

The runner rewrites the ` ```ts ` block in each affected `.md` with
the actual output. Read `git diff` to confirm the change is intended.

## Adding a case

```bash
cat > tests/corpus/analyzer/my_new_test.md <<'EOF'
# My new test

```sql
SELECT id FROM users WHERE id = $1
```

```ts
```
EOF

CORPUS_PROMOTE=1 cargo test -p swell-codegen --test corpus
```

The empty ` ```ts ` block gets filled with the actual output. Inspect,
commit.
