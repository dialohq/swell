// Compile-only checks. Each block asserts the exact inferred type narrows
// through pg's augmented `Pool.query`. `tsc --noEmit` on this file passes
// iff the generated `q` overload pins the right `SqlText<P, R>` brand.

import { pool, q } from "./db";

declare const userId: string;
declare const orgId: string;

async function checks() {
  // 1. Scalar non-null.
  const r1 = await pool.query(q("SELECT id, email FROM users WHERE id = $1"), [userId]);
  const _r1: { id: string; email: string }[] = r1.rows;
  void _r1;

  // 2. Nullable base column.
  const r2 = await pool.query(q("SELECT display_name FROM users WHERE id = $1"), [userId]);
  const _r2: { display_name: string | null }[] = r2.rows;
  void _r2;

  // 3. Enum.
  const r3 = await pool.query(q("SELECT role FROM users WHERE id = $1"), [userId]);
  const _r3: { role: "admin" | "member" }[] = r3.rows;
  void _r3;

  // 4. count(*).
  const r4 = await pool.query(q("SELECT count(*) AS n FROM users WHERE org_id = $1"), [orgId]);
  const _r4: { n: string }[] = r4.rows;
  void _r4;

  // 5. LEFT JOIN — p.body becomes nullable.
  const r5 = await pool.query(
    q("SELECT u.email, p.body, p.published_at FROM users u LEFT JOIN posts p ON p.author_id = u.id WHERE u.org_id = $1"),
    [orgId],
  );
  const _r5: { email: string; body: string | null; published_at: Date | null }[] = r5.rows;
  void _r5;

  // 6. Override "label!" forces non-null.
  const r6 = await pool.query(
    q(`SELECT id, coalesce(display_name, email) AS "label!" FROM users WHERE id = $1`),
    [userId],
  );
  const _r6: { id: string; label: string }[] = r6.rows;
  void _r6;

  // 7. JSON shape.
  const r7 = await pool.query(
    q(`SELECT jsonb_build_object('id', u.id, 'email', u.email, 'name', u.display_name) AS profile FROM users u WHERE u.id = $1`),
    [userId],
  );
  const _r7: { profile: { id: string; email: string; name: string | null } }[] = r7.rows;
  void _r7;

  // 8. JSON aggregate — array of structural objects. `jsonb_agg` returns
  //    NULL on empty input, hence `| null` (this is conservative; coerce
  //    with `coalesce(..., '[]'::jsonb)` in SQL or override `as "x!"` if
  //    the GROUP BY guarantees ≥1 row).
  const r8 = await pool.query(
    q(`SELECT o.name, jsonb_agg(jsonb_build_object('id', u.id, 'email', u.email)) AS members FROM orgs o JOIN users u ON u.org_id = o.id WHERE o.id = $1 GROUP BY o.id, o.name`),
    [orgId],
  );
  const _r8: { name: string; members: { id: string; email: string }[] | null }[] = r8.rows;
  void _r8;
}

void checks;
