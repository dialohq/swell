# exists subquery in where doesnt change select types

```sql
SELECT id, name FROM billing.workspaces w
WHERE EXISTS (
    SELECT 1 FROM billing.memberships m
    WHERE m.workspace_id = w.id AND m.user_id = $1
)
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
    [`SELECT id, name FROM billing.workspaces w
WHERE EXISTS (
    SELECT 1 FROM billing.memberships m
    WHERE m.workspace_id = w.id AND m.user_id = $1
)`]: { params: [string | null]; row: { id: BillingWorkspaces["id"]; name: BillingWorkspaces["name"] } };
  }
}
```
