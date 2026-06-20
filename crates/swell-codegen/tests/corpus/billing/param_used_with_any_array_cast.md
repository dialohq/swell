# param used with any array cast

```sql
SELECT id, email FROM billing.users WHERE id = ANY($1::uuid[])
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
    "SELECT id, email FROM billing.users WHERE id = ANY($1::uuid[])": { params: [string[] | null]; row: { id: BillingUsers["id"]; email: BillingUsers["email"] } };
  }
}
```
