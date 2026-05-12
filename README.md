# swell

Statically-typed inline Postgres queries for TypeScript.

swell scans your source for `sql.many("SELECT …")` / `sql.one(…)` / `sql.exec(…)`
call sites, sends each unique SQL string to a dev Postgres for PARSE/DESCRIBE +
EXPLAIN, and emits a per-package `swell.generated.ts` that wraps your driver
(postgres.js or node-pg) in a typed `sql` handle. Call sites narrow to the
exact registered row + parameter shape; non-literal queries fall through to a
permissive overload.

What you write:

```ts
// db.ts
import postgres from "postgres";
import { createSql } from "./swell.generated";
export const sql = createSql(postgres());
```

```ts
// elsewhere
import { sql } from "./db";

const user = await sql.one(
  "SELECT id, email FROM users WHERE id = $1",
  userId,
);
// user is typed { id: string; email: string }
```

What swell does:

```
src/**/*.ts → scanner → unique SQL strings
                          ↓
               dev PG (PARSE/DESCRIBE + EXPLAIN)
                          ↓
                   analyzer (nullability, JSON shape, enums)
                          ↓
                   src/swell.generated.ts (QueryRegistry + createSql factory)
```

## Layout

- `crates/swell-analyzer` — SQL → TS type inference (Rust, lands in the analyzer PR).
- `crates/swell-scanner` — TS source scanning (Rust, lands in the codegen PR).
- `crates/swell-codegen` — `QueryRegistry` + `createSql` factory emitter.
- `crates/swell-cli` — `swell gen`, `swell watch`, `swell check`, `swell prepare`.
- `packages/runtime` — npm package `swell`: `createTypedSql`, `TypedSql<R>`, adapters.
- `examples/basic` — end-to-end sample app.

## Build

The Nix dev shell is the source of truth:

```sh
nix develop
cargo build --release            # builds the `swell` binary
cd packages/runtime && bun run build
```

## Run

```sh
swell gen      # one-shot: scan everything, regenerate .ts
swell watch    # daemon: file watcher + incremental analyzer + codegen
swell check    # CI: cache must match source; fail if stale or invalid
swell prepare  # populate .swell/ cache for offline builds
```

## Test infra

Swell ships test helpers for the clone-per-test idiom:

```ts
import { initStandardPostgres, makeTemplate, withTestDb } from "swell/testing";
```

## License

MIT OR Apache-2.0
