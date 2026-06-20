# self join with aliases

```sql
SELECT u.email AS member_email, inv.email AS invited_by_email
FROM billing.memberships m
JOIN billing.users u ON u.id = m.user_id
LEFT JOIN billing.users inv ON inv.id = m.invited_by
WHERE m.workspace_id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

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
    [`SELECT u.email AS member_email, inv.email AS invited_by_email
FROM billing.memberships m
JOIN billing.users u ON u.id = m.user_id
LEFT JOIN billing.users inv ON inv.id = m.invited_by
WHERE m.workspace_id = $1`]: { params: [string | null]; row: { member_email: BillingUsers["email"]; invited_by_email: BillingUsers["email"] | null } };
  }
}
```
