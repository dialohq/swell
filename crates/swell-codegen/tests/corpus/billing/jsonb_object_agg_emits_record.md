# jsonb object agg emits record

```sql
SELECT jsonb_object_agg(role::text, c::text) AS by_role
FROM (
    SELECT role, count(*) AS c FROM billing.memberships
    WHERE workspace_id = $1 GROUP BY role
) t
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT jsonb_object_agg(role::text, c::text) AS by_role
FROM (
    SELECT role, count(*) AS c FROM billing.memberships
    WHERE workspace_id = $1 GROUP BY role
) t`]: { params: [string | null]; row: { by_role: Record<string, unknown> | null } };
  }
}
```
