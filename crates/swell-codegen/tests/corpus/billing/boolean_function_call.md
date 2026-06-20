# boolean function call

```sql
SELECT billing.is_member($1, $2, 'admin') AS allowed
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT billing.is_member($1, $2, 'admin') AS allowed": { params: [string | null, string | null]; row: { allowed: boolean | null } };
  }
}
```
