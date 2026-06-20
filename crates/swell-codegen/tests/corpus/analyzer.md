# [Setup](./analyzer.setup.sql)

# Common types

```ts
export interface Orgs {
  id: string;
  name: string;
}
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
```

# Tests

## Scalar select with param

```sql
SELECT id, email FROM users WHERE id = $1
```

```ts
$1: string | null
result: { id: Users["id"]; email: Users["email"] }
```

## Nullable base column

```sql
SELECT display_name FROM users WHERE id = $1
```

```ts
$1: string | null
result: { display_name: Users["display_name"] | null }
```

## Enum column

```sql
SELECT role FROM users WHERE id = $1
```

```ts
$1: string | null
result: { role: Users["role"] }
```

## Count aggregate is bigint

```sql
SELECT count(*) AS n FROM users
```

```ts
result: { n: string }
```

## Timestamp and uuid arrays

```sql
SELECT ARRAY[gen_random_uuid()] AS u, NOW() AS t
```

```ts
result: { u: string[] | null; t: Date | null }
```

## Count star is not nullable

```sql
SELECT count(*) AS n FROM users
```

```ts
result: { n: string }
```

## Sum is nullable

```sql
SELECT sum(1) AS s FROM users
```

```ts
result: { s: string | null }
```

## Coalesce with literal is not nullable

```sql
SELECT coalesce(display_name, 'unknown') AS label FROM users WHERE id = $1
```

```ts
$1: string | null
result: { label: string }
```

## Left join makes rhs nullable

```sql
SELECT u.email, p.body
FROM users u
LEFT JOIN posts p ON p.author_id = u.id
WHERE u.id = $1
```

```ts
$1: string | null
result: { email: Users["email"]; body: Posts["body"] | null }
```

## Inner join preserves not null

```sql
SELECT u.email, o.name
FROM users u
JOIN orgs o ON o.id = u.org_id
WHERE u.id = $1
```

```ts
$1: string | null
result: { email: Users["email"]; name: Orgs["name"] }
```

## Jsonb column is unknown until m7

```sql
SELECT settings FROM users WHERE id = $1
```

```ts
$1: string | null
result: { settings: Users["settings"] }
```

## Override force not null

```sql
SELECT coalesce(display_name, email) AS "label!" FROM users WHERE id = $1
```

```ts
$1: string | null
result: { label: string }
```

## Override force nullable

```sql
SELECT email AS "email_maybe?" FROM users WHERE id = $1
```

```ts
$1: string | null
result: { email_maybe: Users["email"] | null }
```

## Override type

```sql
SELECT settings AS "settings: UserSettings" FROM users WHERE id = $1
```

```ts
$1: string | null
result: { settings: Users["settings"] }
```

## Override type and not null

```sql
SELECT settings AS "settings!: UserSettings" FROM users WHERE id = $1
```

```ts
$1: string | null
result: { settings: Users["settings"] }
```

## Jsonb build object simple

```sql
SELECT jsonb_build_object(
    'id', u.id,
    'email', u.email,
    'name', u.display_name
) AS profile
FROM users u WHERE u.id = $1
```

```ts
$1: string | null
result: { profile: { id: string; email: string; name: string | null } }
```

## Jsonb agg with jsonb build object

```sql
SELECT o.name,
       jsonb_agg(jsonb_build_object('id', u.id, 'email', u.email)) AS members
FROM orgs o JOIN users u ON u.org_id = o.id
WHERE o.id = $1
GROUP BY o.id, o.name
```

```ts
$1: string | null
result: { name: Orgs["name"]; members: { id: string; email: string }[] | null }
```

## Json build object nested

```sql
SELECT jsonb_build_object(
    'user', jsonb_build_object('id', u.id, 'role', u.role),
    'meta', jsonb_build_object('email', u.email)
) AS payload
FROM users u WHERE u.id = $1
```

```ts
$1: string | null
result: { payload: { user: { id: string; role: "admin" | "member" }; meta: { email: string } } }
```

## To jsonb table alias enumerates columns

```sql
SELECT to_jsonb(o) AS row FROM orgs o WHERE o.id = $1
```

```ts
$1: string | null
result: { row: { id: string; name: string } }
```

## Custom domain renders as base type

```sql
SELECT email FROM users WHERE id = $1
```

```ts
$1: string | null
result: { email: Users["email"] }
```

## Custom enum renders as string union

```sql
SELECT role FROM users WHERE id = $1
```

```ts
$1: string | null
result: { role: Users["role"] }
```

## Custom composite type renders as object

```sql
SELECT home_address FROM users WHERE id = $1
```

```ts
$1: string | null
result: { home_address: Users["home_address"] | null }
```

## Jsonb build object with dynamic key

```sql
SELECT jsonb_build_object(
    u.email, u.id,
    'static_key', u.role
) AS payload
FROM users u WHERE u.id = $1
```

```ts
$1: string | null
result: { payload: Record<string, string | "admin" | "member"> }
```

## Enum inside jsonb build object

```sql
SELECT jsonb_build_object('role', u.role) AS payload
FROM users u WHERE u.id = $1
```

```ts
$1: string | null
result: { payload: { role: "admin" | "member" } }
```

## Insert values param to not null column is not nullable

```sql
INSERT INTO orgs (id, name) VALUES ($1, $2)
```

```ts
$1: string
$2: string
result: never
```

## Insert values param to nullable column stays nullable

```sql
INSERT INTO users (id, org_id, email, role, display_name, settings)
         VALUES ($1, $2, $3, $4, $5, $6)
```

```ts
$1: string
$2: string
$3: string
$4: "admin" | "member"
$5: string | null
$6: Json
result: never
```

## Update set param to not null column is not nullable

```sql
UPDATE posts SET body = $1 WHERE id = $2
```

```ts
$1: string
$2: string | null
result: never
```

## Select where param stays nullable

```sql
SELECT id FROM users WHERE id = $1
```

```ts
$1: string | null
result: { id: Users["id"] }
```

## Insert values wrapped in coalesce stays nullable

```sql
INSERT INTO users (id, org_id, email, role, settings)
         VALUES ($1, $2, $3, coalesce($4, 'member'::user_role), $5)
```

```ts
$1: string
$2: string
$3: string
$4: "admin" | "member" | null
$5: Json
result: never
```

## Select column carries table ref

```sql
SELECT id, email FROM users WHERE id = $1
```

```ts
$1: string | null
result: { id: Users["id"]; email: Users["email"] }
```

## Count star has no table ref

```sql
SELECT count(*) AS n FROM users
```

```ts
result: { n: string }
```

## Cast column has no table ref

```sql
SELECT id::text AS id_text FROM users
```

```ts
result: { id_text: string | null }
```

## Insert values param carries table ref

```sql
INSERT INTO orgs (id, name) VALUES ($1, $2)
```

```ts
$1: string
$2: string
result: never
```

## Where param has no table ref

```sql
SELECT id FROM users WHERE id = $1
```

```ts
$1: string | null
result: { id: Users["id"] }
```
