# unnest with ordinality

```sql
SELECT t.label, t.idx
FROM unnest(ARRAY['a', 'b', 'c']::text[]) WITH ORDINALITY AS t(label, idx)
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT t.label, t.idx
FROM unnest(ARRAY['a', 'b', 'c']::text[]) WITH ORDINALITY AS t(label, idx)`]: { params: []; row: { label: string | null; idx: string | null } };
  }
}
```
