# left join makes rhs nullable

```sql
SELECT u.email, p.body
FROM users u
LEFT JOIN posts p ON p.author_id = u.id
WHERE u.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface Posts {
  id: string;
  author_id: string;
  body: string;
  published_at: Date | null;
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
    [`SELECT u.email, p.body
FROM users u
LEFT JOIN posts p ON p.author_id = u.id
WHERE u.id = $1`]: { params: [string | null]; row: { email: Users["email"]; body: Posts["body"] | null } };
  }
}
```
