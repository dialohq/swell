/**
 * swell runtime — `q()` SQL marker + pg type augmentation.
 *
 * Wrap a static SQL string with `q(...)` and pass it to the augmented
 * pg `Pool` / `Client` / `PoolClient` `.query(...)`:
 *
 *   import "./swell.generated";   // loads the Registry augmentation
 *   import { q } from "swell";
 *   const { rows } = await pool.query(
 *     q("SELECT id, email FROM users WHERE id = $1"),
 *     [userId],
 *   );
 *   //      ^? typed by swell's analyzer from the live DB
 *
 * Each package's codegen output (`swell.generated.ts`) is pure
 * `declare module "swell"` augmentation of the `Registry` interface
 * below — `keyof Registry` becomes the union of analysed SQL strings,
 * and `q`'s strict overload narrows on that. Non-literal queries fall
 * through to the permissive overload.
 */

/**
 * Recursive structural type for `json` / `jsonb` columns. Postgres's
 * DESCRIBE only tells us "this is a json blob" — the value-level shape is
 * left to runtime narrowing (zod, decoders, etc.) at the boundary.
 *
 * Object values admit `undefined` so callers can pass `{ a, b: maybeB }`
 * to a jsonb param without round-tripping through JSON.stringify just to
 * drop optional fields. `JSON.stringify` (which pg uses on the wire) and
 * `jsonb_build_object` both treat `undefined` and an absent key the same
 * way, so this widening matches the runtime contract.
 */
export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json | undefined };

/**
 * Branded SQL string carried by `q("…")`. The non-optional `__sqlBrand`
 * intersection makes plain strings *not* assignable, so the augmented
 * `pg.Pool.query(...)` overload only fires for q-marked text.
 */
export type SqlText<P extends unknown[], R> = string & {
  readonly __sqlBrand: { params: P; row: R };
};

/**
 * Per-compilation-unit registry of analysed SQL strings. Empty by
 * default; each package's generated `swell.generated.ts` extends it
 * via `declare module "swell"`. Interface merging is scoped to the
 * importing TS project, so packages don't bleed into each other.
 */
export interface Registry {}

/**
 * No-op SQL marker. Runtime cost is zero (the cast carries the brand at
 * the type level only). The strict overload reads `keyof Registry` —
 * augmented by each package's generated file — and pins the brand to
 * the live-DB-inferred shape. The permissive fallback covers anything
 * not in the Registry (dynamic SQL, queries not yet indexed).
 */
export function q<S extends keyof Registry & string>(
  text: S,
): SqlText<
  Registry[S] extends { params: infer P extends unknown[] } ? P : never,
  Registry[S] extends { row: infer R } ? R : never
>;
export function q<S extends string>(text: S): SqlText<unknown[], unknown>;
export function q(text: string): SqlText<unknown[], unknown> {
  return text as never;
}

// Side-effect import: activates the `pg` module augmentation for
// `Pool/Client/PoolClient.query` whenever swell is loaded. Consumers
// that don't install pg get the standard "Cannot find module" error
// from the augmentation file — pg is an optional peer dep for a reason.
import "./pg.js";

// Also re-export pg-specific helpers from the main entry. Consumers on
// `moduleResolution: "node"` (Node10) can't reach `swell/pg` subpath
// exports — folding the types into the main entry keeps the wiring
// uniform regardless of resolution mode.
export type { RawSql, QueryType } from "./pg.js";
