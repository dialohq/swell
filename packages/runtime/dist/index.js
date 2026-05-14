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
export function q(text) {
    return text;
}
// Side-effect import: activates the `pg` module augmentation for
// `Pool/Client/PoolClient.query` whenever swell is loaded. Consumers
// that don't install pg get the standard "Cannot find module" error
// from the augmentation file — pg is an optional peer dep for a reason.
import "./pg";
//# sourceMappingURL=index.js.map