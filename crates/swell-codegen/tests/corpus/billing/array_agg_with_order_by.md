# array agg with order by

```sql
SELECT array_agg(u.email ORDER BY u.email) AS emails
FROM billing.users u
JOIN billing.memberships m ON m.user_id = u.id
WHERE m.workspace_id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT array_agg(u.email ORDER BY u.email) AS emails
FROM billing.users u
JOIN billing.memberships m ON m.user_id = u.id
WHERE m.workspace_id = $1`]: { params: [string | null]; row: { emails: string[] | null } };
  }
}
```
