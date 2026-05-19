// pg (node-postgres) integration. Two pieces:
//
//   1. Module augmentation adds a `SqlText`-aware overload to
//      `Pool.query` / `Client.query` / `PoolClient.query`, so when the
//      first argument went through `q(...)` the row + param types narrow
//      to whatever swell's analyzer inferred for that SQL. Auto-loaded by
//      `import "./pg"` (the user's tsconfig needs nothing).
//
//   2. An explicit `QueryType` export users can reach for when they
//      wrap pg's client (eg. building their own `dbQueryR`/`dbQueryW`
//      helpers). Augmentation alone can ADD overloads but not REMOVE
//      pg's stock `string` overload — so a q-marked call site that
//      passes wrong-typed values silently falls through to that stock
//      overload and resolves to `QueryResult<any>` instead of erroring.
//      `QueryType` is "what pg.Client.query *should* have been": the
//      `SqlText` overload first, then the rest of pg's overload set with
//      `string` replaced by `RawSql` so a `SqlText` can't match the
//      fallback path.

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
      values?: P,
    ): Promise<QueryResult<R extends QueryResultRow ? R : QueryResultRow>>;
  }

  interface Pool {
    query<P extends unknown[], R>(
      queryText: SqlText<P, R>,
      values?: P,
    ): Promise<QueryResult<R extends QueryResultRow ? R : QueryResultRow>>;
  }
}

/// Plain SQL string that is *not* a `SqlText` — i.e., hasn't been
/// branded by `q("…")`. The `__sqlBrand?: never` clause makes any
/// `SqlText<P, R>` (whose `__sqlBrand` is `{ params; row }`)
/// structurally incompatible, so a `SqlText` can't match an overload
/// typed against `RawSql`. Used to guard the fallback overloads of
/// `QueryType` below.
export type RawSql = string & { readonly __sqlBrand?: never };

/// The overload set pg.Client.query *should* have under swell:
///   - registry-narrowed for `SqlText` arguments (strict first overload)
///   - identical to pg's own overloads for Submittable / QueryConfig
///   - `string` replaced by `RawSql` everywhere else, so wrong-typed
///     `q("…")` args can't silently fall through to `QueryResult<any>`.
///
/// Use when you wrap `pg.Client.query` in your own helper:
///
///   import type { QueryType } from "swell/pg";
///   const dbQueryR: QueryType = ((qt: any, v?: any) =>
///     pool.query(qt, v)) as QueryType;
///
/// The first overload uses no `R extends QueryResultRow` constraint
/// (swell's fallback `q()` returns `SqlText<unknown[], unknown>`, and
/// `unknown` doesn't extend `QueryResultRow`); narrowing happens via
/// a conditional in the return type instead, so registry hits stay
/// typed and registry misses still resolve.
export type QueryType = {
  <P extends unknown[], R>(
    queryText: SqlText<P, R>,
    values?: P,
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
