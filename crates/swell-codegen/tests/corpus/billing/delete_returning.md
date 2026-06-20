# delete returning

```sql
DELETE FROM billing.audit_events
WHERE workspace_id = $1 AND created_at < now() - interval '90 days'
RETURNING id, action
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
    [`DELETE FROM billing.audit_events
WHERE workspace_id = $1 AND created_at < now() - interval '90 days'
RETURNING id, action`]: { params: [string | null]; row: { id: BillingAuditEvents["id"]; action: BillingAuditEvents["action"] } };
  }
}
```
