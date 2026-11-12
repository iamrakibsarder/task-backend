-- A private, read-only page per client listing their invoices (no account needed).
-- The token is kept as is so it can go into invoice emails; it only opens that one
-- client's sent invoices. Turning the link off or making a new one stops the old link.
CREATE TABLE client_portals (
  workspace_id    TEXT NOT NULL,
  client_id       TEXT NOT NULL,
  token           TEXT NOT NULL UNIQUE CHECK (length(token) >= 32),
  enabled         BOOLEAN NOT NULL DEFAULT true,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_viewed_at  TIMESTAMPTZ,
  PRIMARY KEY (workspace_id, client_id),
  FOREIGN KEY (workspace_id, client_id) REFERENCES clients (workspace_id, id) ON DELETE CASCADE
);

ALTER TABLE client_portals ENABLE ROW LEVEL SECURITY;
