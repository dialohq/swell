# cross join lateral unnest array

```sql
SELECT u.email, t.label
FROM billing.users u
CROSS JOIN LATERAL unnest(ARRAY['admin', 'member']::text[]) AS t(label)
WHERE u.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

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
    [`SELECT u.email, t.label
FROM billing.users u
CROSS JOIN LATERAL unnest(ARRAY['admin', 'member']::text[]) AS t(label)
WHERE u.id = $1`]: { params: [string | null]; row: { email: BillingUsers["email"]; label: string | null } };
  }
}
```
