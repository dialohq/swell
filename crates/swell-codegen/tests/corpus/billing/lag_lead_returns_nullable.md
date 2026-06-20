# lag lead returns nullable

```sql
SELECT
    i.issued_at,
    lag(i.issued_at) OVER (PARTITION BY i.workspace_id ORDER BY i.issued_at) AS prev_issued
FROM billing.invoices i
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
    i.issued_at,
    lag(i.issued_at) OVER (PARTITION BY i.workspace_id ORDER BY i.issued_at) AS prev_issued
FROM billing.invoices i`]: { params: []; row: { issued_at: BillingInvoices["issued_at"]; prev_issued: Date | null } };
  }
}
```
