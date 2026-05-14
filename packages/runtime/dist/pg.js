// Module augmentation for `pg` (node-postgres). Adds a SqlText-aware
// overload to `Pool.query` / `Client.query` / `PoolClient.query`, so
// when the first argument went through `q(...)` the row + param types
// narrow to whatever swell's analyzer inferred for that SQL.
//
// Auto-loaded by `import "./pg"` from this package's main entry — the
// user's tsconfig needs nothing.
export {};
//# sourceMappingURL=pg.js.map