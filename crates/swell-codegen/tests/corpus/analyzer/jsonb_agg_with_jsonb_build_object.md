# jsonb agg with jsonb build object

```sql
SELECT o.name,
       jsonb_agg(jsonb_build_object('id', u.id, 'email', u.email)) AS members
FROM orgs o JOIN users u ON u.org_id = o.id
WHERE o.id = $1
GROUP BY o.id, o.name
```

```ts
import { type Json, type SqlText } from "@dialo/swell";

export interface Orgs {
  id: string;
  name: string;
}

declare module "@dialo/swell" {
  interface Registry {
    [`SELECT o.name,
       jsonb_agg(jsonb_build_object('id', u.id, 'email', u.email)) AS members
FROM orgs o JOIN users u ON u.org_id = o.id
WHERE o.id = $1
GROUP BY o.id, o.name`]: { params: [string | null]; row: { name: Orgs["name"]; members: { id: string; email: string }[] | null } };
  }
}
```
