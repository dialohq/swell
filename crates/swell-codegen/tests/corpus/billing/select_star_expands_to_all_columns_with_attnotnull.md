# select star expands to all columns with attnotnull

```sql
SELECT * FROM billing.users WHERE id = $1
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
    "SELECT * FROM billing.users WHERE id = $1": { params: [string | null]; row: { id: BillingUsers["id"]; email: BillingUsers["email"]; display_name: BillingUsers["display_name"] | null; password_hash: BillingUsers["password_hash"]; avatar_url: BillingUsers["avatar_url"] | null; created_at: BillingUsers["created_at"]; last_login_at: BillingUsers["last_login_at"] | null; metadata: BillingUsers["metadata"] } };
  }
}
```
