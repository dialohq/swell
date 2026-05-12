-- Test fixture schema. Mirrors examples/basic/migrations/0001_init.sql,
-- plus a `bank_accounts` table the transaction tests mutate.
CREATE TYPE user_role AS ENUM ('admin', 'member');

CREATE DOMAIN email_address AS text
    CHECK (VALUE ~ '^[^@]+@[^@]+$');

CREATE TABLE orgs (
    id   uuid PRIMARY KEY,
    name text NOT NULL
);

CREATE TABLE users (
    id           uuid PRIMARY KEY,
    org_id       uuid NOT NULL REFERENCES orgs(id),
    email        email_address NOT NULL,
    display_name text,
    role         user_role NOT NULL
);

CREATE TABLE bank_accounts (
    id      uuid PRIMARY KEY,
    balance bigint NOT NULL
);
