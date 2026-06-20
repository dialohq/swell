# custom composite type renders as object

```sql
SELECT home_address FROM users WHERE id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface Users {
  id: string;
  org_id: string;
  email: string;
  display_name: string | null;
  role: "admin" | "member";
  home_address: { street: unknown; city: unknown; zip: unknown } | null;
  settings: Json;
}

declare module "@dialo/swell" {
  interface Registry {
    "SELECT home_address FROM users WHERE id = $1": { params: [string | null]; row: { home_address: Users["home_address"] | null } };
  }
}
```
