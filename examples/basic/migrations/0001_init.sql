-- Custom enum.
CREATE TYPE user_role AS ENUM ('admin', 'member');

-- Custom domain over text — should render as the underlying base TS type.
CREATE DOMAIN email_address AS text
    CHECK (VALUE ~ '^[^@]+@[^@]+$');

-- Custom composite type.
CREATE TYPE address AS (
    street text,
    city   text,
    zip    text
);

CREATE TABLE orgs (
    id   uuid PRIMARY KEY,
    name text NOT NULL
);

CREATE TABLE users (
    id           uuid PRIMARY KEY,
    org_id       uuid NOT NULL REFERENCES orgs(id),
    email        email_address NOT NULL,             -- domain over text
    display_name text,                               -- nullable
    role         user_role NOT NULL,                 -- enum
    home_address address,                            -- composite, nullable
    settings     jsonb NOT NULL
);

CREATE TABLE posts (
    id           uuid PRIMARY KEY,
    author_id    uuid NOT NULL REFERENCES users(id),
    body         text NOT NULL,
    published_at timestamptz                         -- nullable
);
