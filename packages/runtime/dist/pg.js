// pg (node-postgres) integration. Two pieces:
//
//   1. Module augmentation adds a `SqlText`-aware overload to
//      `Pool.query` / `Client.query` / `PoolClient.query`, so when the
//      first argument went through `q(...)` the row + param types narrow
//      to whatever swell's analyzer inferred for that SQL. Auto-loaded by
//      `import "./pg"` (the user's tsconfig needs nothing).
//
//   2. An explicit `QueryType` export users can reach for when they
//      wrap pg's client (eg. building their own `dbQueryR`/`dbQueryW`
//      helpers). Augmentation alone can ADD overloads but not REMOVE
//      pg's stock `string` overload — so a q-marked call site that
//      passes wrong-typed values silently falls through to that stock
//      overload and resolves to `QueryResult<any>` instead of erroring.
//      `QueryType` is "what pg.Client.query *should* have been": the
//      `SqlText` overload first, then the rest of pg's overload set with
//      `string` replaced by `RawSql` so a `SqlText` can't match the
//      fallback path.
export {};
//# sourceMappingURL=pg.js.map