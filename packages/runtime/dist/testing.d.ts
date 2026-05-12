import { type TypedSql } from "./index.js";
/** Connection details for the running Postgres. `deinit` releases resources. */
export interface PostgresHandle {
    host: string;
    port: number;
    user: string;
    password: string;
    database: string;
    deinit(): Promise<void>;
}
/**
 * Spin up a temporary Postgres on a unix socket under `$TMPDIR/swell-*`.
 * The data directory is removed on `deinit()`. Local dev only — CI should
 * point `DATABASE_URL` at a pre-existing instance and use `connectExisting`.
 */
export declare function initStandardPostgres(): Promise<PostgresHandle>;
/**
 * Wrap an existing Postgres for use with the helpers below. The URL must
 * point at a superuser DB (we issue `CREATE DATABASE` against it).
 */
export declare function connectExisting(url: string): PostgresHandle;
export interface MakeTemplateOptions {
    handle: PostgresHandle;
    /** Either a single glob, an array of globs, or absolute SQL file paths. */
    migrations: string | string[];
    /** Extra SQL files applied after migrations (test-only tables). */
    extraSchemas?: string[];
    /** Template DB name. Defaults to `swell_template`. */
    name?: string;
}
/**
 * Create a template database and apply migrations to it. Idempotent — skips
 * rebuild if the prior template's recorded fingerprint matches the current
 * migration set. Caller may invalidate by passing a new `name`.
 */
export declare function makeTemplate(opts: MakeTemplateOptions): Promise<string>;
export interface WithTestDbOptions<T> {
    handle: PostgresHandle;
    template: string;
    fn(sql: TypedSql, meta: {
        dbName: string;
    }): Promise<T>;
}
/**
 * Clone the template into a fresh test DB, run `fn`, drop the clone. Each
 * invocation gets a unique random name so concurrent calls don't collide.
 */
export declare function withTestDb<T>(opts: WithTestDbOptions<T>): Promise<T>;
//# sourceMappingURL=testing.d.ts.map