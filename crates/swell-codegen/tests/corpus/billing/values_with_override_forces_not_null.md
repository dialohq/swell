# values with override forces not null

```sql
SELECT t.id AS "id!", t.label
FROM (VALUES ('a'::text, 'first'), ('b', 'second')) AS t(id, label)
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT t.id AS "id!", t.label
FROM (VALUES ('a'::text, 'first'), ('b', 'second')) AS t(id, label)`]: { params: []; row: { id: string; label: string | null } };
  }
}
```
