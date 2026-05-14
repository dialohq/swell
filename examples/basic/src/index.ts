import { q } from "swell";
import { pool } from "./db";

async function main() {
  const userId = "00000000-0000-0000-0000-000000000000";
  const orgId  = "11111111-1111-1111-1111-111111111111";

  // 1. Param + non-null cols.
  const u = await pool.query(q("SELECT id, email FROM users WHERE id = $1"), [userId]);

  // 2. Nullable base column.
  const dn = await pool.query(q("SELECT display_name FROM users WHERE id = $1"), [userId]);

  // 3. Enum (custom user_role type).
  const r = await pool.query(q("SELECT role FROM users WHERE id = $1"), [userId]);

  // 4. count(*) — bigint → string in node-pg, never null.
  const c = await pool.query(q("SELECT count(*) AS n FROM users WHERE org_id = $1"), [orgId]);

  // 5. LEFT JOIN — body is NOT NULL on table but becomes nullable here.
  const feed = await pool.query(
    q("SELECT u.email, p.body, p.published_at FROM users u LEFT JOIN posts p ON p.author_id = u.id WHERE u.org_id = $1"),
    [orgId],
  );

  // 6. SQLx-style override: force NOT NULL via "label!".
  const profile = await pool.query(
    q(`SELECT id, coalesce(display_name, email) AS "label!" FROM users WHERE id = $1`),
    [userId],
  );

  // 7. JSON shape inference: jsonb_build_object → structural object.
  const dash = await pool.query(
    q(`SELECT jsonb_build_object('id', u.id, 'email', u.email, 'name', u.display_name) AS profile FROM users u WHERE u.id = $1`),
    [userId],
  );

  // 8. JSON aggregate over a JSON-built object.
  const orgFeed = await pool.query(
    q(`SELECT o.name, jsonb_agg(jsonb_build_object('id', u.id, 'email', u.email)) AS members FROM orgs o JOIN users u ON u.org_id = o.id WHERE o.id = $1 GROUP BY o.id, o.name`),
    [orgId],
  );

  console.log({ u, dn, r, c, feed, profile, dash, orgFeed });
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((err) => { console.error(err); process.exit(1); });
}
