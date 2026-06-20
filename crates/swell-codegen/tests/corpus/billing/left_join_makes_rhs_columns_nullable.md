# left join makes rhs columns nullable

```sql
SELECT w.id, w.name, s.status, s.current_period_end
FROM billing.workspaces w
LEFT JOIN billing.subscriptions s ON s.workspace_id = w.id
WHERE w.deleted_at IS NULL
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
    [`SELECT w.id, w.name, s.status, s.current_period_end
FROM billing.workspaces w
LEFT JOIN billing.subscriptions s ON s.workspace_id = w.id
WHERE w.deleted_at IS NULL`]: { params: []; row: { id: BillingWorkspaces["id"]; name: BillingWorkspaces["name"]; status: BillingSubscriptions["status"] | null; current_period_end: BillingSubscriptions["current_period_end"] | null } };
  }
}
```
