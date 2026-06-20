# update set param to not null column is not nullable

```sql
UPDATE posts SET body = $1 WHERE id = $2
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface Posts {
  id: string;
  author_id: string;
  body: string;
  published_at: Date | null;
}

declare module "@dialo/swell" {
  interface Registry {
    "UPDATE posts SET body = $1 WHERE id = $2": { params: [string, string | null]; row: never };
  }
}
```
