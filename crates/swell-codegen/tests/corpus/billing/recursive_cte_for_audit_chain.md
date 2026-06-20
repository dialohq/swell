# recursive cte for audit chain

```sql
WITH RECURSIVE n(level) AS (
    SELECT 0
    UNION ALL
    SELECT level + 1 FROM n WHERE level < 10
)
SELECT level FROM n
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`WITH RECURSIVE n(level) AS (
    SELECT 0
    UNION ALL
    SELECT level + 1 FROM n WHERE level < 10
)
SELECT level FROM n`]: { params: []; row: { level: number | null } };
  }
}
```
