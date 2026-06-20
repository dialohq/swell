# explicit cast

```sql
SELECT $1::int4 + 1 AS n
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT $1::int4 + 1 AS n": { params: [number | null]; row: { n: number | null } };
  }
}
```
