# values derived table

```sql
SELECT t.k, t.v
FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c')) AS t(k, v)
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT t.k, t.v
FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c')) AS t(k, v)`]: { params: []; row: { k: number | null; v: string | null } };
  }
}
```
