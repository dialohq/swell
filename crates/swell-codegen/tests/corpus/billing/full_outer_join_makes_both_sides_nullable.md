# full outer join makes both sides nullable

```sql
SELECT a.email AS left_email, b.email AS right_email
FROM billing.users a
FULL OUTER JOIN billing.users b ON a.id = b.id
WHERE a.id = $1 OR b.id = $2
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
    [`SELECT a.email AS left_email, b.email AS right_email
FROM billing.users a
FULL OUTER JOIN billing.users b ON a.id = b.id
WHERE a.id = $1 OR b.id = $2`]: { params: [string | null, string | null]; row: { left_email: BillingUsers["email"] | null; right_email: BillingUsers["email"] | null } };
  }
}
```
