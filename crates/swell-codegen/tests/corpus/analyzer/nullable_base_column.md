# nullable base column

```sql
SELECT display_name FROM users WHERE id = $1
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
    "SELECT display_name FROM users WHERE id = $1": { params: [string | null]; row: { display_name: Users["display_name"] | null } };
  }
}
```
