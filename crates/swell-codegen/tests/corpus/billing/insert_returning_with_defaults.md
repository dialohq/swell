# insert returning with defaults

```sql
INSERT INTO billing.users (email, password_hash)
VALUES ($1, $2)
RETURNING id, email, created_at, last_login_at
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
    [`INSERT INTO billing.users (email, password_hash)
VALUES ($1, $2)
RETURNING id, email, created_at, last_login_at`]: { params: [string, string]; row: { id: BillingUsers["id"]; email: BillingUsers["email"]; created_at: BillingUsers["created_at"]; last_login_at: BillingUsers["last_login_at"] | null } };
  }
}
```
