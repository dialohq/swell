# inner join preserves not null

```sql
SELECT u.email, o.name
FROM users u
JOIN orgs o ON o.id = u.org_id
WHERE u.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface Orgs {
  id: string;
  name: string;
}
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
    [`SELECT u.email, o.name
FROM users u
JOIN orgs o ON o.id = u.org_id
WHERE u.id = $1`]: { params: [string | null]; row: { email: Users["email"]; name: Orgs["name"] } };
  }
}
```
