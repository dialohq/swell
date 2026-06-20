# coalesce with literal fallback

```sql
SELECT coalesce(sum(amount_cents), 0) AS total_cents
FROM billing.invoices WHERE workspace_id = $1 AND status = 'paid'
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT coalesce(sum(amount_cents), 0) AS total_cents
FROM billing.invoices WHERE workspace_id = $1 AND status = 'paid'`]: { params: [string | null]; row: { total_cents: string } };
  }
}
```
