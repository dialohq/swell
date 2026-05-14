/**
 * swell runtime — `q()` SQL marker + pg type augmentation.
 *
 * Wrap a static SQL string with `q(...)` and pass it to the augmented
 * pg `Pool` / `Client` / `PoolClient` `.query(...)`:
 *
 *   import { q } from "swell";  // or from "./swell.generated" for typed q
 *   const stmt = q("SELECT id, email FROM users WHERE id = $1");
 *   const { rows } = await pool.query(stmt, [userId]);
 *   //      ^? typed by swell's analyzer from the live DB
 *
 * Codegen output (`swell.generated.ts`) emits a typed `q` overload per
 * registered query — that's where row + param narrowing comes from. The
 * runtime `q` exported below is the permissive fallback for SQL that
 * hasn't been indexed yet.
 */

/**
 * Recursive structural type for `json` / `jsonb` columns. Postgres's
 * DESCRIBE only tells us "this is a json blob" — the value-level shape is
 * left to runtime narrowing (zod, decoders, etc.) at the boundary.
 */
export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };

/**
 * Branded SQL string carried by `q("…")`. The non-optional `__sqlBrand`
 * intersection makes plain strings *not* assignable, so the augmented
 * `pg.Pool.query(...)` overload only fires for q-marked text.
 */
export type SqlText<P extends unknown[], R> = string & {
  readonly __sqlBrand: { params: P; row: R };
};

/**
 * No-op SQL marker. Runtime cost is zero (the cast carries the brand at
 * the type level only). The codegen output's typed `q` overloads pin
 * the brand to the live-DB-inferred `{ params; row }` shape for known
 * literals; this fallback covers anything that hasn't been indexed.
 */
export function q<S extends string>(text: S): SqlText<unknown[], unknown> {
  return text as never;
}

// Side-effect import: activates the `pg` module augmentation for
// `Pool/Client/PoolClient.query` whenever swell is loaded. Consumers
// that don't install pg get the standard "Cannot find module" error
// from the augmentation file — pg is an optional peer dep for a reason.
import "./pg";
