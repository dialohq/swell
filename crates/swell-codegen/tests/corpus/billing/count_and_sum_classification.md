# count and sum classification

```sql
SELECT
    count(*) AS total_invoices,
    count(paid_at) AS paid_count,
    sum(amount_cents) AS total_cents,
    avg(amount_cents) AS avg_cents,
    min(issued_at) AS earliest,
    max(issued_at) AS latest
FROM billing.invoices
WHERE workspace_id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT
    count(*) AS total_invoices,
    count(paid_at) AS paid_count,
    sum(amount_cents) AS total_cents,
    avg(amount_cents) AS avg_cents,
    min(issued_at) AS earliest,
    max(issued_at) AS latest
FROM billing.invoices
WHERE workspace_id = $1`]: { params: [string | null]; row: { total_invoices: string; paid_count: string; total_cents: string | null; avg_cents: string | null; earliest: Date | null; latest: Date | null } };
  }
}
```
