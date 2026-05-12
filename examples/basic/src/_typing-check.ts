// Compile-only checks. Each block asserts the exact inferred type.
// `tsc --noEmit` on this file passes iff the generated overloads pick the
// right shape at every call site.

import { sql } from "./db";

declare const userId: string;
declare const orgId: string;

async function checks() {
  // 1. Scalar non-null.
  const r1: { id: string; email: string }[] =
    await sql("SELECT id, email FROM users WHERE id = $1", userId);
  void r1;

  // 2. Nullable base column.
  const r2: { display_name: string | null }[] =
    await sql("SELECT display_name FROM users WHERE id = $1", userId);
  void r2;

  // 3. Enum.
  const r3: { role: "admin" | "member" }[] =
    await sql("SELECT role FROM users WHERE id = $1", userId);
  void r3;

  // 4. count(*).
  const r4: { n: string }[] =
    await sql("SELECT count(*) AS n FROM users WHERE org_id = $1", orgId);
  void r4;

  // 5. LEFT JOIN — p.body becomes nullable.
  const r5: { email: string; body: string | null; published_at: Date | null }[] =
    await sql(
      "SELECT u.email, p.body, p.published_at FROM users u LEFT JOIN posts p ON p.author_id = u.id WHERE u.org_id = $1",
      orgId,
    );
  void r5;

  // 6. Override "label!" forces non-null.
  const r6: { id: string; label: string }[] =
    await sql(
      `SELECT id, coalesce(display_name, email) AS "label!" FROM users WHERE id = $1`,
      userId,
    );
  void r6;

  // 7. JSON shape.
  const r7: { profile: { id: string; email: string; name: string | null } }[] =
    await sql(
      `SELECT jsonb_build_object('id', u.id, 'email', u.email, 'name', u.display_name) AS profile FROM users u WHERE u.id = $1`,
      userId,
    );
  void r7;

  // 8. JSON aggregate — array of structural objects. `jsonb_agg` returns
  //    NULL on empty input, hence `| null` (this is conservative; coerce
  //    with `coalesce(..., '[]'::jsonb)` in SQL or override `as "x!"` if
  //    the GROUP BY guarantees ≥1 row).
  const r8: { name: string; members: { id: string; email: string }[] | null }[] =
    await sql(
      `SELECT o.name, jsonb_agg(jsonb_build_object('id', u.id, 'email', u.email)) AS members FROM orgs o JOIN users u ON u.org_id = o.id WHERE o.id = $1 GROUP BY o.id, o.name`,
      orgId,
    );
  void r8;
}

// API cardinality checks — sql.one / sql.maybe / sql.many / sql.exec must
// each narrow the row shape from the registry entry.
async function apiChecks() {
  const one: { id: string; email: string } =
    await sql.one("SELECT id, email FROM users WHERE id = $1", userId);
  void one;

  const maybe: { id: string; email: string } | null =
    await sql.maybe("SELECT id, email FROM users WHERE id = $1", userId);
  void maybe;

  const many: { id: string; email: string }[] =
    await sql.many("SELECT id, email FROM users WHERE id = $1", userId);
  void many;

  const exec: { rowCount: number } =
    await sql.exec("SELECT id, email FROM users WHERE id = $1", userId);
  void exec;
}

// Parameter-type strictness: registered call sites must reject too few /
// too many args and wrong-typed args at compile time. We use
// `@ts-expect-error` so the file fails to compile if swell ever regresses
// to the permissive `unknown[]` fallback.
//
// declared above: userId: string, orgId: string.
declare const someNumber: number;

async function paramStrictnessChecks() {
  // Correct: 1 string param, narrows to typed row.
  void (await sql.one("SELECT id, email FROM users WHERE id = $1", userId));

  // Missing required positional param.
  // @ts-expect-error too few args
  void (await sql.one("SELECT id, email FROM users WHERE id = $1"));

  // Extra positional param.
  // @ts-expect-error too many args
  void (await sql.one("SELECT id, email FROM users WHERE id = $1", userId, "extra"));

  // Wrong type for $1 (registry says string, we pass number).
  // @ts-expect-error wrong param type
  void (await sql.one("SELECT id, email FROM users WHERE id = $1", someNumber));

  // Same strictness on every method variant.
  // @ts-expect-error wrong param type for exec
  void (await sql.exec("SELECT id, email FROM users WHERE id = $1", someNumber));
  // @ts-expect-error wrong param type for maybe
  void (await sql.maybe("SELECT id, email FROM users WHERE id = $1", someNumber));
  // @ts-expect-error wrong param type for many
  void (await sql.many("SELECT id, email FROM users WHERE id = $1", someNumber));
  // @ts-expect-error wrong param type for call form
  void (await sql("SELECT id, email FROM users WHERE id = $1", someNumber));

  // And inside a transaction.
  await sql.begin(async (tx) => {
    void (await tx.one("SELECT id, email FROM users WHERE id = $1", userId));
    // @ts-expect-error wrong param type inside tx
    void (await tx.one("SELECT id, email FROM users WHERE id = $1", someNumber));
  });
}

// Transaction checks — `tx` inside `sql.begin` is itself a `TypedSql`,
// so every generated registry entry applies inside the callback.
async function txChecks() {
  const result: { id: string; email: string } = await sql.begin(async (tx) => {
    const row = await tx.one("SELECT id, email FROM users WHERE id = $1", userId);
    return row;
  });
  void result;

  const nested: { id: string } = await sql.begin(async (tx) => {
    return tx.savepoint("sp1", async (sp) => {
      const r = await sp.one("SELECT id, email FROM users WHERE id = $1", userId);
      return { id: r.id };
    });
  });
  void nested;
}

void [checks, apiChecks, paramStrictnessChecks, txChecks];
