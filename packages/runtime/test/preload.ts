import { resolve } from "node:path";
import {
  connectExisting,
  initStandardPostgres,
  makeTemplate,
  type PostgresHandle,
} from "../src/testing.ts";

// Run as a side-effect at module load. `bun test --preload` evaluates this
// file once before any test files; top-level await is the simplest way to
// ensure the template exists before tests construct clones from it.
const url = process.env.DATABASE_URL;
const handle: PostgresHandle = url ? connectExisting(url) : await initStandardPostgres();
(globalThis as { __SWELL_PG__?: PostgresHandle }).__SWELL_PG__ = handle;

await makeTemplate({
  handle,
  migrations: resolve(import.meta.dir, "./fixtures/migrations/*.sql"),
  name: "swell_runtime_template",
});

// Clean shutdown when bun exits. `process.on("exit", ...)` runs synchronously
// so we use beforeExit to allow async cleanup.
process.on("beforeExit", () => {
  void handle.deinit();
});
