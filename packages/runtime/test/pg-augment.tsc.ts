// Compile-time check for the pg augmentation. Run via `tsc --noEmit`
// to confirm pg's `Pool/Client/PoolClient.query(...)` narrows row +
// params when fed a `SqlText<P, R>`-branded string. The augmentation
// auto-loads from `../src/index`; pulling `q` (or anything else) in is
// enough.

import type { Pool, Client, PoolClient } from "pg";
import { q, type SqlText } from "../src/index";

declare const pool: Pool;
declare const client: Client;
declare const pc: PoolClient;
declare const userId: string;

// Codegen-emitted typed `q` overload — construct manually here to keep
// the test self-contained (no fixture DB / no `swell.generated.ts`).
declare function typedQ(
  text: "SELECT id, email FROM users WHERE id = $1",
): SqlText<[string], { id: string; email: string }>;

// Type-equality helper. Errors if X and Y aren't structurally identical.
type Equals<X, Y> =
  (<T>() => T extends X ? 1 : 2) extends (<T>() => T extends Y ? 1 : 2) ? true : false;
type AssertTrue<T extends true> = T;

async function checks() {
  const stmt = typedQ("SELECT id, email FROM users WHERE id = $1");

  // Pool.query — typed row + params via the SqlText brand. We assert
  // that the inferred row is EXACTLY `{ id: string; email: string }` —
  // not `any`, which would mean the augmentation overload didn't fire
  // and the permissive `string | QueryConfig` overload caught it instead.
  const r1 = await pool.query(stmt, [userId]);
  type R1 = (typeof r1.rows)[number];
  type _ok1 = AssertTrue<Equals<R1, { id: string; email: string }>>;

  // Client.query — same path.
  const r2 = await client.query(stmt, [userId]);
  type R2 = (typeof r2.rows)[number];
  type _ok2 = AssertTrue<Equals<R2, { id: string; email: string }>>;

  // PoolClient.query — inherits via Client → ClientBase.
  const r3 = await pc.query(stmt, [userId]);
  type R3 = (typeof r3.rows)[number];
  type _ok3 = AssertTrue<Equals<R3, { id: string; email: string }>>;

  void [r1, r2, r3];
  void q;
}

void checks;
