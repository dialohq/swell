# non recursive cte

```sql
WITH active_subs AS (
    SELECT workspace_id, plan_id
    FROM billing.subscriptions
    WHERE status = 'active'
)
SELECT a.workspace_id, p.name AS plan_name
FROM active_subs a
JOIN billing.plans p ON p.id = a.plan_id
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface BillingPlans {
  id: string;
  code: string;
  name: string;
  price_cents: string;
  bill_interval: "monthly" | "yearly";
  features: Json;
  is_archived: boolean;
}
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

declare module "@dialo/swell" {
  interface Registry {
    [`WITH active_subs AS (
    SELECT workspace_id, plan_id
    FROM billing.subscriptions
    WHERE status = 'active'
)
SELECT a.workspace_id, p.name AS plan_name
FROM active_subs a
JOIN billing.plans p ON p.id = a.plan_id`]: { params: []; row: { workspace_id: BillingSubscriptions["workspace_id"]; plan_name: BillingPlans["name"] } };
  }
}
```
