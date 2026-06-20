# param repeated use is one entry

```sql
SELECT id FROM billing.users WHERE id = $1 OR email = $1::text
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
    "SELECT id FROM billing.users WHERE id = $1 OR email = $1::text": { params: [string | null]; row: { id: BillingUsers["id"] } };
  }
}
```
