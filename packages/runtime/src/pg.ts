// Module augmentation for `pg` (node-postgres). Importing this file once
// from a project that uses `q("...")` brands tells TypeScript to narrow
// every `Pool.query(q(...))` / `Client.query(q(...))` call site to the
// `{ params: P; row: R }` shape that swell's analyzer inferred from the
// live database.
//
// Why a separate subpath: the augmentation only makes sense for projects
// that drive node-pg. Projects on postgres.js never need it; isolating it
// behind `import "swell/pg"` keeps the main entry point free of cross-
// driver type pollution.
//
// Usage at the consumer side:
//   // tsconfig.json or any project entry-point .ts file:
//   import "swell/pg";
//
//   import pg from "pg";
//   import { q } from "swell";
//   const pool = new pg.Pool();
//   const stmt = q("SELECT id, email FROM users WHERE id = $1");
//   const { rows } = await pool.query(stmt, [userId]);
//   //                                       ^? params type narrowed
//   //      ^? rows: { id: string; email: string }[]
//
// The new overload accepts `values` as an *array* (not a tuple) because
// node-pg's runtime contract is positional-by-index; TS still verifies
// element-by-element against the registry's `params` tuple.

import type { QueryResult, QueryResultRow } from "pg";
import type { SqlText } from "./index";

declare module "pg" {
  interface ClientBase {
    query<P extends unknown[], R extends QueryResultRow>(
      queryText: SqlText<P, R>,
      values?: P,
    ): Promise<QueryResult<R>>;
  }

  interface Pool {
    query<P extends unknown[], R extends QueryResultRow>(
      queryText: SqlText<P, R>,
      values?: P,
    ): Promise<QueryResult<R>>;
  }

}

// Re-exporting an empty object marker so this file is treated as a module
// (otherwise `declare module` interface-merge augmentations don't apply).
export {};
