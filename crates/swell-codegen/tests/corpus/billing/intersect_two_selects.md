# intersect two selects

```sql
SELECT id FROM billing.users
INTERSECT
SELECT user_id FROM billing.memberships WHERE workspace_id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT id FROM billing.users
INTERSECT
SELECT user_id FROM billing.memberships WHERE workspace_id = $1`]: { params: [string | null]; row: { id: string | null } };
  }
}
```
