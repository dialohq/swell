# json shape aggregate with group by

```sql
SELECT
    w.id,
    jsonb_agg(jsonb_build_object('member', u.email, 'role', m.role)) AS members
FROM billing.workspaces w
JOIN billing.memberships m ON m.workspace_id = w.id
JOIN billing.users u ON u.id = m.user_id
WHERE w.id = $1
GROUP BY w.id
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

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
    w.id,
    jsonb_agg(jsonb_build_object('member', u.email, 'role', m.role)) AS members
FROM billing.workspaces w
JOIN billing.memberships m ON m.workspace_id = w.id
JOIN billing.users u ON u.id = m.user_id
WHERE w.id = $1
GROUP BY w.id`]: { params: [string | null]; row: { id: BillingWorkspaces["id"]; members: { member: string; role: "owner" | "admin" | "member" | "viewer" }[] | null } };
  }
}
```
