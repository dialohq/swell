// Compile-time check for the pg augmentation. Run via `tsc --noEmit`
// to confirm pg's query() narrows when fed a q-branded SQL string.
// The augmentation auto-loads from `../src/index` (no explicit
// `import "../src/pg"`); pulling in `q` is enough.

import type { Pool, Client, PoolClient } from "pg";
import { q } from "../src/index";

declare const pool: Pool;
declare const client: Client;
declare const pc: PoolClient;
declare const userId: string;

// Pre-seed the registry with a fake entry — normally this comes from
// codegen but we want a deterministic test fixture.
declare module "../src/index" {
  interface Registry {
    "SELECT id, email FROM users WHERE id = $1": {
      params: [string];
      row: { id: string; email: string };
    };
    "UPDATE users SET email = $2 WHERE id = $1": {
      params: [string, string];
      row: never;
    };
  }
}

// Type-equality helper. Errors if X and Y aren't structurally identical.
type Equals<X, Y> =
  (<T>() => T extends X ? 1 : 2) extends (<T>() => T extends Y ? 1 : 2) ? true : false;
type AssertTrue<T extends true> = T;

async function checks() {
  const stmt = q("SELECT id, email FROM users WHERE id = $1");

  // Pool.query — typed row + params via the registry lookup. We assert
  // that the inferred row is EXACTLY `{ id: string; email: string }` —
  // not `any`, which would mean the augmentation overload didn't fire
  // and the existing permissive overload caught it instead.
  const r1 = await pool.query(stmt, [userId]);
  type R1 = (typeof r1.rows)[number];
  type _ok1 = AssertTrue<Equals<R1, { id: string; email: string }>>;

  // Client.query — same path.
  const r2 = await client.query(stmt, [userId]);
  type R2 = (typeof r2.rows)[number];
  type _ok2 = AssertTrue<Equals<R2, { id: string; email: string }>>;

  // PoolClient.query — same path (inherits from Client → ClientBase).
  const r3 = await pc.query(stmt, [userId]);
  type R3 = (typeof r3.rows)[number];
  type _ok3 = AssertTrue<Equals<R3, { id: string; email: string }>>;

  // Write-only registry entry (row: never). Params narrow to [string, string]
  // so the SqlText overload still fires.
  const update = q("UPDATE users SET email = $2 WHERE id = $1");
  await pool.query(update, [userId, "new@x.com"]);

  void [r1, r2, r3];
}

void checks;
