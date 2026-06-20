# jsonb arrow operators

```sql
SELECT
    metadata->'theme' AS theme_jsonb,
    metadata->>'theme' AS theme_text,
    (metadata->>'count')::int AS theme_count
FROM billing.users WHERE id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT
    metadata->'theme' AS theme_jsonb,
    metadata->>'theme' AS theme_text,
    (metadata->>'count')::int AS theme_count
FROM billing.users WHERE id = $1`]: { params: [string | null]; row: { theme_jsonb: Json | null; theme_text: string | null; theme_count: number | null } };
  }
}
```
