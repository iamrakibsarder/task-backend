-- Teams: a workspace can have many members. `users.workspace_id` is now the workspace the
-- person is currently working in; `memberships` says which workspaces they belong to.
CREATE TABLE memberships (
  workspace_id    TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
  user_id         TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  role            TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX memberships_by_user ON memberships (user_id);

-- Everyone who exists today owns the workspace they were using.
INSERT INTO memberships (workspace_id, user_id, role)
SELECT workspace_id, id, 'owner' FROM users;

-- Invitations by email. Only the SHA-256 of the link token is stored.
CREATE TABLE invites (
  id              TEXT PRIMARY KEY,
  workspace_id    TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
  email           TEXT NOT NULL CHECK (email = lower(email)),
  role            TEXT NOT NULL CHECK (role IN ('admin', 'member')),
  token_hash      TEXT NOT NULL UNIQUE,
  invited_by      TEXT REFERENCES users (id) ON DELETE SET NULL,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at      TIMESTAMPTZ NOT NULL,
  accepted_at     TIMESTAMPTZ
);
-- One open invite per email per workspace (inviting again replaces it).
CREATE UNIQUE INDEX invites_open ON invites (workspace_id, email) WHERE accepted_at IS NULL;

-- Password reset links: single use, short lived, hashed like sessions.
CREATE TABLE password_resets (
  token_hash      TEXT PRIMARY KEY,
  user_id         TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at      TIMESTAMPTZ NOT NULL,
  used_at         TIMESTAMPTZ
);
CREATE INDEX password_resets_by_user ON password_resets (user_id);

ALTER TABLE memberships     ENABLE ROW LEVEL SECURITY;
ALTER TABLE invites         ENABLE ROW LEVEL SECURITY;
ALTER TABLE password_resets ENABLE ROW LEVEL SECURITY;
