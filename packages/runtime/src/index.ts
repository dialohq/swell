/**
 * swell runtime — driver-agnostic typed `sql`.
 *
 * Each project's codegen output (`swell.generated.ts`) does:
 *
 *   import { createTypedSql, type AnyDriver, type TypedSql } from "swell";
 *
 *   export interface QueryRegistry { ... }
 *   export type Sql = TypedSql<QueryRegistry>;
 *   export function createSql(driver: AnyDriver): Sql {
 *     return createTypedSql<QueryRegistry>(driver);
 *   }
 *
 * Then per-package `db.ts` chooses the driver:
 *
 *   import postgres from "postgres";
 *   import { createSql } from "./swell.generated";
 *   export const sql = createSql(postgres());
 *
 *   // or with node-pg
 *   import { Pool } from "pg";
 *   import { createSql } from "./swell.generated";
 *   export const sql = createSql(new Pool());
 *
 * Call sites look the same on either driver:
 *   await sql.many("SELECT id, email FROM users WHERE id = $1", userId);
 */

/**
 * Shape of a single entry in a query registry. swell emits one of these
 * per analysed query.
 */
export type QueryShape = { params: readonly unknown[]; row: unknown };

/**
 * The empty default — `createTypedSql()` without an explicit registry yields
 * a TypedSql with no registered queries. Every call falls through to the
 * permissive `string` overload (params: `unknown[]`, row: `unknown`).
 */
export type EmptyRegistry = Record<string, QueryShape>;

/**
 * Recursive structural type for `json` / `jsonb` columns. Postgres's
 * DESCRIBE only tells us "this is a json blob" — the value-level shape is
 * left to runtime narrowing.
 */
export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };

// Per-method type helpers — pick params and row from the registry by SQL key.
type Params<R extends EmptyRegistry, S extends string> = S extends keyof R
  ? R[S] extends { params: infer P extends readonly unknown[] }
    ? P
    : unknown[]
  : unknown[];

type Row<R extends EmptyRegistry, S extends string> = S extends keyof R
  ? R[S] extends { row: infer Row }
    ? Row
    : unknown
  : unknown;

/**
 * Statically-typed `sql` handle. `R` is the per-package query registry —
 * each `createSql()` call has its own, so identical SQL text in two
 * databases doesn't share a row shape.
 */
export interface TypedSql<R extends EmptyRegistry = EmptyRegistry> {
  /** Default form — returns all rows. */
  <S extends string>(sql: S, ...values: Params<R, S>): Promise<Row<R, S>[]>;

  /** Exactly one row. Throws if rowCount !== 1. */
  one<S extends string>(sql: S, ...values: Params<R, S>): Promise<Row<R, S>>;

  /** Zero or one row. Returns null if no rows; throws if >1. */
  maybe<S extends string>(sql: S, ...values: Params<R, S>): Promise<Row<R, S> | null>;

  /** All rows — explicit equivalent of the call form. */
  many<S extends string>(sql: S, ...values: Params<R, S>): Promise<Row<R, S>[]>;

  /** Side-effecting statement. Returns affected-row count. */
  exec<S extends string>(sql: S, ...values: Params<R, S>): Promise<{ rowCount: number }>;

  /** Transaction. Commits on resolve, rolls back on throw. */
  begin<T>(fn: (tx: TypedSql<R>) => Promise<T>): Promise<T>;

  /** Named savepoint inside a transaction. Throws if called outside `begin`. */
  savepoint<T>(name: string, fn: (tx: TypedSql<R>) => Promise<T>): Promise<T>;

  /**
   * Escape hatch for dynamic SQL. Bypasses swell codegen; not type-checked.
   * Optionally narrowed via an explicit row type parameter.
   */
  unsafe<T = unknown>(query: string, params?: unknown[]): Promise<T[]>;

  /** Server-side cursor — yields rows one at a time without buffering. */
  cursor<S extends string>(sql: S, ...values: Params<R, S>): AsyncIterable<Row<R, S>>;
  cursor(sql: string, ...values: unknown[]): AsyncIterable<unknown>;

  /** Close the underlying connection / pool. */
  end(opts?: { timeout?: number }): Promise<void>;
}

// ----------------------------------------------------------------------------
// Driver adapter
// ----------------------------------------------------------------------------

/**
 * Either a postgres.js `Sql` or a node-pg `Pool` / `Client` / `PoolClient`.
 * Detected structurally — the runtime never imports either library, so both
 * are *optional* peer dependencies. Users install whichever one they're
 * using (and only that one).
 *
 * The structural shapes below cover only what swell touches. The real
 * postgres.js and pg types are wider; they satisfy these without casts.
 */
export type AnyDriver = PostgresJsLike | NodePgLike;

/** Postgres.js's `cursor()` result: an async iterable of row *batches*. */
export interface PostgresJsCursor<TRow = unknown> {
  [Symbol.asyncIterator](): AsyncIterator<readonly TRow[]>;
}

/**
 * Postgres.js's `unsafe(...)` return: a thenable that resolves to the row
 * list (with `.count` for write-statement row counts), and also exposes
 * `.cursor()` for streaming reads.
 */
export interface PostgresJsPendingQuery<TRow = unknown>
  extends PromiseLike<readonly TRow[] & { count: number }> {
  cursor(rowsPerBatch?: number): PostgresJsCursor<TRow>;
}

/**
 * Shared surface that both `Sql` and `TransactionSql` have. Only the methods
 * swell actually calls — `.unsafe()` for queries; the tagged-template call
 * form is left to the user's own code, so we don't restate it here.
 */
export interface PostgresJsCallable {
  unsafe<TRow = unknown>(
    query: string,
    params?: unknown[],
  ): PostgresJsPendingQuery<TRow>;
}

/**
 * Structural view of postgres.js's `Sql` — the top-level handle.
 *
 * `begin` and `savepoint` are loosely typed (`(cb: (tx: any) => any) =>
 * Promise<any>`) — postgres.js's real signatures rely on a bespoke
 * `UnwrapPromiseArray<T>` and a recursive `TransactionSql` shape that
 * swell can't restate without re-deriving the whole postgres.js type
 * surface. The looser declaration here is what the real types resolve to
 * for assignability purposes; runtime callers are strongly typed via
 * `TypedSql<R>` further out (the adapter handles dispatch internally).
 */
export interface PostgresJsLike extends PostgresJsCallable {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  begin(callback: (tx: any) => any): Promise<any>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  begin(options: string, callback: (tx: any) => any): Promise<any>;
  end?(opts?: { timeout?: number }): Promise<void>;
}

/**
 * Structural view of postgres.js's `TransactionSql` — what `tx` is inside a
 * `begin(...)` callback. Same loose `savepoint` shape so postgres.js's
 * recursive TransactionSql.savepoint trivially satisfies it.
 */
export interface PostgresJsTxLike extends PostgresJsCallable {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  savepoint(callback: (sp: any) => any): Promise<any>;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  savepoint(name: string, callback: (sp: any) => any): Promise<any>;
}

/** Node-pg's query result: `{ rows, rowCount }`. */
export interface NodePgQueryResult<TRow = unknown> {
  rows: TRow[];
  rowCount: number | null;
}

/** Structural view of node-pg's `Pool` / `Client` / `PoolClient`. */
export interface NodePgLike {
  query<TRow = unknown>(
    text: string,
    values?: unknown[],
  ): Promise<NodePgQueryResult<TRow>>;
  /** Only present on `Pool` — yields a per-tx `PoolClient`. */
  connect?(): Promise<NodePgPoolClientLike>;
  end?(): Promise<void>;
}

/** A `Pool`-acquired `PoolClient` that must be released after use. */
export interface NodePgPoolClientLike extends NodePgLike {
  release(err?: boolean | Error): void;
}

// Normalised internal driver: a tiny query/begin/savepoint/cursor surface.
// All adapters reduce to this; `makeTypedSql` consumes it. `inTx` is true
// for adapters created inside a transaction — gates `.savepoint`.
interface NormDriver {
  query(sql: string, params: unknown[]): Promise<{ rows: unknown[]; count: number }>;
  begin<T>(fn: (tx: NormDriver) => Promise<T>): Promise<T>;
  savepoint<T>(name: string, fn: (tx: NormDriver) => Promise<T>): Promise<T>;
  cursor(sql: string, params: unknown[]): AsyncIterable<unknown>;
  end(opts?: { timeout?: number }): Promise<void>;
}

function isPostgresJs(d: AnyDriver): d is PostgresJsLike {
  return (
    typeof d === "function" &&
    typeof (d as PostgresJsLike).unsafe === "function" &&
    typeof (d as PostgresJsLike).begin === "function"
  );
}

function isNodePg(d: AnyDriver): d is NodePgLike {
  return (
    typeof d === "object" &&
    d !== null &&
    typeof (d as NodePgLike).query === "function"
  );
}

function adapt(d: AnyDriver): NormDriver {
  if (isPostgresJs(d)) return adaptPostgresJs(d);
  if (isNodePg(d)) return adaptNodePg(d, false);
  throw new Error(
    "swell: unsupported driver — expected a postgres.js `Sql` or a node-pg `Pool`/`Client`",
  );
}

// Postgres.js's `Sql` (top level) has `begin`; its `TransactionSql` only has
// `savepoint`. We accept both shapes and dispatch off whether `begin` is
// present — that's what gates `begin`/`savepoint` at runtime.
function adaptPostgresJs(sql: PostgresJsLike | PostgresJsTxLike): NormDriver {
  const hasBegin = "begin" in sql && typeof sql.begin === "function";
  const hasSavepoint = "savepoint" in sql && typeof sql.savepoint === "function";
  return {
    async query(q, params) {
      const result = await sql.unsafe(q, params);
      // postgres.js returns a `RowList` — Array<TRow> with a `.count` field.
      // For write statements `.count` is the affected-row count; for reads
      // it equals `rows.length`.
      const rows = result as unknown as readonly unknown[];
      const count = (result as unknown as { count?: number }).count;
      return { rows: [...rows], count: count ?? rows.length };
    },
    begin(fn) {
      if (!hasBegin) {
        throw new Error("swell: nested begin() inside a transaction — use savepoint()");
      }
      return (sql as PostgresJsLike).begin((tx) => fn(adaptPostgresJs(tx)));
    },
    savepoint(name, fn) {
      if (!hasSavepoint) {
        throw new Error("swell: savepoint() must be called inside begin()");
      }
      return (sql as PostgresJsTxLike).savepoint(name, (sp) =>
        fn(adaptPostgresJs(sp)),
      );
    },
    cursor(q, params) {
      return {
        async *[Symbol.asyncIterator]() {
          for await (const batch of sql.unsafe(q, params).cursor()) {
            for (const row of batch) yield row;
          }
        },
      };
    },
    end(opts) {
      return "end" in sql && typeof sql.end === "function"
        ? sql.end(opts) ?? Promise.resolve()
        : Promise.resolve();
    },
  };
}

function adaptNodePg(client: NodePgLike, inTx: boolean): NormDriver {
  return {
    async query(q, params) {
      const result = await client.query(q, params);
      return { rows: result.rows, count: result.rowCount ?? result.rows.length };
    },
    async begin(fn) {
      // `connect` distinguishes a Pool from a Client. Pools yield a per-tx
      // PoolClient that we must release; on a bare Client we run the tx in
      // place.
      const conn = client.connect ? await client.connect() : client;
      const release =
        "release" in conn && typeof conn.release === "function"
          ? () => (conn as NodePgPoolClientLike).release()
          : () => {};
      try {
        await conn.query("BEGIN");
        const out = await fn(adaptNodePg(conn, true));
        await conn.query("COMMIT");
        return out;
      } catch (err) {
        try {
          await conn.query("ROLLBACK");
        } catch {
          // swallow rollback errors; the outer error is the real problem.
        }
        throw err;
      } finally {
        release();
      }
    },
    async savepoint(name, fn) {
      if (!inTx) {
        throw new Error("swell: savepoint() must be called inside begin()");
      }
      const safeName = quoteSavepoint(name);
      await client.query(`SAVEPOINT ${safeName}`);
      try {
        const out = await fn(adaptNodePg(client, true));
        await client.query(`RELEASE SAVEPOINT ${safeName}`);
        return out;
      } catch (err) {
        try {
          await client.query(`ROLLBACK TO SAVEPOINT ${safeName}`);
        } catch {
          // swallow
        }
        throw err;
      }
    },
    cursor(q, params) {
      // node-pg streams via the `pg-query-stream` package — loaded lazily
      // so it stays an optional peer. If you call `.cursor()` on a node-pg
      // driver without `pg-query-stream` installed, the import throws with
      // a clear `MODULE_NOT_FOUND`.
      return {
        async *[Symbol.asyncIterator]() {
          const QueryStream = await loadPgQueryStream();
          const stream = new QueryStream(q, params);
          // pg-query-stream needs a single Client/PoolClient (it submits
          // its own Submittable). On a Pool we check out one for the
          // lifetime of the stream; on a Client we use it directly.
          const conn = client.connect ? await client.connect() : client;
          const release =
            "release" in conn && typeof conn.release === "function"
              ? () => (conn as NodePgPoolClientLike).release()
              : () => {};
          try {
            // node-pg's `client.query(submittable)` returns the submittable
            // (here the stream). The cast handles that overloaded shape.
            (conn as NodePgPgQueryStreamCapable).query(stream);
            for await (const row of stream as AsyncIterable<unknown>) {
              yield row;
            }
          } finally {
            release();
          }
        },
      };
    },
    end() {
      return client.end?.() ?? Promise.resolve();
    },
  };
}

// Lazy loader for `pg-query-stream`. Resolved on first cursor() call; any
// further calls hit the cached module.
//
// Declared as a `new`-able constructor type so we don't depend on the
// real `pg-query-stream` typings at compile time.
type PgQueryStreamCtor = new (
  text: string,
  values?: unknown[],
  config?: { batchSize?: number; highWaterMark?: number },
) => AsyncIterable<unknown>;

interface NodePgPgQueryStreamCapable {
  query<T>(submittable: T): T;
}

let pgQueryStreamCache: PgQueryStreamCtor | null = null;
async function loadPgQueryStream(): Promise<PgQueryStreamCtor> {
  if (pgQueryStreamCache) return pgQueryStreamCache;
  try {
    // `pg-query-stream` is an *optional* peer — there's no @types/pg-query-stream
    // bundled with swell, so we tell TS to skip module resolution here. If the
    // user calls `.cursor()` on a node-pg driver without the peer installed,
    // the dynamic import throws and we surface the install hint below.
    // @ts-ignore optional peer dependency
    const mod = (await import("pg-query-stream")) as unknown as {
      default?: PgQueryStreamCtor;
    };
    const ctor = mod.default ?? (mod as unknown as PgQueryStreamCtor);
    pgQueryStreamCache = ctor;
    return ctor;
  } catch (err) {
    throw new Error(
      "swell: node-pg .cursor() needs the `pg-query-stream` package installed alongside `pg`",
      { cause: err },
    );
  }
}

function quoteSavepoint(name: string): string {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) {
    throw new Error(`swell: invalid savepoint name ${JSON.stringify(name)}`);
  }
  return name;
}

// ----------------------------------------------------------------------------
// TypedSql constructor
// ----------------------------------------------------------------------------

function makeTypedSql<R extends EmptyRegistry>(d: NormDriver): TypedSql<R> {
  const run = (q: string, vs: unknown[]) => d.query(q, vs);

  const fn = ((q: string, ...vs: unknown[]) =>
    run(q, vs).then((r) => r.rows)) as unknown as TypedSql<R>;

  fn.many = ((q: string, ...vs: unknown[]) =>
    run(q, vs).then((r) => r.rows)) as TypedSql<R>["many"];

  fn.one = (async (q: string, ...vs: unknown[]) => {
    const { rows } = await run(q, vs);
    if (rows.length !== 1) {
      throw new Error(`swell: expected exactly one row, got ${rows.length}`);
    }
    return rows[0];
  }) as TypedSql<R>["one"];

  fn.maybe = (async (q: string, ...vs: unknown[]) => {
    const { rows } = await run(q, vs);
    if (rows.length > 1) {
      throw new Error(`swell: expected zero or one row, got ${rows.length}`);
    }
    return rows[0] ?? null;
  }) as TypedSql<R>["maybe"];

  fn.exec = (async (q: string, ...vs: unknown[]) => {
    const { count } = await run(q, vs);
    return { rowCount: count };
  }) as TypedSql<R>["exec"];

  fn.begin = (<T>(callback: (tx: TypedSql<R>) => Promise<T>) =>
    d.begin(async (tx) => callback(makeTypedSql<R>(tx)))) as TypedSql<R>["begin"];

  fn.savepoint = (<T>(name: string, callback: (tx: TypedSql<R>) => Promise<T>) =>
    d.savepoint(name, async (sp) => callback(makeTypedSql<R>(sp)))) as TypedSql<R>["savepoint"];

  fn.unsafe = ((q: string, params: unknown[] = []) =>
    run(q, params).then((r) => r.rows)) as TypedSql<R>["unsafe"];

  fn.cursor = ((q: string, ...vs: unknown[]) =>
    d.cursor(q, vs)) as TypedSql<R>["cursor"];

  fn.end = (opts?: { timeout?: number }) => d.end(opts);

  return fn;
}

/**
 * Build a typed `sql` handle over either a postgres.js `Sql` or a node-pg
 * `Pool`/`Client`/`PoolClient`. The codegen output's `createSql` is a thin
 * wrapper around this that binds the registry type parameter.
 */
export function createTypedSql<R extends EmptyRegistry = EmptyRegistry>(
  driver: AnyDriver,
): TypedSql<R> {
  return makeTypedSql<R>(adapt(driver));
}
