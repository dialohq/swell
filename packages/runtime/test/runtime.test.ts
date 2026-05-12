import { describe, expect, test } from "bun:test";
import type { PostgresHandle } from "../src/testing.ts";
import { withTestDb } from "../src/testing.ts";

function pg(): PostgresHandle {
  const h = (globalThis as { __SWELL_PG__?: PostgresHandle }).__SWELL_PG__;
  if (!h) throw new Error("preload.ts did not run");
  return h;
}

const TEMPLATE = "swell_runtime_template";

const ORG = "11111111-1111-1111-1111-111111111111";
const ACCT1 = "22222222-2222-2222-2222-222222222222";
const ACCT2 = "33333333-3333-3333-3333-333333333333";

describe("runtime — basic query", () => {
  test("call form returns rows", async () => {
    await withTestDb({
      handle: pg(),
      template: TEMPLATE,
      fn: async (sql) => {
        await sql.exec("INSERT INTO orgs (id, name) VALUES ($1, $2)", ORG, "acme");
        const rows = await sql("SELECT id, name FROM orgs WHERE id = $1", ORG);
        expect(rows.length).toBe(1);
        expect(rows[0]).toEqual({ id: ORG, name: "acme" });
      },
    });
  });

  test(".one throws on zero rows; ok on one", async () => {
    await withTestDb({
      handle: pg(),
      template: TEMPLATE,
      fn: async (sql) => {
        await expect(
          sql.one("SELECT id FROM orgs WHERE id = $1", ORG),
        ).rejects.toThrow(/expected exactly one row, got 0/);

        await sql.exec("INSERT INTO orgs (id, name) VALUES ($1, $2)", ORG, "acme");
        const row = await sql.one("SELECT id, name FROM orgs WHERE id = $1", ORG);
        expect(row).toEqual({ id: ORG, name: "acme" });
      },
    });
  });

  test(".maybe returns null on zero, row on one, throws on many", async () => {
    await withTestDb({
      handle: pg(),
      template: TEMPLATE,
      fn: async (sql) => {
        const empty = await sql.maybe("SELECT id FROM orgs WHERE id = $1", ORG);
        expect(empty).toBeNull();

        await sql.exec("INSERT INTO orgs (id, name) VALUES ($1, $2)", ORG, "acme");
        const one = await sql.maybe("SELECT id FROM orgs WHERE id = $1", ORG);
        expect(one).toEqual({ id: ORG });

        const ORG2 = "44444444-4444-4444-4444-444444444444";
        await sql.exec("INSERT INTO orgs (id, name) VALUES ($1, $2)", ORG2, "beta");
        await expect(
          sql.maybe("SELECT id FROM orgs"),
        ).rejects.toThrow(/expected zero or one row, got 2/);
      },
    });
  });

  test(".exec reports affected rowCount", async () => {
    await withTestDb({
      handle: pg(),
      template: TEMPLATE,
      fn: async (sql) => {
        const r1 = await sql.exec("INSERT INTO orgs (id, name) VALUES ($1, $2)", ORG, "acme");
        expect(r1.rowCount).toBe(1);
        const r2 = await sql.exec("UPDATE orgs SET name = $2 WHERE id = $1", ORG, "renamed");
        expect(r2.rowCount).toBe(1);
        const r3 = await sql.exec(
          "DELETE FROM orgs WHERE id = $1",
          "55555555-5555-5555-5555-555555555555",
        );
        expect(r3.rowCount).toBe(0);
      },
    });
  });
});

describe("runtime — transactions", () => {
  test("begin commits on resolve", async () => {
    await withTestDb({
      handle: pg(),
      template: TEMPLATE,
      fn: async (sql) => {
        await sql.exec("INSERT INTO bank_accounts (id, balance) VALUES ($1, $2)", ACCT1, 100);
        await sql.exec("INSERT INTO bank_accounts (id, balance) VALUES ($1, $2)", ACCT2, 100);

        await sql.begin(async (tx) => {
          await tx.exec("UPDATE bank_accounts SET balance = balance - 50 WHERE id = $1", ACCT1);
          await tx.exec("UPDATE bank_accounts SET balance = balance + 50 WHERE id = $1", ACCT2);
        });

        const a1 = await sql.one("SELECT balance FROM bank_accounts WHERE id = $1", ACCT1);
        const a2 = await sql.one("SELECT balance FROM bank_accounts WHERE id = $1", ACCT2);
        expect(a1).toEqual({ balance: "50" });
        expect(a2).toEqual({ balance: "150" });
      },
    });
  });

  test("begin rolls back on throw", async () => {
    await withTestDb({
      handle: pg(),
      template: TEMPLATE,
      fn: async (sql) => {
        await sql.exec("INSERT INTO bank_accounts (id, balance) VALUES ($1, $2)", ACCT1, 100);

        await expect(
          sql.begin(async (tx) => {
            await tx.exec(
              "UPDATE bank_accounts SET balance = balance - 50 WHERE id = $1",
              ACCT1,
            );
            throw new Error("oops");
          }),
        ).rejects.toThrow(/oops/);

        const a = await sql.one("SELECT balance FROM bank_accounts WHERE id = $1", ACCT1);
        expect(a).toEqual({ balance: "100" });
      },
    });
  });

  test("savepoint commits + rolls back independently of outer tx", async () => {
    await withTestDb({
      handle: pg(),
      template: TEMPLATE,
      fn: async (sql) => {
        await sql.exec("INSERT INTO bank_accounts (id, balance) VALUES ($1, $2)", ACCT1, 100);

        await sql.begin(async (tx) => {
          await tx.exec(
            "UPDATE bank_accounts SET balance = balance - 10 WHERE id = $1",
            ACCT1,
          );
          // Savepoint that rolls back — outer tx should still commit the -10.
          try {
            await tx.savepoint("inner", async (sp) => {
              await sp.exec(
                "UPDATE bank_accounts SET balance = balance - 1000 WHERE id = $1",
                ACCT1,
              );
              throw new Error("rollback the savepoint");
            });
          } catch {
            // expected
          }
        });

        const a = await sql.one("SELECT balance FROM bank_accounts WHERE id = $1", ACCT1);
        expect(a).toEqual({ balance: "90" });
      },
    });
  });
});

describe("runtime — isolation", () => {
  test("16 parallel withTestDb calls each see an empty schema", async () => {
    const ids = Array.from({ length: 16 }, (_, i) => i);
    await Promise.all(
      ids.map((i) =>
        withTestDb({
          handle: pg(),
          template: TEMPLATE,
          fn: async (sql) => {
            // Each clone starts with no orgs.
            const before = await sql("SELECT count(*) AS n FROM orgs");
            expect(before[0]).toEqual({ n: "0" });

            const id = `${i.toString().padStart(8, "0")}-0000-0000-0000-000000000000`;
            await sql.exec("INSERT INTO orgs (id, name) VALUES ($1, $2)", id, `org-${i}`);

            const after = await sql.one("SELECT name FROM orgs WHERE id = $1", id);
            expect(after).toEqual({ name: `org-${i}` });
          },
        }),
      ),
    );
  });
});
