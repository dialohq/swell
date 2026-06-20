# insert values param to nullable column stays nullable

```sql
INSERT INTO users (id, org_id, email, role, display_name, settings)
         VALUES ($1, $2, $3, $4, $5, $6)
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
    [`INSERT INTO users (id, org_id, email, role, display_name, settings)
         VALUES ($1, $2, $3, $4, $5, $6)`]: { params: [string, string, string, "admin" | "member", string | null, Json]; row: never };
  }
}
```
