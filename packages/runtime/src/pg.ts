// Module augmentation for `pg` (node-postgres). Adds a SqlText-aware
// overload to `Pool.query` / `Client.query` / `PoolClient.query`, so
// when the first argument went through `q(...)` the row + param types
// narrow to whatever swell's analyzer inferred for that SQL.
//
// Auto-loaded by `import "./pg"` from this package's main entry — the
// user's tsconfig needs nothing.

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
