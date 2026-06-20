# set returning function in from

```sql
SELECT * FROM billing.upcoming_invoices(30)
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT * FROM billing.upcoming_invoices(30)": { params: []; row: { invoice_id: string | null; workspace: string | null; due_at: Date | null; amount: string | null } };
  }
}
```
