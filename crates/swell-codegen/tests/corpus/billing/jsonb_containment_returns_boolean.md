# jsonb containment returns boolean

```sql
SELECT metadata @> $1::jsonb AS has_subset
FROM billing.users WHERE id = $2
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT metadata @> $1::jsonb AS has_subset
FROM billing.users WHERE id = $2`]: { params: [Json | null, string | null]; row: { has_subset: boolean | null } };
  }
}
```
