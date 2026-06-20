# row number over partition

```sql
SELECT
    w.name,
    row_number() OVER (PARTITION BY w.id ORDER BY i.issued_at DESC) AS rn,
    i.amount_cents
FROM billing.workspaces w
JOIN billing.invoices i ON i.workspace_id = w.id
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
export interface BillingWorkspaces {
  id: string;
  slug: string;
  name: string;
  billing_email: string;
  billing_address: { line1: unknown; line2: unknown; city: unknown; region: unknown; country: unknown; postal: unknown } | null;
  created_at: Date;
  deleted_at: Date | null;
  settings: Json;
}

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT
    w.name,
    row_number() OVER (PARTITION BY w.id ORDER BY i.issued_at DESC) AS rn,
    i.amount_cents
FROM billing.workspaces w
JOIN billing.invoices i ON i.workspace_id = w.id`]: { params: []; row: { name: BillingWorkspaces["name"]; rn: string | null; amount_cents: BillingInvoices["amount_cents"] } };
  }
}
```
