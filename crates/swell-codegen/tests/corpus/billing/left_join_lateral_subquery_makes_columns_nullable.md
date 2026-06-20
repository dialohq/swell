# left join lateral subquery makes columns nullable

```sql
SELECT u.email, latest.id AS latest_invoice_id, latest.amount_cents
FROM billing.users u
LEFT JOIN LATERAL (
    SELECT i.id, i.amount_cents
    FROM billing.invoices i
    JOIN billing.workspaces w ON w.id = i.workspace_id
    JOIN billing.memberships m ON m.workspace_id = w.id AND m.user_id = u.id
    ORDER BY i.issued_at DESC
    LIMIT 1
) latest ON TRUE
WHERE u.id = $1
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
export interface BillingUsers {
  id: string;
  email: string;
  display_name: string | null;
  password_hash: string;
  avatar_url: string | null;
  created_at: Date;
  last_login_at: Date | null;
  metadata: Json;
}

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT u.email, latest.id AS latest_invoice_id, latest.amount_cents
FROM billing.users u
LEFT JOIN LATERAL (
    SELECT i.id, i.amount_cents
    FROM billing.invoices i
    JOIN billing.workspaces w ON w.id = i.workspace_id
    JOIN billing.memberships m ON m.workspace_id = w.id AND m.user_id = u.id
    ORDER BY i.issued_at DESC
    LIMIT 1
) latest ON TRUE
WHERE u.id = $1`]: { params: [string | null]; row: { email: BillingUsers["email"]; latest_invoice_id: BillingInvoices["id"] | null; amount_cents: BillingInvoices["amount_cents"] | null } };
  }
}
```
