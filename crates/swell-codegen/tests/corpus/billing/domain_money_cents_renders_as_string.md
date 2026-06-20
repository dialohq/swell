# domain money cents renders as string

```sql
SELECT price_cents FROM billing.plans WHERE code = $1
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

declare module "@dialo/swell" {
  interface Registry {
    "SELECT price_cents FROM billing.plans WHERE code = $1": { params: [string | null]; row: { price_cents: BillingPlans["price_cents"] } };
  }
}
```
