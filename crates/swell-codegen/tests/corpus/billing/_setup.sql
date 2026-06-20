-- A realistic SaaS billing schema. Loaded fresh for the corpus tests.
-- Drops everything first so the tests can re-run without manual cleanup.

DROP SCHEMA IF EXISTS billing CASCADE;
CREATE SCHEMA billing;
SET search_path = billing, public;

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
