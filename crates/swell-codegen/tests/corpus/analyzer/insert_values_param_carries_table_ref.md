# insert values param carries table ref

```sql
INSERT INTO orgs (id, name) VALUES ($1, $2)
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface Orgs {
  id: string;
  name: string;
}

declare module "@dialo/swell" {
  interface Registry {
    "INSERT INTO orgs (id, name) VALUES ($1, $2)": { params: [string, string]; row: never };
  }
}
```
