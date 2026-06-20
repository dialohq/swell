# update returning old and new

```sql
UPDATE billing.invoices
SET status = 'paid', paid_at = now()
WHERE id = $1 AND status = 'open'
RETURNING id, status, paid_at, amount_cents
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
    [`UPDATE billing.invoices
SET status = 'paid', paid_at = now()
WHERE id = $1 AND status = 'open'
RETURNING id, status, paid_at, amount_cents`]: { params: [string | null]; row: { id: BillingInvoices["id"]; status: BillingInvoices["status"]; paid_at: BillingInvoices["paid_at"] | null; amount_cents: BillingInvoices["amount_cents"] } };
  }
}
```
