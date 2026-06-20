# grouping sets make keys nullable

```sql
SELECT
    workspace_id,
    status,
    count(*) AS n
FROM billing.invoices
GROUP BY GROUPING SETS ((workspace_id), (status), ())
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
    [`SELECT
    workspace_id,
    status,
    count(*) AS n
FROM billing.invoices
GROUP BY GROUPING SETS ((workspace_id), (status), ())`]: { params: []; row: { workspace_id: BillingInvoices["workspace_id"]; status: BillingInvoices["status"]; n: string } };
  }
}
```
