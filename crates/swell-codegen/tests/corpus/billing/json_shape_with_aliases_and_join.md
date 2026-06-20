# json shape with aliases and join

```sql
SELECT jsonb_build_object(
    'workspace_id', w.id,
    'workspace_name', w.name,
    'plan', p.code,
    'status', s.status,
    'mrr_cents', billing.workspace_revenue_cents(w.id)
) AS summary
FROM billing.workspaces w
JOIN billing.subscriptions s ON s.workspace_id = w.id
JOIN billing.plans p ON p.id = s.plan_id
WHERE w.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT jsonb_build_object(
    'workspace_id', w.id,
    'workspace_name', w.name,
    'plan', p.code,
    'status', s.status,
    'mrr_cents', billing.workspace_revenue_cents(w.id)
) AS summary
FROM billing.workspaces w
JOIN billing.subscriptions s ON s.workspace_id = w.id
JOIN billing.plans p ON p.id = s.plan_id
WHERE w.id = $1`]: { params: [string | null]; row: { summary: { workspace_id: string; workspace_name: string; plan: string; status: "trialing" | "active" | "past_due" | "canceled" | "incomplete"; mrr_cents: unknown } } };
  }
}
```
