# enum inside jsonb build object

```sql
SELECT jsonb_build_object('role', u.role) AS payload
FROM users u WHERE u.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT jsonb_build_object('role', u.role) AS payload
FROM users u WHERE u.id = $1`]: { params: [string | null]; row: { payload: { role: "admin" | "member" } } };
  }
}
```
