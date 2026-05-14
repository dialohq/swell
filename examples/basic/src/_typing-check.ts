// Compile-only checks. Each assertion's annotation pins the exact row
// shape pg's augmented `Pool.query` narrows to. `tsc --noEmit` on this
// file passes iff the generated `q` overload brands `SqlText<P, R>`
// correctly for every registered SQL string.

import { q } from "swell";
import { pool } from "./db";

declare const userId: string;
declare const orgId: string;

async function checks() {
  const _1: { id: string; email: string }[] =
    (await pool.query(q("SELECT id, email FROM users WHERE id = $1"), [userId])).rows;

  const _2: { display_name: string | null }[] =
    (await pool.query(q("SELECT display_name FROM users WHERE id = $1"), [userId])).rows;

  const _3: { role: "admin" | "member" }[] =
    (await pool.query(q("SELECT role FROM users WHERE id = $1"), [userId])).rows;

  const _4: { n: string }[] =
    (await pool.query(q("SELECT count(*) AS n FROM users WHERE org_id = $1"), [orgId])).rows;

  // LEFT JOIN — p.body is NOT NULL on the base table but becomes nullable
  // through the outer join.
  const _5: { email: string; body: string | null; published_at: Date | null }[] =
    (await pool.query(
      q("SELECT u.email, p.body, p.published_at FROM users u LEFT JOIN posts p ON p.author_id = u.id WHERE u.org_id = $1"),
      [orgId],
    )).rows;

  // SQLx-style `"label!"` override forces NOT NULL on the inferred column.
  const _6: { id: string; label: string }[] =
    (await pool.query(
      q(`SELECT id, coalesce(display_name, email) AS "label!" FROM users WHERE id = $1`),
      [userId],
    )).rows;

  const _7: { profile: { id: string; email: string; name: string | null } }[] =
    (await pool.query(
      q(`SELECT jsonb_build_object('id', u.id, 'email', u.email, 'name', u.display_name) AS profile FROM users u WHERE u.id = $1`),
      [userId],
    )).rows;

  // `jsonb_agg` returns NULL on empty input (conservative; coerce with
  // `coalesce(..., '[]'::jsonb)` in SQL or override `as "x!"` if the
  // GROUP BY guarantees ≥1 row).
  const _8: { name: string; members: { id: string; email: string }[] | null }[] =
    (await pool.query(
      q(`SELECT o.name, jsonb_agg(jsonb_build_object('id', u.id, 'email', u.email)) AS members FROM orgs o JOIN users u ON u.org_id = o.id WHERE o.id = $1 GROUP BY o.id, o.name`),
      [orgId],
    )).rows;

  return [_1, _2, _3, _4, _5, _6, _7, _8];
}

void checks;
