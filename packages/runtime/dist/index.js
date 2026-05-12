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
function isPostgresJs(d) {
    return (typeof d === "function" &&
        typeof d.unsafe === "function" &&
        typeof d.begin === "function");
}
function isNodePg(d) {
    return (typeof d === "object" &&
        d !== null &&
        typeof d.query === "function");
}
function adapt(d) {
    if (isPostgresJs(d))
        return adaptPostgresJs(d);
    if (isNodePg(d))
        return adaptNodePg(d, false);
    throw new Error("swell: unsupported driver — expected a postgres.js `Sql` or a node-pg `Pool`/`Client`");
}
// Postgres.js's `Sql` (top level) has `begin`; its `TransactionSql` only has
// `savepoint`. We accept both shapes and dispatch off whether `begin` is
// present — that's what gates `begin`/`savepoint` at runtime.
function adaptPostgresJs(sql) {
    const hasBegin = "begin" in sql && typeof sql.begin === "function";
    const hasSavepoint = "savepoint" in sql && typeof sql.savepoint === "function";
    return {
        async query(q, params) {
            const result = await sql.unsafe(q, params);
            // postgres.js returns a `RowList` — Array<TRow> with a `.count` field.
            // For write statements `.count` is the affected-row count; for reads
            // it equals `rows.length`.
            const rows = result;
            const count = result.count;
            return { rows: [...rows], count: count ?? rows.length };
        },
        begin(fn) {
            if (!hasBegin) {
                throw new Error("swell: nested begin() inside a transaction — use savepoint()");
            }
            return sql.begin((tx) => fn(adaptPostgresJs(tx)));
        },
        savepoint(name, fn) {
            if (!hasSavepoint) {
                throw new Error("swell: savepoint() must be called inside begin()");
            }
            return sql.savepoint(name, (sp) => fn(adaptPostgresJs(sp)));
        },
        cursor(q, params) {
            return {
                async *[Symbol.asyncIterator]() {
                    for await (const batch of sql.unsafe(q, params).cursor()) {
                        for (const row of batch)
                            yield row;
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
function adaptNodePg(client, inTx) {
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
            const release = "release" in conn && typeof conn.release === "function"
                ? () => conn.release()
                : () => { };
            try {
                await conn.query("BEGIN");
                const out = await fn(adaptNodePg(conn, true));
                await conn.query("COMMIT");
                return out;
            }
            catch (err) {
                try {
                    await conn.query("ROLLBACK");
                }
                catch {
                    // swallow rollback errors; the outer error is the real problem.
                }
                throw err;
            }
            finally {
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
            }
            catch (err) {
                try {
                    await client.query(`ROLLBACK TO SAVEPOINT ${safeName}`);
                }
                catch {
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
                    const release = "release" in conn && typeof conn.release === "function"
                        ? () => conn.release()
                        : () => { };
                    try {
                        // node-pg's `client.query(submittable)` returns the submittable
                        // (here the stream). The cast handles that overloaded shape.
                        conn.query(stream);
                        for await (const row of stream) {
                            yield row;
                        }
                    }
                    finally {
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
let pgQueryStreamCache = null;
async function loadPgQueryStream() {
    if (pgQueryStreamCache)
        return pgQueryStreamCache;
    try {
        // `pg-query-stream` is an *optional* peer — there's no @types/pg-query-stream
        // bundled with swell, so we tell TS to skip module resolution here. If the
        // user calls `.cursor()` on a node-pg driver without the peer installed,
        // the dynamic import throws and we surface the install hint below.
        // @ts-ignore optional peer dependency
        const mod = (await import("pg-query-stream"));
        const ctor = mod.default ?? mod;
        pgQueryStreamCache = ctor;
        return ctor;
    }
    catch (err) {
        throw new Error("swell: node-pg .cursor() needs the `pg-query-stream` package installed alongside `pg`", { cause: err });
    }
}
function quoteSavepoint(name) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) {
        throw new Error(`swell: invalid savepoint name ${JSON.stringify(name)}`);
    }
    return name;
}
// ----------------------------------------------------------------------------
// TypedSql constructor
// ----------------------------------------------------------------------------
function makeTypedSql(d) {
    const run = (q, vs) => d.query(q, vs);
    const fn = ((q, ...vs) => run(q, vs).then((r) => r.rows));
    fn.many = ((q, ...vs) => run(q, vs).then((r) => r.rows));
    fn.one = (async (q, ...vs) => {
        const { rows } = await run(q, vs);
        if (rows.length !== 1) {
            throw new Error(`swell: expected exactly one row, got ${rows.length}`);
        }
        return rows[0];
    });
    fn.maybe = (async (q, ...vs) => {
        const { rows } = await run(q, vs);
        if (rows.length > 1) {
            throw new Error(`swell: expected zero or one row, got ${rows.length}`);
        }
        return rows[0] ?? null;
    });
    fn.exec = (async (q, ...vs) => {
        const { count } = await run(q, vs);
        return { rowCount: count };
    });
    fn.begin = ((callback) => d.begin(async (tx) => callback(makeTypedSql(tx))));
    fn.savepoint = ((name, callback) => d.savepoint(name, async (sp) => callback(makeTypedSql(sp))));
    fn.unsafe = ((q, params = []) => run(q, params).then((r) => r.rows));
    fn.cursor = ((q, ...vs) => d.cursor(q, vs));
    fn.end = (opts) => d.end(opts);
    return fn;
}
/**
 * Build a typed `sql` handle over either a postgres.js `Sql` or a node-pg
 * `Pool`/`Client`/`PoolClient`. The codegen output's `createSql` is a thin
 * wrapper around this that binds the registry type parameter.
 */
export function createTypedSql(driver) {
    return makeTypedSql(adapt(driver));
}
//# sourceMappingURL=index.js.map