# Setup

```sql
-- A realistic SaaS billing schema. Loaded fresh for the corpus tests.
-- Drops everything first so the tests can re-run without manual cleanup.

DROP SCHEMA IF EXISTS billing CASCADE;
CREATE SCHEMA billing;
SET search_path = billing, public;

-- Defensive: clean up any leftover from `cast_policy.md`. Its setup
-- creates a user-defined function-based cast which would otherwise
-- persist into this suite's analyzer connect and flip the `pg_cast`
-- probe to Conservative.
DROP TYPE IF EXISTS public.swell_cast_test_type CASCADE;

-- ---------- Custom types ----------

CREATE TYPE role AS ENUM ('owner', 'admin', 'member', 'viewer');

CREATE TYPE subscription_status AS ENUM (
    'trialing', 'active', 'past_due', 'canceled', 'incomplete'
);

CREATE TYPE interval_kind AS ENUM ('monthly', 'yearly');

CREATE TYPE invoice_status AS ENUM ('draft', 'open', 'paid', 'void', 'uncollectible');

-- Domain over bigint with a non-negative invariant.
CREATE DOMAIN money_cents AS bigint
    CHECK (VALUE >= 0);

-- Composite type for postal addresses.
CREATE TYPE postal_address AS (
    line1   text,
    line2   text,
    city    text,
    region  text,
    country text,
    postal  text
);

-- ---------- Tables ----------

CREATE TABLE users (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email           text NOT NULL UNIQUE,
    display_name    text,
    password_hash   text NOT NULL,
    avatar_url      text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_login_at   timestamptz,
    metadata        jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE workspaces (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug            text NOT NULL UNIQUE,
    name            text NOT NULL,
    billing_email   text NOT NULL,
    billing_address postal_address,
    created_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz,
    settings        jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE memberships (
    workspace_id  uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role          role NOT NULL,
    joined_at     timestamptz NOT NULL DEFAULT now(),
    invited_by    uuid REFERENCES users(id),
    PRIMARY KEY (workspace_id, user_id)
);

CREATE TABLE plans (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code            text NOT NULL UNIQUE,
    name            text NOT NULL,
    price_cents     money_cents NOT NULL,
    bill_interval   interval_kind NOT NULL,
    features        jsonb NOT NULL DEFAULT '[]'::jsonb,
    is_archived     boolean NOT NULL DEFAULT false
);

CREATE TABLE subscriptions (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id          uuid NOT NULL UNIQUE REFERENCES workspaces(id),
    plan_id               uuid NOT NULL REFERENCES plans(id),
    status                subscription_status NOT NULL,
    trial_ends_at         timestamptz,
    current_period_start  timestamptz NOT NULL,
    current_period_end    timestamptz NOT NULL,
    canceled_at           timestamptz,
    created_at            timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE invoices (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    subscription_id uuid NOT NULL REFERENCES subscriptions(id),
    workspace_id    uuid NOT NULL REFERENCES workspaces(id),
    number          text NOT NULL UNIQUE,
    status          invoice_status NOT NULL DEFAULT 'draft',
    amount_cents    money_cents NOT NULL,
    issued_at       timestamptz NOT NULL DEFAULT now(),
    paid_at         timestamptz,
    due_at          timestamptz NOT NULL,
    notes           text
);

CREATE TABLE invoice_lines (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    invoice_id   uuid NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    description  text NOT NULL,
    amount_cents money_cents NOT NULL,
    quantity     int NOT NULL DEFAULT 1,
    metadata     jsonb NOT NULL DEFAULT '{}'::jsonb
);

-- Promotion windows — exercise range / multirange columns and array-of-enum.
CREATE TABLE promotions (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id     uuid NOT NULL REFERENCES workspaces(id),
    code             text NOT NULL,
    valid_during     tstzrange NOT NULL,
    blackout_periods tstzmultirange,
    eligible_roles   role[] NOT NULL DEFAULT '{}',
    discount_pct     numeric(5, 2) NOT NULL,
    -- A stored generated column whose expression is nullable.
    code_lower       text GENERATED ALWAYS AS (lower(code)) STORED
);

CREATE TABLE audit_events (
    id           bigserial PRIMARY KEY,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,
    actor_id     uuid REFERENCES users(id),
    action       text NOT NULL,
    target_type  text NOT NULL,
    target_id    text,
    payload      jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- Feature flags exercise CHECK-based literal narrowing on text columns:
-- swell reduces a column-bound CHECK predicate (equality / IN / ANY /
-- IS NULL OR <pred>) into a TS literal union and applies it to the
-- column's `text` rendering.
CREATE TABLE feature_flags (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    scope         text NOT NULL CHECK (scope IN ('global', 'workspace', 'user')),
    tier          text NOT NULL CHECK (tier = ANY (ARRAY['free', 'pro', 'enterprise'])),
    pinned_to     text CHECK (pinned_to IS NULL OR pinned_to = 'beta'),
    locked_value  text NOT NULL CHECK (locked_value = 'on')
);

-- Tier 2: jsonb object shapes. AND-chain of jsonb_typeof / ?& / ->-typed
-- predicates reduces to a TS object type.
CREATE TABLE widgets (
    id    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    meta  jsonb NOT NULL CHECK (
        jsonb_typeof(meta) = 'object'
        AND meta ?& ARRAY['width', 'height']
        AND jsonb_typeof(meta -> 'width') = 'number'
        AND jsonb_typeof(meta -> 'height') = 'number'
    )
);

-- Tier 3 col-level: OR over AND-chains reduces to a TS union.
CREATE TABLE payloads (
    id       uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    payload  jsonb NOT NULL CHECK (
        (payload ->> 'kind' = 'text'
         AND jsonb_typeof(payload -> 'body') = 'string')
        OR (payload ->> 'kind' = 'image'
            AND jsonb_typeof(payload -> 'url') = 'string')
    )
);

-- Tier 3 row-level (num_nonnulls): exactly one of `email` / `phone`
-- non-null per row → TS row variants.
CREATE TABLE contacts (
    id     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email  text,
    phone  text,
    CHECK (num_nonnulls(email, phone) = 1)
);

-- Tier 3 row-level (CASE): discriminant column pins per-branch
-- refinement on the JSON config; ELSE false makes it exhaustive.
CREATE TABLE field_configs (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    field_type  text NOT NULL,
    config      jsonb NOT NULL,
    CHECK (CASE
        WHEN field_type = 'text'   THEN jsonb_typeof(config -> 'maxLength') = 'number'
        WHEN field_type = 'select' THEN jsonb_typeof(config -> 'options')   = 'array'
        ELSE false
    END)
);

-- Multi-column OR-of-AND row narrowing: each branch pins a different
-- combination of `kind` (literal) and which of `url` / `body` is
-- non-null. The row reducer recognises arbitrary AND-chains of
-- atomic predicates (`= lit`, `IS NOT NULL`, `IS NULL`).
CREATE TABLE blocks (
    id    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind  text NOT NULL,
    url   text,
    body  text,
    CHECK (
        (kind = 'image' AND url  IS NOT NULL AND body IS NULL)
        OR (kind = 'text' AND body IS NOT NULL AND url  IS NULL)
    )
);

-- CASE THEN with a NullTest narrows a non-discriminant column per
-- variant; ELSE true widens with the discriminant catch-all.
CREATE TABLE shipments (
    id        uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    status    text NOT NULL,
    paid_at   timestamptz,
    CHECK (CASE
        WHEN status = 'paid'  THEN paid_at IS NOT NULL
        WHEN status = 'draft' THEN paid_at IS NULL
        ELSE true
    END)
);

-- ---------- Functions ----------

-- Sum of paid invoices in cents for a workspace. The `SET search_path`
-- attribute keeps the function body's `money_cents` reference resolvable
-- even when the caller has a different search_path.
CREATE OR REPLACE FUNCTION workspace_revenue_cents(ws uuid)
RETURNS money_cents
LANGUAGE sql STABLE
SET search_path = billing, public AS $$
    SELECT coalesce(sum(amount_cents), 0)::money_cents
    FROM billing.invoices
    WHERE workspace_id = ws AND status = 'paid'
$$;

-- Whether a user is a member of a workspace, with optional minimum role.
CREATE OR REPLACE FUNCTION is_member(ws uuid, u uuid, min_role role DEFAULT 'viewer')
RETURNS boolean
LANGUAGE sql STABLE
SET search_path = billing, public AS $$
    SELECT EXISTS (
        SELECT 1 FROM billing.memberships
        WHERE workspace_id = ws AND user_id = u
          AND CASE min_role
              WHEN 'owner'  THEN role = 'owner'
              WHEN 'admin'  THEN role IN ('owner', 'admin')
              WHEN 'member' THEN role IN ('owner', 'admin', 'member')
              ELSE TRUE
          END
    )
$$;

-- Set-returning function for common queries.
CREATE OR REPLACE FUNCTION upcoming_invoices(window_days int)
RETURNS TABLE (
    invoice_id uuid,
    workspace  text,
    due_at     timestamptz,
    amount     money_cents
) LANGUAGE sql STABLE
SET search_path = billing, public AS $$
    SELECT i.id, w.name, i.due_at, i.amount_cents
    FROM billing.invoices i
    JOIN billing.workspaces w ON w.id = i.workspace_id
    WHERE i.status = 'open' AND i.due_at < now() + (window_days || ' days')::interval
    ORDER BY i.due_at
$$;

-- ---------- Views ----------
-- Exercise swell's view-recursion: `pg_get_viewdef` + recursive build
-- so the analyzer can see the underlying outer-join widening on
-- `w.last_login_at`.
CREATE OR REPLACE VIEW workspace_overview AS
SELECT
    w.id          AS workspace_id,
    w.name        AS workspace_name,
    u.email       AS owner_email,
    u.last_login_at AS owner_last_login_at,
    count(m.user_id)::bigint AS member_count
FROM billing.workspaces w
LEFT JOIN billing.memberships m ON m.workspace_id = w.id
LEFT JOIN billing.users      u ON u.id = m.user_id AND m.role = 'owner'
GROUP BY w.id, u.email, u.last_login_at;
```

# Common types

```ts
export interface BillingAuditEvents {
  id: string;
  workspace_id: string | null;
  actor_id: string | null;
  action: string;
  target_type: string;
  target_id: string | null;
  payload: Json;
  created_at: Date;
}
export interface BillingBlocksBase {
  id: string;
  kind: string;
  url: string | null;
  body: string | null;
}
export type BillingBlocks = BillingBlocksBase & ({ body: null; kind: "image"; url: string } | { body: string; kind: "text"; url: null });
export interface BillingContactsBase {
  id: string;
  email: string | null;
  phone: string | null;
}
export type BillingContacts = BillingContactsBase & ({ email: string; phone: null } | { email: null; phone: string });
export interface BillingFeatureFlags {
  id: string;
  scope: "global" | "workspace" | "user";
  tier: "free" | "pro" | "enterprise";
  pinned_to: "beta" | null;
  locked_value: "on";
}
export interface BillingFieldConfigsBase {
  id: string;
  field_type: string;
  config: Json;
}
export type BillingFieldConfigs = BillingFieldConfigsBase & ({ config: { maxLength: number } & Record<string, Json>; field_type: "text" } | { config: { options: Json[] } & Record<string, Json>; field_type: "select" });
export interface BillingInvoices {
  id: string;
  subscription_id: string;
  workspace_id: string;
  number: string;
  status: "draft" | "open" | "paid" | "void" | "uncollectible";
  amount_cents: string;
  issued_at: Date;
  paid_at: Date | null;
  due_at: Date;
  notes: string | null;
}
export interface BillingMemberships {
  workspace_id: string;
  user_id: string;
  role: "owner" | "admin" | "member" | "viewer";
  joined_at: Date;
  invited_by: string | null;
}
export interface BillingPayloads {
  id: string;
  payload: { body: string; kind: "text" } & Record<string, Json> | { kind: "image"; url: string } & Record<string, Json>;
}
export interface BillingPlans {
  id: string;
  code: string;
  name: string;
  price_cents: string;
  bill_interval: "monthly" | "yearly";
  features: Json;
  is_archived: boolean;
}
export interface BillingPromotions {
  id: string;
  workspace_id: string;
  code: string;
  valid_during: { lower: Date | null; upper: Date | null };
  blackout_periods: { lower: Date | null; upper: Date | null } | null;
  eligible_roles: ("owner" | "admin" | "member" | "viewer")[];
  discount_pct: string;
  code_lower: string | null;
}
export interface BillingShipmentsBase {
  id: string;
  status: string;
  paid_at: Date | null;
}
export type BillingShipments = BillingShipmentsBase & ({ paid_at: Date; status: "paid" } | { paid_at: null; status: "draft" } | { status: Exclude<string, "paid" | "draft"> });
export interface BillingSubscriptions {
  id: string;
  workspace_id: string;
  plan_id: string;
  status: "trialing" | "active" | "past_due" | "canceled" | "incomplete";
  trial_ends_at: Date | null;
  current_period_start: Date;
  current_period_end: Date;
  canceled_at: Date | null;
  created_at: Date;
}
export interface BillingUsers {
  id: string;
  email: string;
  display_name: string | null;
  password_hash: string;
  avatar_url: string | null;
  created_at: Date;
  last_login_at: Date | null;
  metadata: Json;
}
export interface BillingWidgets {
  id: string;
  meta: { height: number; width: number } & Record<string, Json>;
}
export interface BillingWorkspaceOverview {
  workspace_id: string | null;
  workspace_name: string | null;
  owner_email: string | null;
  owner_last_login_at: Date | null;
  member_count: string | null;
}
export interface BillingWorkspaces {
  id: string;
  slug: string;
  name: string;
  billing_email: string;
  billing_address: { line1: string | null; line2: string | null; city: string | null; region: string | null; country: string | null; postal: string | null } | null;
  created_at: Date;
  deleted_at: Date | null;
  settings: Json;
}
```

# Tests

## Select with inner join preserves not null

```sql
SELECT u.email, u.display_name, m.role, m.joined_at
FROM billing.users u
JOIN billing.memberships m ON m.user_id = u.id
WHERE m.workspace_id = $1
```

```ts
$1: string | null
result: { email: BillingUsers["email"]; display_name: BillingUsers["display_name"]; role: BillingMemberships["role"]; joined_at: BillingMemberships["joined_at"] }
```

## Left join makes rhs columns nullable

```sql
SELECT w.id, w.name, s.status, s.current_period_end
FROM billing.workspaces w
LEFT JOIN billing.subscriptions s ON s.workspace_id = w.id
WHERE w.deleted_at IS NULL
```

```ts
result: { id: BillingWorkspaces["id"]; name: BillingWorkspaces["name"]; status: BillingSubscriptions["status"] | null; current_period_end: BillingSubscriptions["current_period_end"] | null }
```

## Full outer join makes both sides nullable

```sql
SELECT a.email AS left_email, b.email AS right_email
FROM billing.users a
FULL OUTER JOIN billing.users b ON a.id = b.id
WHERE a.id = $1 OR b.id = $2
```

```ts
$1: string | null
$2: string | null
result: { left_email: BillingUsers["email"]; right_email: null } | { left_email: null; right_email: BillingUsers["email"] } | { left_email: BillingUsers["email"]; right_email: BillingUsers["email"] }
```

## Self join with aliases

```sql
SELECT u.email AS member_email, inv.email AS invited_by_email
FROM billing.memberships m
JOIN billing.users u ON u.id = m.user_id
LEFT JOIN billing.users inv ON inv.id = m.invited_by
WHERE m.workspace_id = $1
```

```ts
$1: string | null
result: { member_email: BillingUsers["email"]; invited_by_email: BillingUsers["email"] | null }
```

## Cross join does not introduce nulls

```sql
SELECT u.email, p.code
FROM billing.users u CROSS JOIN billing.plans p
WHERE u.id = $1
```

```ts
$1: string | null
result: { email: BillingUsers["email"]; code: BillingPlans["code"] }
```

## Count and sum classification

```sql
SELECT
    count(*) AS total_invoices,
    count(paid_at) AS paid_count,
    sum(amount_cents) AS total_cents,
    avg(amount_cents) AS avg_cents,
    min(issued_at) AS earliest,
    max(issued_at) AS latest
FROM billing.invoices
WHERE workspace_id = $1
```

```ts
$1: string | null
result: { total_invoices: string; paid_count: string; total_cents: string | null; avg_cents: string | null; earliest: Date | null; latest: Date | null }
```

## Group by having

```sql
SELECT workspace_id, count(*) AS member_count
FROM billing.memberships
GROUP BY workspace_id
HAVING count(*) > 1
```

```ts
result: { workspace_id: BillingMemberships["workspace_id"]; member_count: string }
```

## Coalesce with literal fallback

```sql
SELECT coalesce(sum(amount_cents), 0) AS total_cents
FROM billing.invoices WHERE workspace_id = $1 AND status = 'paid'
```

```ts
$1: string | null
result: { total_cents: string }
```

## Insert returning with defaults

```sql
INSERT INTO billing.users (email, password_hash)
VALUES ($1, $2)
RETURNING id, email, created_at, last_login_at
```

```ts
$1: string
$2: string
result: { id: BillingUsers["id"]; email: BillingUsers["email"]; created_at: BillingUsers["created_at"]; last_login_at: BillingUsers["last_login_at"] }
```

## Insert on conflict returning

```sql
INSERT INTO billing.users (email, password_hash)
VALUES ($1, $2)
ON CONFLICT (email) DO UPDATE
    SET password_hash = EXCLUDED.password_hash
RETURNING id, email
```

```ts
$1: string
$2: string
result: { id: BillingUsers["id"]; email: BillingUsers["email"] }
```

## Update returning old and new

```sql
UPDATE billing.invoices
SET status = 'paid', paid_at = now()
WHERE id = $1 AND status = 'open'
RETURNING id, status, paid_at, amount_cents
```

```ts
$1: string | null
result: { id: BillingInvoices["id"]; status: BillingInvoices["status"]; paid_at: BillingInvoices["paid_at"]; amount_cents: BillingInvoices["amount_cents"] }
```

## Update returning with override corrects nullability

```sql
UPDATE billing.invoices
SET paid_at = now()
WHERE id = $1
RETURNING paid_at AS "paid_at!"
```

```ts
$1: string | null
result: { "paid_at!": BillingInvoices["paid_at"] }
```

## Delete returning

```sql
DELETE FROM billing.audit_events
WHERE workspace_id = $1 AND created_at < now() - interval '90 days'
RETURNING id, action
```

```ts
$1: string | null
result: { id: BillingAuditEvents["id"]; action: BillingAuditEvents["action"] }
```

## Scalar subquery in select

```sql
SELECT
    w.name,
    (SELECT count(*) FROM billing.memberships m WHERE m.workspace_id = w.id) AS members
FROM billing.workspaces w WHERE w.id = $1
```

```ts
$1: string | null
result: { name: BillingWorkspaces["name"]; members: string }
```

## Exists subquery in where doesnt change select types

```sql
SELECT id, name FROM billing.workspaces w
WHERE EXISTS (
    SELECT 1 FROM billing.memberships m
    WHERE m.workspace_id = w.id AND m.user_id = $1
)
```

```ts
$1: string | null
result: { id: BillingWorkspaces["id"]; name: BillingWorkspaces["name"] }
```

## Derived table in from

```sql
SELECT t.workspace_id, t.cnt
FROM (
    SELECT workspace_id, count(*) AS cnt
    FROM billing.invoices GROUP BY workspace_id
) t
WHERE t.cnt > 5
```

```ts
result: { workspace_id: BillingInvoices["workspace_id"]; cnt: string }
```

## Non recursive cte

```sql
WITH active_subs AS (
    SELECT workspace_id, plan_id
    FROM billing.subscriptions
    WHERE status = 'active'
)
SELECT a.workspace_id, p.name AS plan_name
FROM active_subs a
JOIN billing.plans p ON p.id = a.plan_id
```

```ts
result: { workspace_id: BillingSubscriptions["workspace_id"]; plan_name: BillingPlans["name"] }
```

## Recursive cte for audit chain

```sql
WITH RECURSIVE n(level) AS (
    SELECT 0
    UNION ALL
    SELECT level + 1 FROM n WHERE level < 10
)
SELECT level FROM n
```

```ts
result: { level: number }
```

## Row number over partition

```sql
SELECT
    w.name,
    row_number() OVER (PARTITION BY w.id ORDER BY i.issued_at DESC) AS rn,
    i.amount_cents
FROM billing.workspaces w
JOIN billing.invoices i ON i.workspace_id = w.id
```

```ts
result: { name: BillingWorkspaces["name"]; rn: string; amount_cents: BillingInvoices["amount_cents"] }
```

## Lag lead returns nullable

```sql
SELECT
    i.issued_at,
    lag(i.issued_at) OVER (PARTITION BY i.workspace_id ORDER BY i.issued_at) AS prev_issued
FROM billing.invoices i
```

```ts
result: { issued_at: BillingInvoices["issued_at"]; prev_issued: Date | null }
```

## Calling custom scalar function

```sql
SELECT billing.workspace_revenue_cents(w.id) AS revenue
FROM billing.workspaces w WHERE w.id = $1
```

```ts
$1: string | null
result: { revenue: string | null }
```

## Boolean function call

```sql
SELECT billing.is_member($1, $2, 'admin') AS allowed
```

```ts
$1: string | null
$2: string | null
result: { allowed: boolean | null }
```

## Set returning function in from

```sql
SELECT * FROM billing.upcoming_invoices(30)
```

```ts
result: { invoice_id: string | null; workspace: string | null; due_at: Date | null; amount: string | null }
```

## Case with else keeps unknown

```sql
SELECT
    CASE WHEN status = 'paid' THEN amount_cents ELSE 0 END AS recognised,
    CASE WHEN status = 'paid' THEN amount_cents END AS pending
FROM billing.invoices WHERE id = $1
```

```ts
$1: string | null
result: { recognised: string; pending: string | null }
```

## Nullif is nullable

```sql
SELECT nullif($1::text, '') AS t
```

```ts
$1: string | null
result: { t: string | null }
```

## Explicit cast

```sql
SELECT $1::int4 + 1 AS n
```

```ts
$1: number | null
result: { n: number | null }
```

## Union of two selects

```sql
SELECT id, 'paid' AS bucket FROM billing.invoices WHERE status = 'paid'
UNION ALL
SELECT id, 'open' FROM billing.invoices WHERE status = 'open'
```

```ts
result: { id: string; bucket: "paid" | "open" }
```

## Json shape with aliases and join

```sql
SELECT jsonb_build_object(
    'workspace_id', w.id,
    'workspace_name', w.name,
    'plan', p.code,
    'status', s.status,
    'mrr_cents', billing.workspace_revenue_cents(w.id)
) AS summary
FROM billing.workspaces w
JOIN billing.subscriptions s ON s.workspace_id = w.id
JOIN billing.plans p ON p.id = s.plan_id
WHERE w.id = $1
```

```ts
$1: string | null
result: { summary: { workspace_id: string; workspace_name: string; plan: string; status: "trialing" | "active" | "past_due" | "canceled" | "incomplete"; mrr_cents: string | null } }
```

## Json shape aggregate with group by

```sql
SELECT
    w.id,
    jsonb_agg(jsonb_build_object('member', u.email, 'role', m.role)) AS members
FROM billing.workspaces w
JOIN billing.memberships m ON m.workspace_id = w.id
JOIN billing.users u ON u.id = m.user_id
WHERE w.id = $1
GROUP BY w.id
```

```ts
$1: string | null
result: { id: BillingWorkspaces["id"]; members: { member: string; role: "owner" | "admin" | "member" | "viewer" }[] | null }
```

## Json shape dynamic key generates an idiomatic record type (if constants are last)

```sql
SELECT jsonb_build_object(
    u.email, u.id,
    'role', m.role
) AS lookup
FROM billing.users u JOIN billing.memberships m ON m.user_id = u.id
WHERE u.id = $1
```

```ts
$1: string | null
result: { lookup: { [k: string]: string; role: "owner" | "admin" | "member" | "viewer" } }
```

## Json shape dynamic key collapses to record if constants are first

```sql
SELECT jsonb_build_object(
    'role', m.role,
    u.email, u.id
) AS lookup
FROM billing.users u JOIN billing.memberships m ON m.user_id = u.id
WHERE u.id = $1
```

```ts
$1: string | null
result: { lookup: Record<string, string | "owner" | "admin" | "member" | "viewer"> }
```

## Composite postal address renders as object

```sql
SELECT billing_address FROM billing.workspaces WHERE id = $1
```

```ts
$1: string | null
result: { billing_address: BillingWorkspaces["billing_address"] }
```

## Enum column rendered as union

```sql
SELECT status FROM billing.subscriptions WHERE workspace_id = $1
```

```ts
$1: string | null
result: { status: BillingSubscriptions["status"] }
```

## Domain money cents renders as string

```sql
SELECT price_cents FROM billing.plans WHERE code = $1
```

```ts
$1: string | null
result: { price_cents: BillingPlans["price_cents"] }
```

## not null from s.id propagates to status on left join through inference

```sql
SELECT w.id, s.status AS "status"
FROM billing.workspaces w
LEFT JOIN billing.subscriptions s ON s.workspace_id = w.id
WHERE s.id IS NOT NULL AND w.id = $1
```

```ts
$1: string | null
result: { id: BillingWorkspaces["id"]; status: BillingSubscriptions["status"] }
```

## Force not null on left join with filter

```sql
SELECT w.id, s.status AS "status!"
FROM billing.workspaces w
LEFT JOIN billing.subscriptions s ON s.workspace_id = w.id
WHERE w.id = $1
```

```ts
$1: string | null
result: { id: BillingWorkspaces["id"]; "status!": BillingSubscriptions["status"] }
```

## Override typed jsonb

```sql
SELECT settings AS "settings"
FROM billing.workspaces WHERE id = $1
```

```ts
$1: string | null
result: { settings: BillingWorkspaces["settings"] }
```

## Cross join lateral unnest array

```sql
SELECT u.email, t.label
FROM billing.users u
CROSS JOIN LATERAL unnest(ARRAY['admin', 'member']::text[]) AS t(label)
WHERE u.id = $1
```

```ts
$1: string | null
result: { email: BillingUsers["email"]; label: string }
```

## Left join lateral subquery makes columns nullable

```sql
SELECT u.email, latest.id AS latest_invoice_id, latest.amount_cents
FROM billing.users u
LEFT JOIN LATERAL (
    SELECT i.id, i.amount_cents
    FROM billing.invoices i
    JOIN billing.workspaces w ON w.id = i.workspace_id
    JOIN billing.memberships m ON m.workspace_id = w.id AND m.user_id = u.id
    ORDER BY i.issued_at DESC
    LIMIT 1
) latest ON TRUE
WHERE u.id = $1
```

```ts
$1: string | null
result: { email: BillingUsers["email"]; latest_invoice_id: BillingInvoices["id"] | null; amount_cents: BillingInvoices["amount_cents"] | null }
```

## Jsonb arrow operators

```sql
SELECT
    metadata->'theme' AS theme_jsonb,
    metadata->>'theme' AS theme_text,
    (metadata->>'count')::int AS theme_count
FROM billing.users WHERE id = $1
```

```ts
$1: string | null
result: { theme_jsonb: Json | null; theme_text: string | null; theme_count: number | null }
```

## Jsonb containment returns boolean

```sql
SELECT metadata @> $1::jsonb AS has_subset
FROM billing.users WHERE id = $2
```

```ts
$1: Json | null
$2: string | null
result: { has_subset: boolean | null }
```

## Select star expands to the row type

```sql
SELECT * FROM billing.users WHERE id = $1
```

```ts
$1: string | null
result: BillingUsers
```

## Select star through left join

```sql
SELECT u.*, m.role
FROM billing.users u
LEFT JOIN billing.memberships m ON m.user_id = u.id AND m.workspace_id = $1
WHERE u.id = $2
```

```ts
$1: string | null
$2: string | null
result: BillingUsers & { role: BillingMemberships["role"] | null }
```

## Array agg with order by

```sql
SELECT array_agg(u.email ORDER BY u.email) AS emails
FROM billing.users u
JOIN billing.memberships m ON m.user_id = u.id
WHERE m.workspace_id = $1
```

```ts
$1: string | null
result: { emails: string[] | null }
```

## Filter clause on count

```sql
SELECT
    count(*) AS total,
    count(*) FILTER (WHERE status = 'paid') AS paid,
    count(*) FILTER (WHERE status = 'open') AS open
FROM billing.invoices WHERE workspace_id = $1
```

```ts
$1: string | null
result: { total: string; paid: string; open: string }
```

## Grouping sets make keys nullable

```sql
SELECT
    workspace_id,
    status,
    count(*) AS n
FROM billing.invoices
GROUP BY GROUPING SETS ((workspace_id), (status), ())
```

```ts
result: { workspace_id: BillingInvoices["workspace_id"]; status: null; n: string } | { workspace_id: null; status: BillingInvoices["status"]; n: string } | { workspace_id: null; status: null; n: string }
```

## Param used with any array cast

```sql
SELECT id, email FROM billing.users WHERE id = ANY($1::uuid[])
```

```ts
$1: string[] | null
result: { id: BillingUsers["id"]; email: BillingUsers["email"] }
```

## Param explicit text in is null

```sql
SELECT id FROM billing.users WHERE $1::text IS NULL OR id = $2
```

```ts
$1: string | null
$2: string | null
result: { id: BillingUsers["id"] }
```

## Param repeated use is one entry

```sql
SELECT id FROM billing.users WHERE id = $1 OR email = $1::text
```

```ts
$1: string | null
result: { id: BillingUsers["id"] }
```

## Values derived table

```sql
SELECT t.k, t.v
FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c')) AS t(k, v)
```

```ts
result: { k: number; v: string }
```

## Values with override forces not null

```sql
SELECT t.id AS "id", t.label
FROM (VALUES ('a'::text, 'first'), ('b', 'second')) AS t(id, label)
```

```ts
result: { id: string; label: string }
```

## Unknown column returns error

```sql
SELECT no_such_column FROM billing.users
```

```ts
error: no_such_column
```

## Unknown table returns error

```sql
SELECT 1 FROM billing.no_such_table
```

```ts
error: no_such_table
```

## Syntax error returns error

```sql
SELECT id FORM billing.users
```

```ts
error: syntax error
```

## Insert select returning

```sql
INSERT INTO billing.audit_events (workspace_id, action, target_type, target_id, payload)
SELECT s.workspace_id, 'subscription.renewed', 'subscription', s.id::text, '{}'::jsonb
FROM billing.subscriptions s
WHERE s.current_period_end < now() + interval '1 day'
RETURNING id, action, created_at
```

```ts
result: { id: BillingAuditEvents["id"]; action: BillingAuditEvents["action"]; created_at: BillingAuditEvents["created_at"] }
```

## Insert on conflict do nothing returning

```sql
INSERT INTO billing.users (email, password_hash) VALUES ($1, $2)
ON CONFLICT (email) DO NOTHING
RETURNING id, email
```

```ts
$1: string
$2: string
result: { id: BillingUsers["id"]; email: BillingUsers["email"] }
```

## Distinct on keeps attnotnull

```sql
SELECT DISTINCT ON (workspace_id)
    workspace_id, status, issued_at
FROM billing.invoices
ORDER BY workspace_id, issued_at DESC
```

```ts
result: { workspace_id: BillingInvoices["workspace_id"]; status: BillingInvoices["status"]; issued_at: BillingInvoices["issued_at"] }
```

## Select distinct

```sql
SELECT DISTINCT status FROM billing.invoices
```

```ts
result: { status: BillingInvoices["status"] }
```

## Unnest with ordinality

```sql
SELECT t.label, t.idx
FROM unnest(ARRAY['a', 'b', 'c']::text[]) WITH ORDINALITY AS t(label, idx)
```

```ts
result: { label: string; idx: string }
```

## Intersect two selects

```sql
SELECT id FROM billing.users
INTERSECT
SELECT user_id FROM billing.memberships WHERE workspace_id = $1
```

```ts
$1: string | null
result: { id: string }
```

## Except all

```sql
SELECT id FROM billing.users
EXCEPT ALL
SELECT user_id FROM billing.memberships WHERE workspace_id = $1
```

```ts
$1: string | null
result: { id: string }
```

## Range column renders as lower upper

```sql
SELECT valid_during FROM billing.promotions WHERE id = $1
```

```ts
$1: string | null
result: { valid_during: BillingPromotions["valid_during"] }
```

## Multirange column renders as lower upper too

```sql
SELECT blackout_periods FROM billing.promotions WHERE id = $1
```

```ts
$1: string | null
result: { blackout_periods: BillingPromotions["blackout_periods"] }
```

## Array of enum renders as paren union

```sql
SELECT eligible_roles FROM billing.promotions WHERE id = $1
```

```ts
$1: string | null
result: { eligible_roles: BillingPromotions["eligible_roles"] }
```

## Generated identity is not null

```sql
SELECT id, code, code_lower FROM billing.promotions WHERE id = $1
```

```ts
$1: string | null
result: { id: BillingPromotions["id"]; code: BillingPromotions["code"]; code_lower: BillingPromotions["code_lower"] }
```

## Generated stored column is nullable when expr can be

```sql
SELECT code_lower FROM billing.promotions WHERE id = $1
```

```ts
$1: string | null
result: { code_lower: BillingPromotions["code_lower"] }
```

## Insert into table with identity does not require id param

```sql
INSERT INTO billing.promotions (workspace_id, code, valid_during, discount_pct)
VALUES ($1, $2, $3, $4)
RETURNING id, code, code_lower
```

```ts
$1: string
$2: string
$3: { lower: Date | null; upper: Date | null }
$4: string
result: { id: BillingPromotions["id"]; code: BillingPromotions["code"]; code_lower: BillingPromotions["code_lower"] }
```

## Jsonb object agg emits record

```sql
SELECT jsonb_object_agg(role::text, c::text) AS by_role
FROM (
    SELECT role, count(*) AS c FROM billing.memberships
    WHERE workspace_id = $1 GROUP BY role
) t
```

```ts
$1: string | null
result: { by_role: Record<string, string> | null }
```

## Merge with returning

```sql
MERGE INTO billing.invoices i
USING (SELECT $1::uuid AS id, $2::text AS new_status) src
    ON src.id = i.id
WHEN MATCHED THEN
    UPDATE SET status = src.new_status::billing.invoice_status
WHEN NOT MATCHED THEN
    DO NOTHING
RETURNING i.id, i.status
```

```ts
$1: string | null
$2: string | null
result: { id: BillingInvoices["id"]; status: BillingInvoices["status"] }
```

## Scalar sublink count is non-null

```sql
SELECT (SELECT count(*) FROM billing.users) AS n
```

```ts
result: { n: string }
```

## Scalar sublink with non-aggregate body is nullable

```sql
SELECT (SELECT email FROM billing.users WHERE id = $1) AS e
```

```ts
$1: string | null
result: { e: string | null }
```

## Scalar sublink with nullable aggregate stays nullable

```sql
SELECT (SELECT max(amount_cents) FROM billing.invoices) AS top
```

```ts
result: { top: string | null }
```

## Exists subquery is non-null boolean

```sql
SELECT EXISTS (SELECT 1 FROM billing.invoices WHERE status = 'paid') AS any_paid
```

```ts
result: { any_paid: boolean }
```

## Array subquery is non-null array

```sql
SELECT ARRAY(SELECT id FROM billing.users WHERE email = $1) AS ids
```

```ts
$1: string | null
result: { ids: string[] }
```

## Select from view with outer-join widening underneath

```sql
SELECT v.workspace_id, v.workspace_name, v.owner_email, v.owner_last_login_at, v.member_count
FROM billing.workspace_overview v
WHERE v.workspace_id = $1
```

```ts
$1: string | null
result: BillingWorkspaceOverview
```

## Bare column ref against view picks up non-null aggregate

```sql
SELECT member_count FROM billing.workspace_overview WHERE workspace_id = $1
```

```ts
$1: string | null
result: { member_count: BillingWorkspaceOverview["member_count"] }
```

## Check IN narrows the row type via table reference

```sql
SELECT scope FROM billing.feature_flags WHERE id = $1
```

```ts
$1: string | null
result: { scope: BillingFeatureFlags["scope"] }
```

## Check ANY narrows the row type via table reference

```sql
SELECT tier FROM billing.feature_flags WHERE id = $1
```

```ts
$1: string | null
result: { tier: BillingFeatureFlags["tier"] }
```

## Check equality narrows to a single literal

```sql
SELECT locked_value FROM billing.feature_flags WHERE id = $1
```

```ts
$1: string | null
result: { locked_value: BillingFeatureFlags["locked_value"] }
```

## Check IS NULL OR widens the union with null

```sql
SELECT pinned_to FROM billing.feature_flags WHERE id = $1
```

```ts
$1: string | null
result: { pinned_to: BillingFeatureFlags["pinned_to"] }
```

## Check tier 2 narrows jsonb to an object shape

```sql
SELECT meta FROM billing.widgets WHERE id = $1
```

```ts
$1: string | null
result: { meta: BillingWidgets["meta"] }
```

## Check tier 3 col-level OR over AND-chains narrows jsonb to a union

```sql
SELECT payload FROM billing.payloads WHERE id = $1
```

```ts
$1: string | null
result: { payload: BillingPayloads["payload"] }
```

## Check tier 3 row-level num_nonnulls emits a row variant intersection

```sql
SELECT email, phone FROM billing.contacts WHERE id = $1
```

```ts
$1: string | null
result: { email: BillingContacts["email"]; phone: BillingContacts["phone"] }
```

## Check tier 3 row-level CASE discriminates field configs by type

```sql
SELECT field_type, config FROM billing.field_configs WHERE id = $1
```

```ts
$1: string | null
result: { field_type: BillingFieldConfigs["field_type"]; config: BillingFieldConfigs["config"] }
```

## Check tier 2 jsonb arrow expression on narrowed col loses table ref

```sql
SELECT meta -> 'width' AS width FROM billing.widgets WHERE id = $1
```

```ts
$1: string | null
result: { width: Json | null }
```

## Select star against table with row variants preserves intersection

```sql
SELECT * FROM billing.contacts WHERE id = $1
```

```ts
$1: string | null
result: BillingContacts
```

## Check OR-of-AND with multi-column narrowing emits row variants

```sql
SELECT * FROM billing.blocks WHERE id = $1
```

```ts
$1: string | null
result: BillingBlocks
```

## Check CASE THEN with IS NOT NULL pins per-branch nullability

```sql
SELECT * FROM billing.shipments WHERE id = $1
```

```ts
$1: string | null
result: BillingShipments
```
