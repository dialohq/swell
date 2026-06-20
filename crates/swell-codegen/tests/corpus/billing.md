# [Setup](./billing.setup.sql)

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
export interface BillingWorkspaces {
  id: string;
  slug: string;
  name: string;
  billing_email: string;
  billing_address: { line1: unknown; line2: unknown; city: unknown; region: unknown; country: unknown; postal: unknown } | null;
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
result: { email: BillingUsers["email"]; display_name: BillingUsers["display_name"] | null; role: BillingMemberships["role"]; joined_at: BillingMemberships["joined_at"] }
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
result: { left_email: BillingUsers["email"] | null; right_email: BillingUsers["email"] | null }
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
result: { id: BillingUsers["id"]; email: BillingUsers["email"]; created_at: BillingUsers["created_at"]; last_login_at: BillingUsers["last_login_at"] | null }
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
result: { id: BillingInvoices["id"]; status: BillingInvoices["status"]; paid_at: BillingInvoices["paid_at"] | null; amount_cents: BillingInvoices["amount_cents"] }
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
result: { paid_at: BillingInvoices["paid_at"] }
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
result: { name: BillingWorkspaces["name"]; members: string | null }
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
result: { level: number | null }
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
result: { name: BillingWorkspaces["name"]; rn: string | null; amount_cents: BillingInvoices["amount_cents"] }
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
result: { id: string | null; bucket: string | null }
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
result: { summary: { workspace_id: string; workspace_name: string; plan: string; status: "trialing" | "active" | "past_due" | "canceled" | "incomplete"; mrr_cents: unknown } }
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

## Json shape dynamic key collapses to record

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
result: { lookup: Record<string, string | "owner" | "admin" | "member" | "viewer"> }
```

## Composite postal address renders as object

```sql
SELECT billing_address FROM billing.workspaces WHERE id = $1
```

```ts
$1: string | null
result: { billing_address: BillingWorkspaces["billing_address"] | null }
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

## Force not null on join with filter

```sql
SELECT w.id, s.status AS "status!"
FROM billing.workspaces w
LEFT JOIN billing.subscriptions s ON s.workspace_id = w.id
WHERE s.id IS NOT NULL AND w.id = $1
```

```ts
$1: string | null
result: { id: BillingWorkspaces["id"]; status: BillingSubscriptions["status"] }
```

## Override typed jsonb

```sql
SELECT settings AS "settings: WorkspaceSettings"
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
result: { email: BillingUsers["email"]; label: string | null }
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

## Select star expands to all columns with attnotnull

```sql
SELECT * FROM billing.users WHERE id = $1
```

```ts
$1: string | null
result: { id: BillingUsers["id"]; email: BillingUsers["email"]; display_name: BillingUsers["display_name"] | null; password_hash: BillingUsers["password_hash"]; avatar_url: BillingUsers["avatar_url"] | null; created_at: BillingUsers["created_at"]; last_login_at: BillingUsers["last_login_at"] | null; metadata: BillingUsers["metadata"] }
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
result: { id: BillingUsers["id"]; email: BillingUsers["email"]; display_name: BillingUsers["display_name"] | null; password_hash: BillingUsers["password_hash"]; avatar_url: BillingUsers["avatar_url"] | null; created_at: BillingUsers["created_at"]; last_login_at: BillingUsers["last_login_at"] | null; metadata: BillingUsers["metadata"]; role: BillingMemberships["role"] | null }
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
result: { workspace_id: BillingInvoices["workspace_id"]; status: BillingInvoices["status"]; n: string }
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
result: { k: number | null; v: string | null }
```

## Values with override forces not null

```sql
SELECT t.id AS "id!", t.label
FROM (VALUES ('a'::text, 'first'), ('b', 'second')) AS t(id, label)
```

```ts
result: { id: string; label: string | null }
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
result: { label: string | null; idx: string | null }
```

## Intersect two selects

```sql
SELECT id FROM billing.users
INTERSECT
SELECT user_id FROM billing.memberships WHERE workspace_id = $1
```

```ts
$1: string | null
result: { id: string | null }
```

## Except all

```sql
SELECT id FROM billing.users
EXCEPT ALL
SELECT user_id FROM billing.memberships WHERE workspace_id = $1
```

```ts
$1: string | null
result: { id: string | null }
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
result: { blackout_periods: BillingPromotions["blackout_periods"] | null }
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
result: { id: BillingPromotions["id"]; code: BillingPromotions["code"]; code_lower: BillingPromotions["code_lower"] | null }
```

## Generated stored column is nullable when expr can be

```sql
SELECT code_lower FROM billing.promotions WHERE id = $1
```

```ts
$1: string | null
result: { code_lower: BillingPromotions["code_lower"] | null }
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
result: { id: BillingPromotions["id"]; code: BillingPromotions["code"]; code_lower: BillingPromotions["code_lower"] | null }
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
result: { by_role: Record<string, unknown> | null }
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
