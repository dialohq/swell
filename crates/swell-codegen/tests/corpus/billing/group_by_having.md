# group by having

```sql
SELECT workspace_id, count(*) AS member_count
FROM billing.memberships
GROUP BY workspace_id
HAVING count(*) > 1
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

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT workspace_id, count(*) AS member_count
FROM billing.memberships
GROUP BY workspace_id
HAVING count(*) > 1`]: { params: []; row: { workspace_id: BillingMemberships["workspace_id"]; member_count: string } };
  }
}
```
