# insert into table with identity does not require id param

```sql
INSERT INTO billing.promotions (workspace_id, code, valid_during, discount_pct)
VALUES ($1, $2, $3, $4)
RETURNING id, code, code_lower
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
    [`INSERT INTO billing.promotions (workspace_id, code, valid_during, discount_pct)
VALUES ($1, $2, $3, $4)
RETURNING id, code, code_lower`]: { params: [string, string, { lower: Date | null; upper: Date | null }, string]; row: { id: BillingPromotions["id"]; code: BillingPromotions["code"]; code_lower: BillingPromotions["code_lower"] | null } };
  }
}
```
