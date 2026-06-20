# select star through left join

```sql
SELECT u.*, m.role
FROM billing.users u
LEFT JOIN billing.memberships m ON m.user_id = u.id AND m.workspace_id = $1
WHERE u.id = $2
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
    [`SELECT u.*, m.role
FROM billing.users u
LEFT JOIN billing.memberships m ON m.user_id = u.id AND m.workspace_id = $1
WHERE u.id = $2`]: { params: [string | null, string | null]; row: { id: BillingUsers["id"]; email: BillingUsers["email"]; display_name: BillingUsers["display_name"] | null; password_hash: BillingUsers["password_hash"]; avatar_url: BillingUsers["avatar_url"] | null; created_at: BillingUsers["created_at"]; last_login_at: BillingUsers["last_login_at"] | null; metadata: BillingUsers["metadata"]; role: BillingMemberships["role"] | null } };
  }
}
```
