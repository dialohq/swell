# override typed jsonb

```sql
SELECT settings AS "settings: WorkspaceSettings"
FROM billing.workspaces WHERE id = $1
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
    [`SELECT settings AS "settings: WorkspaceSettings"
FROM billing.workspaces WHERE id = $1`]: { params: [string | null]; row: { settings: BillingWorkspaces["settings"] } };
  }
}
```
