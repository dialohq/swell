# nullif is nullable

```sql
SELECT nullif($1::text, '') AS t
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT nullif($1::text, '') AS t": { params: [string | null]; row: { t: string | null } };
  }
}
```
