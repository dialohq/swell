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
export type QueryShape = {
    params: readonly unknown[];
    row: unknown;
};
/**
 * Per-project registry of `q("…")`-marked SQL strings. Codegen augments
 * this interface via `declare module "swell" { interface Registry {…} }`
 * so the literal SQL string maps to its typed `{ params; row }` shape.
 *
 * Empty in the runtime; populated only after the project's
 * `swell.generated.ts` is included in the tsconfig.
 */
export interface Registry {
}
/**
 * Branded SQL string carried by `q("…")`. The intersection with the
 * non-optional `__sqlBrand` makes plain strings *not* assignable —
 * `TypedSql.many(...)` picks the SqlText-typed overload when (and only
 * when) the argument went through `q`. Plain strings keep falling
 * through to the legacy literal-narrowing overload, which preserves
 * backward compatibility for the existing `sql.many("SELECT …", arg)`
 * form.
 */
export type SqlText<P extends unknown[], Row> = string & {
    readonly __sqlBrand: {
        params: P;
        row: Row;
    };
};
type RegistryLookup<S extends string> = S extends keyof Registry ? Registry[S] : {
    params: unknown[];
    row: unknown;
};
/**
 * `q("SELECT id FROM users WHERE id = $1")` — no-op SQL marker. The
 * runtime cost is one cast (no allocation); the type-level work is the
 * conditional `Registry` lookup that pins the `SqlText` brand to the
 * exact params + row shape for this literal.
 *
 *   const stmt = q("SELECT id FROM users WHERE id = $1");
 *   const rows = await sql.many(stmt, userId);
 *   //    ^? { id: string }[]
 */
export declare function q<S extends string>(text: S): SqlText<RegistryLookup<S>["params"] extends infer P ? P extends readonly unknown[] ? P & unknown[] : unknown[] : unknown[], RegistryLookup<S>["row"]>;
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
export type Json = null | boolean | number | string | Json[] | {
    [key: string]: Json;
};
type Params<R extends EmptyRegistry, S extends string> = S extends keyof R ? R[S] extends {
    params: infer P extends readonly unknown[];
} ? P : unknown[] : unknown[];
type Row<R extends EmptyRegistry, S extends string> = S extends keyof R ? R[S] extends {
    row: infer Row;
} ? Row : unknown : unknown;
/**
 * Type guard used in the legacy-literal overload signatures: collapses a
 * `SqlText`-branded string to `never` so the q-marker overload is the
 * only one that matches `q("…")` results. Plain string literals stay as
 * themselves and still get the registry lookup.
 */
type PlainSql<S extends string> = S extends {
    readonly __sqlBrand: unknown;
} ? never : S;
/**
 * Statically-typed `sql` handle. `R` is the per-package query registry —
 * each `createSql()` call has its own, so identical SQL text in two
 * databases doesn't share a row shape.
 *
 * Each method has two overloads:
 *
 *   1. **q-marker form** (preferred): `sql.many(q("SELECT …"), …vs)`. The
 *      `SqlText<P, Row>` brand pins params + row at the call site via the
 *      global `Registry` interface (augmented by codegen).
 *   2. **literal-string form** (legacy): `sql.many("SELECT …", …vs)`. The
 *      literal SQL string narrows against the per-package `R` registry.
 *
 * The legacy overload's parameter is `PlainSql<S>` — for `q(...)`-branded
 * strings that collapses to `never`, so TS routes branded args to the
 * q-form and plain literals to the legacy form without ambiguity.
 */
export interface TypedSql<R extends EmptyRegistry = EmptyRegistry> {
    /** Default form — returns all rows. */
    <P extends unknown[], Row>(sql: SqlText<P, Row>, ...values: P): Promise<Row[]>;
    <S extends string>(sql: PlainSql<S>, ...values: Params<R, S>): Promise<Row<R, S>[]>;
    /** Exactly one row. Throws if rowCount !== 1. */
    one<P extends unknown[], Row>(sql: SqlText<P, Row>, ...values: P): Promise<Row>;
    one<S extends string>(sql: PlainSql<S>, ...values: Params<R, S>): Promise<Row<R, S>>;
    /** Zero or one row. Returns null if no rows; throws if >1. */
    maybe<P extends unknown[], Row>(sql: SqlText<P, Row>, ...values: P): Promise<Row | null>;
    maybe<S extends string>(sql: PlainSql<S>, ...values: Params<R, S>): Promise<Row<R, S> | null>;
    /** All rows — explicit equivalent of the call form. */
    many<P extends unknown[], Row>(sql: SqlText<P, Row>, ...values: P): Promise<Row[]>;
    many<S extends string>(sql: PlainSql<S>, ...values: Params<R, S>): Promise<Row<R, S>[]>;
    /** Side-effecting statement. Returns affected-row count. */
    exec<P extends unknown[], Row>(sql: SqlText<P, Row>, ...values: P): Promise<{
        rowCount: number;
    }>;
    exec<S extends string>(sql: PlainSql<S>, ...values: Params<R, S>): Promise<{
        rowCount: number;
    }>;
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
    cursor<P extends unknown[], Row>(sql: SqlText<P, Row>, ...values: P): AsyncIterable<Row>;
    cursor<S extends string>(sql: PlainSql<S>, ...values: Params<R, S>): AsyncIterable<Row<R, S>>;
    cursor(sql: string, ...values: unknown[]): AsyncIterable<unknown>;
    /** Close the underlying connection / pool. */
    end(opts?: {
        timeout?: number;
    }): Promise<void>;
}
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
export interface PostgresJsPendingQuery<TRow = unknown> extends PromiseLike<readonly TRow[] & {
    count: number;
}> {
    cursor(rowsPerBatch?: number): PostgresJsCursor<TRow>;
}
/**
 * Shared surface that both `Sql` and `TransactionSql` have. Only the methods
 * swell actually calls — `.unsafe()` for queries; the tagged-template call
 * form is left to the user's own code, so we don't restate it here.
 */
export interface PostgresJsCallable {
    unsafe<TRow = unknown>(query: string, params?: unknown[]): PostgresJsPendingQuery<TRow>;
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
    begin(callback: (tx: any) => any): Promise<any>;
    begin(options: string, callback: (tx: any) => any): Promise<any>;
    end?(opts?: {
        timeout?: number;
    }): Promise<void>;
}
/**
 * Structural view of postgres.js's `TransactionSql` — what `tx` is inside a
 * `begin(...)` callback. Same loose `savepoint` shape so postgres.js's
 * recursive TransactionSql.savepoint trivially satisfies it.
 */
export interface PostgresJsTxLike extends PostgresJsCallable {
    savepoint(callback: (sp: any) => any): Promise<any>;
    savepoint(name: string, callback: (sp: any) => any): Promise<any>;
}
/** Node-pg's query result: `{ rows, rowCount }`. */
export interface NodePgQueryResult<TRow = unknown> {
    rows: TRow[];
    rowCount: number | null;
}
/** Structural view of node-pg's `Pool` / `Client` / `PoolClient`. */
export interface NodePgLike {
    query<TRow = unknown>(text: string, values?: unknown[]): Promise<NodePgQueryResult<TRow>>;
    /** Only present on `Pool` — yields a per-tx `PoolClient`. */
    connect?(): Promise<NodePgPoolClientLike>;
    end?(): Promise<void>;
}
/** A `Pool`-acquired `PoolClient` that must be released after use. */
export interface NodePgPoolClientLike extends NodePgLike {
    release(err?: boolean | Error): void;
}
/**
 * Build a typed `sql` handle over either a postgres.js `Sql` or a node-pg
 * `Pool`/`Client`/`PoolClient`. The codegen output's `createSql` is a thin
 * wrapper around this that binds the registry type parameter.
 */
export declare function createTypedSql<R extends EmptyRegistry = EmptyRegistry>(driver: AnyDriver): TypedSql<R>;
export {};
//# sourceMappingURL=index.d.ts.map