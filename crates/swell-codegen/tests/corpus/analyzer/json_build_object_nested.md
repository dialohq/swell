# json build object nested

```sql
SELECT jsonb_build_object(
    'user', jsonb_build_object('id', u.id, 'role', u.role),
    'meta', jsonb_build_object('email', u.email)
) AS payload
FROM users u WHERE u.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT jsonb_build_object(
    'user', jsonb_build_object('id', u.id, 'role', u.role),
    'meta', jsonb_build_object('email', u.email)
) AS payload
FROM users u WHERE u.id = $1`]: { params: [string | null]; row: { payload: { user: { id: string; role: "admin" | "member" }; meta: { email: string } } } };
  }
}
```
