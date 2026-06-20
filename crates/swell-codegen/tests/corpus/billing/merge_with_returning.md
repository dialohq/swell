# merge with returning

```sql
MERGE INTO billing.invoices i
USING (SELECT $1::uuid AS id, $2::text AS new_status) src
    ON src.id = i.id
WHEN MATCHED THEN
    UPDATE SET status = src.new_status::billing.invoice_status
WHEN NOT MATCHED THEN
    DO NOTHING
RETURNING i.id, i.status
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
    [`MERGE INTO billing.invoices i
USING (SELECT $1::uuid AS id, $2::text AS new_status) src
    ON src.id = i.id
WHEN MATCHED THEN
    UPDATE SET status = src.new_status::billing.invoice_status
WHEN NOT MATCHED THEN
    DO NOTHING
RETURNING i.id, i.status`]: { params: [string | null, string | null]; row: { id: BillingInvoices["id"]; status: BillingInvoices["status"] } };
  }
}
```
