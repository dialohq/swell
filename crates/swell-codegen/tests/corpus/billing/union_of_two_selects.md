# union of two selects

```sql
SELECT id, 'paid' AS bucket FROM billing.invoices WHERE status = 'paid'
UNION ALL
SELECT id, 'open' FROM billing.invoices WHERE status = 'open'
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT id, 'paid' AS bucket FROM billing.invoices WHERE status = 'paid'
UNION ALL
SELECT id, 'open' FROM billing.invoices WHERE status = 'open'`]: { params: []; row: { id: string | null; bucket: string | null } };
  }
}
```
