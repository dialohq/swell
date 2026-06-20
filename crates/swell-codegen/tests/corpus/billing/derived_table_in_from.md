# derived table in from

```sql
SELECT t.workspace_id, t.cnt
FROM (
    SELECT workspace_id, count(*) AS cnt
    FROM billing.invoices GROUP BY workspace_id
) t
WHERE t.cnt > 5
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface BillingInvoices {
  id: string;
  subscription_id: string;
  workspace_id: string;
  number: string;
  status: "draft" | "open" | "paid" | "void" | "uncollectible";
  amount_cents: string;
  issued_at: Date;
  paid_at: Date | null;
  due_at: Date;
  notes: string | null;
}

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT t.workspace_id, t.cnt
FROM (
    SELECT workspace_id, count(*) AS cnt
    FROM billing.invoices GROUP BY workspace_id
) t
WHERE t.cnt > 5`]: { params: []; row: { workspace_id: BillingInvoices["workspace_id"]; cnt: string } };
  }
}
```
