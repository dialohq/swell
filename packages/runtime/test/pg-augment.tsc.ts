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

  // Negative cases: SqlText with mismatched values must ERROR, not
  // silently resolve to `QueryResult<any>` via pg's stock `string`
  // overload. Strictness rides on two pieces:
  //   - `SqlText` is *not* `string` at the type level, so pg's stock
  //     `query(text: string, …)` overload can't catch a q-marked call.
  //   - `values?: NoInfer<P>` anchors P solely on the SqlText, so
  //     `["a", "b"]` can't widen P to a supertype that happens to fit.
  // Each `@ts-expect-error` is itself an assertion: if any of these
  // start type-checking, tsc errors with "unused @ts-expect-error".

  // Wrong element type (number vs string).
  // @ts-expect-error pool.query: number doesn't fit P=[string]
  await pool.query(stmt, [42]);
  // @ts-expect-error client.query: same
  await client.query(stmt, [42]);
  // @ts-expect-error pc.query: same
  await pc.query(stmt, [42]);

  // Wrong arity (two values for a one-param query).
  // @ts-expect-error pool.query: ["a","b"] doesn't fit P=[string]
  await pool.query(stmt, ["a", "b"]);

  void [r1, r2, r3];
  void q;
}

void checks;
