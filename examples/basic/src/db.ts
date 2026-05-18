import { Pool } from "pg";
// Loads two augmentations: swell's `declare module "pg"` (gives
// `pool.query(q(…), […])` row + param narrowing) and this package's
// `declare module "swell"` Registry entries (drives the narrowing).
import "@dialo/swell";
import "./swell.generated";

export const pool = new Pool();
