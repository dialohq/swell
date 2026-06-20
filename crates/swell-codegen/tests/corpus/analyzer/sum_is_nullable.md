# sum is nullable

```sql
SELECT sum(1) AS s FROM users
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT sum(1) AS s FROM users": { params: []; row: { s: string | null } };
  }
}
```
