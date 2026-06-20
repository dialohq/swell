# enum column rendered as union

```sql
SELECT status FROM billing.subscriptions WHERE workspace_id = $1
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

declare module "@dialo/swell" {
  interface Registry {
    "SELECT status FROM billing.subscriptions WHERE workspace_id = $1": { params: [string | null]; row: { status: BillingSubscriptions["status"] } };
  }
}
```
