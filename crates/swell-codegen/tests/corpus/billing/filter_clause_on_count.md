# filter clause on count

```sql
SELECT
    count(*) AS total,
    count(*) FILTER (WHERE status = 'paid') AS paid,
    count(*) FILTER (WHERE status = 'open') AS open
FROM billing.invoices WHERE workspace_id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT
    count(*) AS total,
    count(*) FILTER (WHERE status = 'paid') AS paid,
    count(*) FILTER (WHERE status = 'open') AS open
FROM billing.invoices WHERE workspace_id = $1`]: { params: [string | null]; row: { total: string; paid: string; open: string } };
  }
}
```
