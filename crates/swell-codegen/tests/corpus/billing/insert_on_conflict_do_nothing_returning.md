# insert on conflict do nothing returning

```sql
INSERT INTO billing.users (email, password_hash) VALUES ($1, $2)
ON CONFLICT (email) DO NOTHING
RETURNING id, email
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
    [`INSERT INTO billing.users (email, password_hash) VALUES ($1, $2)
ON CONFLICT (email) DO NOTHING
RETURNING id, email`]: { params: [string, string]; row: { id: BillingUsers["id"]; email: BillingUsers["email"] } };
  }
}
```
