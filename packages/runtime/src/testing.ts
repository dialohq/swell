/**
 * Test helpers for spinning up a Postgres and running queries against
 * per-test cloned databases.
 *
 * The pattern, lifted from dialo's `makeCaddie` + clone-per-test idiom:
 *   1. `initStandardPostgres()` — spins up an ephemeral Postgres on a unix
 *      socket under `$TMPDIR`. Or use `connectExisting(url)` in CI.
 *   2. `makeTemplate({ handle, migrations })` — creates a "template DB",
 *      applies migration SQL files in lexicographic order. Idempotent: skips
 *      rebuild if the schema fingerprint (sha256 of migration contents)
 *      matches the previous run.
 *   3. `withTestDb({ handle, template, fn })` — `CREATE DATABASE x WITH
 *      TEMPLATE template`, builds a `TypedSql` against it, runs `fn`, drops
 *      the DB. Microsecond clones; parallel-safe (random suffixes).
 *
 * Why a template clone rather than transaction rollback for isolation:
 *   - parallel-safe across multiple test processes;
 *   - sees the same DB state the production code sees (no pending-tx
 *     visibility quirks);
 *   - fast: Postgres's `CREATE DATABASE ... TEMPLATE` reuses block files.
 */
import { spawn, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import postgres from "postgres";
import { createTypedSql, type TypedSql } from "./index.js";

/** Connection details for the running Postgres. `deinit` releases resources. */
export interface PostgresHandle {
  host: string; // unix socket directory or hostname
  port: number; // tcp port; meaningless for socket-only handles
  user: string;
  password: string;
  database: string; // the root "postgres" DB
  deinit(): Promise<void>;
}

/**
 * Spin up a temporary Postgres on a unix socket under `$TMPDIR/swell-*`.
 * The data directory is removed on `deinit()`. Local dev only — CI should
 * point `DATABASE_URL` at a pre-existing instance and use `connectExisting`.
 */
export async function initStandardPostgres(): Promise<PostgresHandle> {
  const root = mkdtempSync(join(tmpdir(), "swell-"));
  const dataDir = join(root, "data");
  const sockDir = join(root, "sock");
  await new Promise<void>((res, rej) => {
    const p = spawn("initdb", ["-D", dataDir, "-U", "postgres", "--auth=trust", "--no-sync"], {
      stdio: "pipe",
    });
    let err = "";
    p.stderr?.on("data", (d) => { err += d.toString(); });
    p.on("exit", (code) => (code === 0 ? res() : rej(new Error(`initdb failed: ${err}`))));
  });

  // Postgres won't auto-create the socket dir; do it.
  await import("node:fs").then(({ mkdirSync }) => mkdirSync(sockDir, { recursive: true }));

  const proc: ChildProcess = spawn(
    "postgres",
    ["-D", dataDir, "-k", sockDir, "-h", "", "-c", "fsync=off", "-c", "synchronous_commit=off"],
    { stdio: "pipe" },
  );

  // Wait for the server to accept connections.
  await waitFor(async () => {
    const sql = postgres({ host: sockDir, user: "postgres", database: "postgres" });
    try {
      await sql`SELECT 1`;
      await sql.end({ timeout: 1 });
      return true;
    } catch {
      try { await sql.end({ timeout: 1 }); } catch { /* swallow */ }
      return false;
    }
  }, 30_000);

  return {
    host: sockDir,
    port: 5432,
    user: "postgres",
    password: "",
    database: "postgres",
    async deinit() {
      proc.kill("SIGTERM");
      await new Promise<void>((res) => proc.once("exit", () => res()));
      rmSync(root, { recursive: true, force: true });
    },
  };
}

/**
 * Wrap an existing Postgres for use with the helpers below. The URL must
 * point at a superuser DB (we issue `CREATE DATABASE` against it).
 */
export function connectExisting(url: string): PostgresHandle {
  const u = new URL(url);
  // Allow socket-style URLs: `postgres:///db?host=/path/to/sock`.
  const sockHost = u.searchParams.get("host");
  const host = sockHost ?? u.hostname;
  return {
    host,
    port: u.port ? Number(u.port) : 5432,
    user: decodeURIComponent(u.username || "postgres"),
    password: decodeURIComponent(u.password),
    database: (u.pathname || "/postgres").slice(1) || "postgres",
    async deinit() {
      // no-op — we don't own the server
    },
  };
}

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
export async function makeTemplate(opts: MakeTemplateOptions): Promise<string> {
  const name = opts.name ?? "swell_template";
  const files = await resolveMigrationFiles(opts.migrations);
  const extras = opts.extraSchemas ?? [];
  const fingerprint = await hashFiles([...files, ...extras]);

  const root = postgres({
    host: opts.handle.host,
    port: opts.handle.port,
    user: opts.handle.user,
    password: opts.handle.password,
    database: opts.handle.database,
  });

  try {
    // Advisory lock so concurrent test processes don't race on template setup.
    // 1894123573 is just a random 32-bit constant; collisions are harmless.
    await root.unsafe("SELECT pg_advisory_lock(1894123573)");

    const existing = await root.unsafe(
      `SELECT 1 FROM pg_database WHERE datname = $1`,
      [name],
    );
    if (existing.length > 0) {
      const meta = await root.unsafe(
        `SELECT description FROM pg_shdescription
           JOIN pg_database ON pg_shdescription.objoid = pg_database.oid
           WHERE pg_database.datname = $1`,
        [name],
      );
      const recorded = meta[0]?.description as string | undefined;
      if (recorded === fingerprint) {
        await root.unsafe("SELECT pg_advisory_unlock(1894123573)");
        await root.end({ timeout: 1 });
        return name;
      }
      // Different fingerprint → rebuild. Drop any active connections first.
      await root.unsafe(
        `SELECT pg_terminate_backend(pid) FROM pg_stat_activity
           WHERE datname = $1 AND pid <> pg_backend_pid()`,
        [name],
      );
      await root.unsafe(`DROP DATABASE ${quoteIdent(name)}`);
    }

    await root.unsafe(`CREATE DATABASE ${quoteIdent(name)}`);
  } finally {
    await root.unsafe("SELECT pg_advisory_unlock(1894123573)").catch(() => {});
    await root.end({ timeout: 1 });
  }

  // Apply migrations + extra schemas inside the new template.
  const tpl = postgres({
    host: opts.handle.host,
    port: opts.handle.port,
    user: opts.handle.user,
    password: opts.handle.password,
    database: name,
  });
  try {
    for (const path of [...files, ...extras]) {
      const sql = readFileSync(path, "utf8");
      await tpl.unsafe(sql);
    }
  } finally {
    await tpl.end({ timeout: 1 });
  }

  // Record the fingerprint on the template DB for the next run.
  const root2 = postgres({
    host: opts.handle.host,
    port: opts.handle.port,
    user: opts.handle.user,
    password: opts.handle.password,
    database: opts.handle.database,
  });
  try {
    await root2.unsafe(
      `COMMENT ON DATABASE ${quoteIdent(name)} IS ${quoteLit(fingerprint)}`,
    );
  } finally {
    await root2.end({ timeout: 1 });
  }

  return name;
}

export interface WithTestDbOptions<T> {
  handle: PostgresHandle;
  template: string;
  fn(sql: TypedSql, meta: { dbName: string }): Promise<T>;
}

/**
 * Clone the template into a fresh test DB, run `fn`, drop the clone. Each
 * invocation gets a unique random name so concurrent calls don't collide.
 */
export async function withTestDb<T>(opts: WithTestDbOptions<T>): Promise<T> {
  const dbName = `swell_test_${randomHex(8)}`;

  const root = postgres({
    host: opts.handle.host,
    port: opts.handle.port,
    user: opts.handle.user,
    password: opts.handle.password,
    database: opts.handle.database,
  });
  try {
    await root.unsafe(
      `CREATE DATABASE ${quoteIdent(dbName)} WITH TEMPLATE ${quoteIdent(opts.template)}`,
    );
  } finally {
    await root.end({ timeout: 1 });
  }

  const conn = postgres({
    host: opts.handle.host,
    port: opts.handle.port,
    user: opts.handle.user,
    password: opts.handle.password,
    database: dbName,
  });
  const sql = createTypedSql(conn);
  try {
    return await opts.fn(sql, { dbName });
  } finally {
    await sql.end({ timeout: 1 });

    const cleanup = postgres({
      host: opts.handle.host,
      port: opts.handle.port,
      user: opts.handle.user,
      password: opts.handle.password,
      database: opts.handle.database,
    });
    try {
      // Force-disconnect any lingering connections then drop.
      await cleanup.unsafe(
        `SELECT pg_terminate_backend(pid) FROM pg_stat_activity
           WHERE datname = $1 AND pid <> pg_backend_pid()`,
        [dbName],
      );
      await cleanup.unsafe(`DROP DATABASE IF EXISTS ${quoteIdent(dbName)}`);
    } finally {
      await cleanup.end({ timeout: 1 });
    }
  }
}

// ---------------------------------------------------------------------------

async function resolveMigrationFiles(spec: string | string[]): Promise<string[]> {
  const items = Array.isArray(spec) ? spec : [spec];
  const out: string[] = [];
  for (const item of items) {
    if (item.includes("*")) {
      // Trivial glob: only support `<dir>/*.sql` or `<dir>/**/*.sql`. We
      // don't pull in a glob library — keep dependencies minimal.
      const m = item.match(/^(.*?)(\/\*\*)?\/\*\.([a-z]+)$/i);
      if (!m) {
        throw new Error(`swell/testing: unsupported glob "${item}". Use <dir>/*.sql.`);
      }
      const dir = m[1];
      const ext = m[3]!;
      const recursive = m[2] === "/**";
      out.push(...listFiles(dir!, ext, recursive));
    } else {
      out.push(resolve(item));
    }
  }
  out.sort();
  return out;
}

function listFiles(dir: string, ext: string, recursive: boolean): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (recursive) out.push(...listFiles(full, ext, recursive));
    } else if (entry.isFile() && entry.name.endsWith(`.${ext}`)) {
      out.push(full);
    }
  }
  return out;
}

async function hashFiles(paths: string[]): Promise<string> {
  const h = createHash("sha256");
  for (const p of paths.sort()) {
    h.update(p + " ");
    h.update(readFileSync(p));
    h.update("");
  }
  return h.digest("hex");
}

function quoteIdent(name: string): string {
  if (!/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(name)) {
    throw new Error(`swell/testing: refusing unsafe identifier ${JSON.stringify(name)}`);
  }
  return `"${name}"`;
}

function quoteLit(value: string): string {
  return `'${value.replace(/'/g, "''")}'`;
}

function randomHex(bytes: number): string {
  return createHash("sha256")
    .update(`${process.pid}-${Date.now()}-${Math.random()}`)
    .digest("hex")
    .slice(0, bytes * 2);
}

async function waitFor(check: () => Promise<boolean>, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let delay = 50;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise((res) => setTimeout(res, delay));
    delay = Math.min(delay * 1.5, 500);
  }
  throw new Error(`swell/testing: timed out after ${timeoutMs}ms waiting for condition`);
}
