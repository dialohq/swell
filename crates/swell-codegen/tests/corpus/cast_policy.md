# Setup

```sql
-- This suite exists to exercise swell's per-cast policy: only the
-- specific `(source_typoid, target_typoid)` pair that has a
-- user-defined `castmethod='f'` in `pg_cast` should widen — an
-- unrelated unsafe cast (`mytype::text`) must NOT taint `id::text` in
-- the same query.
--
-- The setup installs:
--   * `swell_cast_test_type`         — a custom composite.
--   * `swell_cast_test_to_text(…)`   — a user-defined cast function.
--   * `CAST (swell_cast_test_type AS text) WITH FUNCTION`
--                                     — the only unsafe cast in the DB.
--   * `cast_users(id uuid, name text NOT NULL, tag swell_cast_test_type NOT NULL)`
--
-- Idempotent: reruns drop everything first.

DROP TABLE IF EXISTS cast_users CASCADE;
DROP TYPE  IF EXISTS swell_cast_test_type CASCADE;

CREATE TYPE swell_cast_test_type AS (n int);

CREATE FUNCTION swell_cast_test_to_text(swell_cast_test_type)
    RETURNS text
    LANGUAGE sql IMMUTABLE
    AS $$ SELECT $1.n::text $$;

CREATE CAST (swell_cast_test_type AS text)
    WITH FUNCTION swell_cast_test_to_text;

CREATE TABLE cast_users (
    id    uuid PRIMARY KEY,
    name  text NOT NULL,
    tag   swell_cast_test_type NOT NULL
);
```

# Common types

```ts
export interface CastUsers {
  id: string;
  name: string;
  tag: { n: number | null };
}
```

# Tests

## Cast through built-in I/O stays non-null

```sql
SELECT id::text AS id_text FROM cast_users
```

```ts
result: { id_text: string }
```

## Cast through user-defined castfunc widens

```sql
SELECT tag::text AS tag_text FROM cast_users
```

```ts
result: { tag_text: string | null }
```

## Cast of literal does not need the unsafe cast lookup

```sql
SELECT 'hello'::text AS greeting
```

```ts
result: { greeting: string }
```

## Unrelated cast in the same query is not tainted by the unsafe one

```sql
SELECT id::text AS id_text, tag::text AS tag_text FROM cast_users
```

```ts
result: { id_text: string; tag_text: string | null }
```

## Bare column ref is unaffected

```sql
SELECT name FROM cast_users
```

```ts
result: { name: CastUsers["name"] }
```
