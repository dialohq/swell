# insert values wrapped in coalesce stays nullable

```sql
INSERT INTO users (id, org_id, email, role, settings)
         VALUES ($1, $2, $3, coalesce($4, 'member'::user_role), $5)
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
    [`INSERT INTO users (id, org_id, email, role, settings)
         VALUES ($1, $2, $3, coalesce($4, 'member'::user_role), $5)`]: { params: [string, string, string, "admin" | "member" | null, Json]; row: never };
  }
}
```
