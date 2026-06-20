# insert select returning

```sql
INSERT INTO billing.audit_events (workspace_id, action, target_type, target_id, payload)
SELECT s.workspace_id, 'subscription.renewed', 'subscription', s.id::text, '{}'::jsonb
FROM billing.subscriptions s
WHERE s.current_period_end < now() + interval '1 day'
RETURNING id, action, created_at
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface BillingAuditEvents {
  id: string;
  workspace_id: string | null;
  actor_id: string | null;
  action: string;
  target_type: string;
  target_id: string | null;
  payload: Json;
  created_at: Date;
}

declare module "@dialo/swell" {
  interface Registry {
    [`INSERT INTO billing.audit_events (workspace_id, action, target_type, target_id, payload)
SELECT s.workspace_id, 'subscription.renewed', 'subscription', s.id::text, '{}'::jsonb
FROM billing.subscriptions s
WHERE s.current_period_end < now() + interval '1 day'
RETURNING id, action, created_at`]: { params: []; row: { id: BillingAuditEvents["id"]; action: BillingAuditEvents["action"]; created_at: BillingAuditEvents["created_at"] } };
  }
}
```
