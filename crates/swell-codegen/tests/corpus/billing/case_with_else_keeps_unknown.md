# case with else keeps unknown

```sql
SELECT
    CASE WHEN status = 'paid' THEN amount_cents ELSE 0 END AS recognised,
    CASE WHEN status = 'paid' THEN amount_cents END AS pending
FROM billing.invoices WHERE id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT
    CASE WHEN status = 'paid' THEN amount_cents ELSE 0 END AS recognised,
    CASE WHEN status = 'paid' THEN amount_cents END AS pending
FROM billing.invoices WHERE id = $1`]: { params: [string | null]; row: { recognised: string; pending: string | null } };
  }
}
```
