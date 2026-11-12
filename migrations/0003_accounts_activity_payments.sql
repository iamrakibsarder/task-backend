-- Accounts: each user owns one workspace. Passwords are Argon2id hashes.
CREATE TABLE users (
  id              TEXT PRIMARY KEY,
  email           TEXT NOT NULL UNIQUE CHECK (email = lower(email) AND position('@' in email) > 1),
  name            TEXT NOT NULL DEFAULT '',
  password_hash   TEXT NOT NULL,
  workspace_id    TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX users_by_workspace ON users (workspace_id);

-- Sign-in sessions. Only the SHA-256 of the bearer token is stored, so a database leak
-- doesn't hand out working tokens.
CREATE TABLE sessions (
  token_hash      TEXT PRIMARY KEY,
  user_id         TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_seen_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at      TIMESTAMPTZ NOT NULL
);
CREATE INDEX sessions_by_user ON sessions (user_id);

-- Who changed what, newest first.
CREATE TABLE activity (
  id              BIGSERIAL PRIMARY KEY,
  workspace_id    TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
  at              TIMESTAMPTZ NOT NULL DEFAULT now(),
  actor           TEXT NOT NULL DEFAULT '',
  action          TEXT NOT NULL CHECK (action IN ('create', 'update', 'delete', 'import', 'settings')),
  entity          TEXT NOT NULL,
  entity_id       TEXT NOT NULL DEFAULT '',
  summary         TEXT NOT NULL DEFAULT ''
);
CREATE INDEX activity_by_workspace ON activity (workspace_id, id DESC);

-- Partial payments received against an invoice.
ALTER TABLE invoices ADD COLUMN payments JSONB NOT NULL DEFAULT '[]';

ALTER TABLE users    ENABLE ROW LEVEL SECURITY;
ALTER TABLE sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE activity ENABLE ROW LEVEL SECURITY;
