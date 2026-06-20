# json shape dynamic key collapses to record

```sql
SELECT jsonb_build_object(
    u.email, u.id,
    'role', m.role
) AS lookup
FROM billing.users u JOIN billing.memberships m ON m.user_id = u.id
WHERE u.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT jsonb_build_object(
    u.email, u.id,
    'role', m.role
) AS lookup
FROM billing.users u JOIN billing.memberships m ON m.user_id = u.id
WHERE u.id = $1`]: { params: [string | null]; row: { lookup: Record<string, string | "owner" | "admin" | "member" | "viewer"> } };
  }
}
```
