# cast column has no table ref

```sql
SELECT id::text AS id_text FROM users
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT id::text AS id_text FROM users": { params: []; row: { id_text: string | null } };
  }
}
```
