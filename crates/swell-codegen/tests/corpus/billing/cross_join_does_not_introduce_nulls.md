# cross join does not introduce nulls

```sql
SELECT u.email, p.code
FROM billing.users u CROSS JOIN billing.plans p
WHERE u.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface BillingPlans {
  id: string;
  code: string;
  name: string;
  price_cents: string;
  bill_interval: "monthly" | "yearly";
  features: Json;
  is_archived: boolean;
}
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
    [`SELECT u.email, p.code
FROM billing.users u CROSS JOIN billing.plans p
WHERE u.id = $1`]: { params: [string | null]; row: { email: BillingUsers["email"]; code: BillingPlans["code"] } };
  }
}
```
