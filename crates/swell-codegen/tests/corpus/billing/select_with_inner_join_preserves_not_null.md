# select with inner join preserves not null

```sql
SELECT u.email, u.display_name, m.role, m.joined_at
FROM billing.users u
JOIN billing.memberships m ON m.user_id = u.id
WHERE m.workspace_id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface BillingMemberships {
  workspace_id: string;
  user_id: string;
  role: "owner" | "admin" | "member" | "viewer";
  joined_at: Date;
  invited_by: string | null;
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
    [`SELECT u.email, u.display_name, m.role, m.joined_at
FROM billing.users u
JOIN billing.memberships m ON m.user_id = u.id
WHERE m.workspace_id = $1`]: { params: [string | null]; row: { email: BillingUsers["email"]; display_name: BillingUsers["display_name"] | null; role: BillingMemberships["role"]; joined_at: BillingMemberships["joined_at"] } };
  }
}
```
