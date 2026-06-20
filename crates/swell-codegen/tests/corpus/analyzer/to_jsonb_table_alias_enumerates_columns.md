# to jsonb table alias enumerates columns

```sql
SELECT to_jsonb(o) AS row FROM orgs o WHERE o.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT to_jsonb(o) AS row FROM orgs o WHERE o.id = $1": { params: [string | null]; row: { row: { id: string; name: string } } };
  }
}
```
