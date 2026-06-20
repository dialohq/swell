# jsonb build object simple

```sql
SELECT jsonb_build_object(
    'id', u.id,
    'email', u.email,
    'name', u.display_name
) AS profile
FROM users u WHERE u.id = $1
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT jsonb_build_object(
    'id', u.id,
    'email', u.email,
    'name', u.display_name
) AS profile
FROM users u WHERE u.id = $1`]: { params: [string | null]; row: { profile: { id: string; email: string; name: string | null } } };
  }
}
```
