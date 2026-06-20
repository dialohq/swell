# range column renders as lower upper

```sql
SELECT valid_during FROM billing.promotions WHERE id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface BillingPromotions {
  id: string;
  workspace_id: string;
  code: string;
  valid_during: { lower: Date | null; upper: Date | null };
  blackout_periods: { lower: Date | null; upper: Date | null } | null;
  eligible_roles: ("owner" | "admin" | "member" | "viewer")[];
  discount_pct: string;
  code_lower: string | null;
}

declare module "@dialo/swell" {
  interface Registry {
    "SELECT valid_during FROM billing.promotions WHERE id = $1": { params: [string | null]; row: { valid_during: BillingPromotions["valid_during"] } };
  }
}
```
