# jsonb build object with dynamic key

```sql
SELECT jsonb_build_object(
    u.email, u.id,
    'static_key', u.role
) AS payload
FROM users u WHERE u.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT jsonb_build_object(
    u.email, u.id,
    'static_key', u.role
) AS payload
FROM users u WHERE u.id = $1`]: { params: [string | null]; row: { payload: Record<string, string | "admin" | "member"> } };
  }
}
```
