# force not null on join with filter

```sql
SELECT w.id, s.status AS "status!"
FROM billing.workspaces w
LEFT JOIN billing.subscriptions s ON s.workspace_id = w.id
WHERE s.id IS NOT NULL AND w.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface BillingSubscriptions {
  id: string;
  workspace_id: string;
  plan_id: string;
  status: "trialing" | "active" | "past_due" | "canceled" | "incomplete";
  trial_ends_at: Date | null;
  current_period_start: Date;
  current_period_end: Date;
  canceled_at: Date | null;
  created_at: Date;
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
    [`SELECT w.id, s.status AS "status!"
FROM billing.workspaces w
LEFT JOIN billing.subscriptions s ON s.workspace_id = w.id
WHERE s.id IS NOT NULL AND w.id = $1`]: { params: [string | null]; row: { id: BillingWorkspaces["id"]; status: BillingSubscriptions["status"] } };
  }
}
```
