/**
 * Core type definitions shared between the main entry and the pg
 * augmentation. Lives in its own file so `index.ts` and `pg.ts` don't
 * have to import from each other (the type-only cycle confuses some
 * downstream toolchains).
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
 * Branded SQL marker carried by `q("…")`. At runtime this *is* the SQL
 * string (`q` is a no-op cast); at the type level we deliberately drop
 * the `string &` intersection so a `SqlText` is **not** assignable to
 * `string`. That gap is what makes the pg `.query(...)` augmentation
 * strict: wrong-typed values can't silently fall through to pg's stock
 * `query(text: string, …)` overload — there's no `string` to fall
 * through *to*. Module augmentation can only add overloads, never
 * remove them, so we make the brand un-string-like instead.
 */
export type SqlText<P extends unknown[], R> = {
  readonly __sqlBrand: { params: P; row: R };
};

/**
 * Per-compilation-unit registry of analysed SQL strings. Empty by
 * default; each package's generated `swell.generated.ts` extends it
 * via `declare module "swell"`. Interface merging is scoped to the
 * importing TS project, so packages don't bleed into each other.
 */
export interface Registry {}
