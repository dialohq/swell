// pg (node-postgres) integration. Two pieces:
//
//   1. Module augmentation adds a `SqlText`-aware overload to
//      `Pool.query` / `Client.query` / `PoolClient.query`, so when the
//      first argument went through `q(...)` the row + param types narrow
//      to whatever swell's analyzer inferred for that SQL. Auto-loaded by
//      `import "./pg"` (the user's tsconfig needs nothing).
//
//      Strictness: `SqlText` is *not* a `string` at the type level (see
//      `types.ts`). pg's stock `query(text: string, …)` overload can
//      therefore never catch a `q("…")` call site, so wrong-typed
//      values can't silently fall through to `QueryResult<any>` —
//      they produce a real overload-mismatch error. No tsconfig
//      wiring, no @types/pg fork.
//
//   2. An explicit `QueryType` export users can reach for when they
//      wrap pg's client (eg. building their own `dbQueryR`/`dbQueryW`
//      helpers).

import type {
  Submittable,
  QueryArrayConfig,
  QueryArrayResult,
  QueryConfig,
  QueryConfigValues,
  QueryResult,
  QueryResultRow,
} from "pg";
import type { SqlText } from "./types.js";

declare module "pg" {
  interface ClientBase {
    query<P extends unknown[], R>(
      queryText: SqlText<P, R>,
      values?: NoInfer<P>,
    ): Promise<QueryResult<R extends QueryResultRow ? R : QueryResultRow>>;
  }

  interface Pool {
    query<P extends unknown[], R>(
      queryText: SqlText<P, R>,
      values?: NoInfer<P>,
    ): Promise<QueryResult<R extends QueryResultRow ? R : QueryResultRow>>;
  }
}

/// Plain SQL string — alias for `string`, kept as a named type so the
/// `QueryType` fallback overload reads symmetrically against the
/// `SqlText` overload above it. (`SqlText` is no longer a `string` at
/// the type level, so no extra "exclude SqlText" guard is needed here.)
export type RawSql = string;

/// The overload set pg.Client.query / pg.Pool.query has under swell,
/// re-exported as a type for users wrapping pg in their own helper:
///
///   import type { QueryType } from "swell/pg";
///   const dbQueryR: QueryType = ((qt: any, v?: any) =>
///     pool.query(qt, v)) as QueryType;
///
/// Behaviour matches the augmented `Pool.query` / `Client.query`:
///   - `SqlText<P, R>` argument → row + params narrowed by the registry
///   - mismatched values for a `SqlText` → overload error (not `any`)
///   - everything else → pg's stock overloads
///
/// The first overload uses no `R extends QueryResultRow` constraint
/// (swell's fallback `q()` returns `SqlText<unknown[], unknown>`, and
/// `unknown` doesn't extend `QueryResultRow`); narrowing happens via
/// a conditional in the return type instead, so registry hits stay
/// typed and registry misses still resolve.
export type QueryType = {
  <P extends unknown[], R>(
    queryText: SqlText<P, R>,
    values?: NoInfer<P>,
  ): Promise<QueryResult<R extends QueryResultRow ? R : QueryResultRow>>;
  <T extends Submittable>(queryStream: T): Promise<T>;
  <R extends any[] = any[], I = any[]>(
    queryConfig: QueryArrayConfig<I>,
    values?: QueryConfigValues<I>,
  ): Promise<QueryArrayResult<R>>;
  <R extends QueryResultRow = any, I = any>(
    queryConfig: QueryConfig<I>,
  ): Promise<QueryResult<R>>;
  <R extends QueryResultRow = any, I = any[]>(
    queryTextOrConfig: RawSql | QueryConfig<I>,
    values?: QueryConfigValues<I>,
  ): Promise<QueryResult<R>>;
};
