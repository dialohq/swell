# calling custom scalar function

```sql
SELECT billing.workspace_revenue_cents(w.id) AS revenue
FROM billing.workspaces w WHERE w.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT billing.workspace_revenue_cents(w.id) AS revenue
FROM billing.workspaces w WHERE w.id = $1`]: { params: [string | null]; row: { revenue: string | null } };
  }
}
```
