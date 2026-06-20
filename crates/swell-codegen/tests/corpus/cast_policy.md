# Setup

```sql
-- This suite exists to exercise the conservative branch of
-- swell's pg_cast policy probe. The setup installs a
-- user-defined `castmethod = 'f'` cast — the only cast shape that
-- can legitimately return NULL on non-NULL input — and the tests
-- below verify that the analyzer correctly *stops* propagating
-- non-null through `<expr>::T` once such a cast exists.
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
    name  text NOT NULL
);
```

# Common types

```ts
export interface CastUsers {
  id: string;
  name: string;
}
```

# Tests

## Cast of NOT NULL column is widened under conservative policy

```sql
SELECT id::text AS id_text FROM cast_users
```

```ts
result: { id_text: string | null }
```

## Cast of literal is non-null even under conservative policy

```sql
SELECT 'hello'::text AS greeting
```

```ts
result: { greeting: string }
```

## Cast of NOT NULL column to itself is still widened

```sql
SELECT name::text AS name_copy FROM cast_users
```

```ts
result: { name_copy: CastUsers["name"] | null }
```

## Bare column ref without cast is not widened

```sql
SELECT name FROM cast_users
```

```ts
result: { name: CastUsers["name"] }
```
