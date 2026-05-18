# swell

Statically-typed inline Postgres queries for TypeScript.

swell scans your source for `q("SELECT …")` call sites, sends each unique SQL
string to a dev Postgres for PARSE/DESCRIBE + EXPLAIN, and emits a per-package
`swell.generated.ts` that pins a typed `SqlText<Params, Row>` brand on every
known query. A module augmentation over node-postgres makes
`pool.query(q(…), […])` narrow rows + params to the inferred shape; non-literal
queries fall through to the permissive `string`-typed overload.

What you write:

```ts
// db.ts
import { Pool } from "pg";
import "swell";

export { q } from "./swell.generated";
export const pool = new Pool();
```

```ts
// elsewhere
import { pool, q } from "./db";

const { rows } = await pool.query(
  q("SELECT id, email FROM users WHERE id = $1"),
  [userId],
);
// rows is typed { id: string; email: string }[]
```

What swell does:

```
src/**/*.ts → scanner → unique SQL strings
                          ↓
               dev PG (PARSE/DESCRIBE + EXPLAIN)
                          ↓
                   analyzer (nullability, JSON shape, enums)
                          ↓
             src/swell.generated.ts (Registry + typed q overload)
```

## Layout

- `crates/swell-analyzer` — SQL → TS type inference (Rust).
- `crates/swell-scanner` — TS source scanning for `q("...")` calls (Rust).
- `crates/swell-codegen` — per-package `Registry` + typed `q` overload emitter.
- `crates/swell-cli` — `swell gen`, `swell watch`, `swell check`, `swell prepare`.
- `packages/runtime` — npm package `swell`: the `q()` marker + pg module augmentation.
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

## CI

The same checks run locally as on remote — both via `nix-fast-build`:

```sh
nix-fast-build --flake .#checks.x86_64-linux   # full check matrix
nix build .#checks.x86_64-linux.runtime-typecheck  # single check
nix build .#checks.x86_64-linux.example-typecheck
nix build .#checks.x86_64-linux.cargo-build
```

A green `nix-fast-build` locally guarantees a green CI run.

## License

MIT OR Apache-2.0
