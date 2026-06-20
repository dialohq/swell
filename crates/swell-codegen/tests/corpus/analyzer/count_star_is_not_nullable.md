# count star is not nullable

```sql
SELECT count(*) AS n FROM users
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT count(*) AS n FROM users": { params: []; row: { n: string } };
  }
}
```
