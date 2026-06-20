# distinct on keeps attnotnull

```sql
SELECT DISTINCT ON (workspace_id)
    workspace_id, status, issued_at
FROM billing.invoices
ORDER BY workspace_id, issued_at DESC
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
    [`SELECT DISTINCT ON (workspace_id)
    workspace_id, status, issued_at
FROM billing.invoices
ORDER BY workspace_id, issued_at DESC`]: { params: []; row: { workspace_id: BillingInvoices["workspace_id"]; status: BillingInvoices["status"]; issued_at: BillingInvoices["issued_at"] } };
  }
}
```
