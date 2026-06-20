# timestamp and uuid arrays

```sql
SELECT ARRAY[gen_random_uuid()] AS u, NOW() AS t
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    "SELECT ARRAY[gen_random_uuid()] AS u, NOW() AS t": { params: []; row: { u: string[] | null; t: Date | null } };
  }
}
```
