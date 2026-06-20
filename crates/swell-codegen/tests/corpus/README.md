# End-to-end test corpus

One `.md` file = one **suite** of tests sharing a schema. The runner
lives in `tests/corpus.rs`; `cargo test -p swell-codegen --test
corpus` runs every suite, parsing the markdown with `pulldown-cmark`
and asserting each test against the full pipeline (live Postgres →
analyzer → codegen helpers).

## Suite format

````md
# Setup

```sql
DROP TABLE IF EXISTS users CASCADE;
CREATE TABLE users (id uuid PRIMARY KEY, …);
```

# Common types

```ts
export interface Users { id: string; … }
```

# Tests

## A scenario name

```sql
SELECT id, email FROM users WHERE id = $1
```

```ts
$1: string | null
result: { id: Users["id"]; email: Users["email"] }
```
````

Sections, recognised by H1 headings:

* **`# Setup`** — an inline ` ```sql ` block applied to the DB before
  any test in the suite runs. Markdown-link form `# [Setup](./x.sql)`
  is also supported for fixtures shared across multiple suites
  (resolved relative to the .md).
* **`# Common types`** — the rendered table interfaces. Maintained
  automatically: when the schema changes, `CORPUS_PROMOTE=1` rewrites
  this block.
* **`# Tests`** — header marker. Each `## <name>` underneath is one
  test case with two blocks: a ` ```sql ` (the query) and a ` ```ts `
  (the expected). The expected uses the **compact form**:
    * `$N: <type>` — one line per param.
    * `result: <ts-type>` — the row type, or `result: never` for
      write-only queries.
    * `error: <substring>` — the analyzer must fail with this
      substring in the error text. No other content allowed in an
      error case.

`_schemas.txt` (one schema per line) lists schemas the analyzer's
type catalog should load — needed when fixtures put their objects in
a non-`public` schema (e.g. `billing`). Defaults to `public`.

## Promotion (cram-style)

```bash
CORPUS_PROMOTE=1 cargo test -p swell-codegen --test corpus
```

The runner rewrites the `# Common types` block and each `## <test>`'s
` ```ts ` block in place, preserving everything else byte-for-byte.
Inspect with `git diff`, commit if right.

## Adding a test

Drop a `## <name>` block into the appropriate suite with the SQL
fence and an empty ` ```ts ``` ` block. Run with `CORPUS_PROMOTE=1`.
The expected gets filled in automatically.

## Adding a suite

Create `tests/corpus/<name>.md` (top-level — suites are flat,
file-per-suite). Either include an inline ` ```sql ` block under
`# Setup` or link to a sibling `_setup.sql`.
