import { Pool } from "pg";
// Side-effect: applies swell's `declare module "pg"` augmentation so
// `pool.query(q(…), […])` narrows rows + params to the registry shape.
import "swell";

export { q } from "./swell.generated";
export const pool = new Pool();
