# update returning with override corrects nullability

```sql
UPDATE billing.invoices
SET paid_at = now()
WHERE id = $1
RETURNING paid_at AS "paid_at!"
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
SET paid_at = now()
WHERE id = $1
RETURNING paid_at AS "paid_at!"`]: { params: [string | null]; row: { paid_at: BillingInvoices["paid_at"] } };
  }
}
```
