# coalesce with literal is not nullable

```sql
SELECT coalesce(display_name, 'unknown') AS label FROM users WHERE id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT coalesce(display_name, 'unknown') AS label FROM users WHERE id = $1": { params: [string | null]; row: { label: string } };
  }
}
```
