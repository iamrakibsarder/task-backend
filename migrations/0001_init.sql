-- TaskX schema. Mirrors taskx-frontend/src/types/domain.ts.
-- Every row belongs to a workspace, so one database can host many businesses
-- once sign-in is added (workspace_id will come from the signed-in user).

CREATE TABLE workspaces (
  id              TEXT PRIMARY KEY,
  business_name   TEXT NOT NULL DEFAULT '',
  owner_name      TEXT NOT NULL DEFAULT '',
  email           TEXT NOT NULL DEFAULT '',
  address         TEXT NOT NULL DEFAULT '',
  currency        TEXT NOT NULL DEFAULT 'USD' CHECK (currency ~ '^[A-Z]{3}$'),
  default_rate    NUMERIC(12, 2) NOT NULL DEFAULT 50 CHECK (default_rate >= 0),
  invoice_prefix  TEXT NOT NULL DEFAULT 'INV-',
  terms_days      INTEGER NOT NULL DEFAULT 14 CHECK (terms_days BETWEEN 0 AND 365),
  tax_rate        NUMERIC(7, 3) NOT NULL DEFAULT 0 CHECK (tax_rate BETWEEN 0 AND 100),
  tax_label       TEXT NOT NULL DEFAULT 'Tax',
  payment_info    TEXT NOT NULL DEFAULT '',
  invoice_note    TEXT NOT NULL DEFAULT 'Thank you for your business.',
  auto_approve    BOOLEAN NOT NULL DEFAULT FALSE,
  team            BOOLEAN NOT NULL DEFAULT FALSE,
  members         TEXT[] NOT NULL DEFAULT '{}',
  rounding        TEXT NOT NULL DEFAULT 'nearest' CHECK (rounding IN ('nearest', 'up', 'down')),
  -- '' = nothing locked, otherwise YYYY-MM (that month and earlier are read-only)
  locked_through  TEXT NOT NULL DEFAULT '' CHECK (locked_through = '' OR locked_through ~ '^\d{4}-(0[1-9]|1[0-2])$'),
  -- minutes between timer screenshots, 0 = off
  screenshot_minutes INTEGER NOT NULL DEFAULT 5 CHECK (screenshot_minutes BETWEEN 0 AND 240),
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE clients (
  workspace_id    TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
  id              TEXT NOT NULL,
  name            TEXT NOT NULL CHECK (length(trim(name)) > 0),
  color           TEXT NOT NULL DEFAULT '#7d22f2',
  contact         TEXT NOT NULL DEFAULT '',
  email           TEXT NOT NULL DEFAULT '',
  address         TEXT NOT NULL DEFAULT '',
  billing         TEXT NOT NULL DEFAULT 'hourly' CHECK (billing IN ('hourly', 'retainer')),
  rate            NUMERIC(12, 2) CHECK (rate >= 0),
  retainer_fee    NUMERIC(12, 2) NOT NULL DEFAULT 0 CHECK (retainer_fee >= 0),
  included_hours  NUMERIC(8, 2) NOT NULL DEFAULT 0 CHECK (included_hours >= 0),
  cap             NUMERIC(8, 2) NOT NULL DEFAULT 0 CHECK (cap >= 0),
  active          BOOLEAN NOT NULL DEFAULT TRUE,
  created_ms      BIGINT NOT NULL DEFAULT 0,
  sample          BOOLEAN NOT NULL DEFAULT FALSE,
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, id)
);

CREATE TABLE projects (
  workspace_id    TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
  id              TEXT NOT NULL,
  client_id       TEXT,
  name            TEXT NOT NULL CHECK (length(trim(name)) > 0),
  rate            NUMERIC(12, 2) CHECK (rate >= 0),
  internal        BOOLEAN NOT NULL DEFAULT FALSE,
  archived        BOOLEAN NOT NULL DEFAULT FALSE,
  budget_hours    NUMERIC(8, 2) CHECK (budget_hours >= 0),
  sample          BOOLEAN NOT NULL DEFAULT FALSE,
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, id),
  FOREIGN KEY (workspace_id, client_id) REFERENCES clients (workspace_id, id) ON DELETE CASCADE,
  CHECK (internal OR client_id IS NOT NULL)
);

CREATE TABLE invoices (
  workspace_id    TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
  id              TEXT NOT NULL,
  seq             INTEGER NOT NULL CHECK (seq > 0),
  number          TEXT NOT NULL,
  client_id       TEXT NOT NULL,
  client          JSONB NOT NULL,          -- client details frozen on the invoice
  sender          JSONB,                   -- sender details frozen when sent ("from" in the API)
  period          TEXT NOT NULL CHECK (period ~ '^\d{4}-(0[1-9]|1[0-2])$'),
  issue_date      DATE NOT NULL,
  due_date        DATE NOT NULL,
  currency        TEXT NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  lines           JSONB NOT NULL DEFAULT '[]',
  discount        NUMERIC(14, 2) NOT NULL DEFAULT 0 CHECK (discount >= 0),
  tax_rate        NUMERIC(7, 3) NOT NULL DEFAULT 0 CHECK (tax_rate BETWEEN 0 AND 100),
  tax_label       TEXT NOT NULL DEFAULT 'Tax',
  notes           TEXT NOT NULL DEFAULT '',
  status          TEXT NOT NULL CHECK (status IN ('draft', 'sent', 'paid', 'void')),
  sent_at         DATE,
  paid_at         DATE,
  paid_note       TEXT,
  credits         JSONB NOT NULL DEFAULT '[]',
  created_ms      BIGINT NOT NULL DEFAULT 0,
  sample          BOOLEAN NOT NULL DEFAULT FALSE,
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, id),
  UNIQUE (workspace_id, number),
  FOREIGN KEY (workspace_id, client_id) REFERENCES clients (workspace_id, id),
  CHECK (due_date >= issue_date)
);

CREATE TABLE tasks (
  workspace_id    TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
  id              TEXT NOT NULL,
  date            DATE NOT NULL,
  project_id      TEXT NOT NULL,
  memo            TEXT NOT NULL CHECK (length(trim(memo)) > 0),
  link            TEXT NOT NULL DEFAULT '',
  minutes         INTEGER NOT NULL CHECK (minutes > 0 AND minutes % 15 = 0),
  status          TEXT NOT NULL CHECK (status IN ('review', 'billable', 'invoiced', 'waived', 'internal')),
  by_name         TEXT NOT NULL DEFAULT '',
  invoice_id      TEXT,
  waive_reason    TEXT,
  merged_from     JSONB,                   -- original entries when this task was made by merging
  created_ms      BIGINT NOT NULL DEFAULT 0,
  sample          BOOLEAN NOT NULL DEFAULT FALSE,
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, id),
  FOREIGN KEY (workspace_id, project_id) REFERENCES projects (workspace_id, id),
  FOREIGN KEY (workspace_id, invoice_id) REFERENCES invoices (workspace_id, id),
  CHECK ((status = 'invoiced') = (invoice_id IS NOT NULL))
);

CREATE TABLE expenses (
  workspace_id    TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
  id              TEXT NOT NULL,
  date            DATE NOT NULL,
  client_id       TEXT,
  description     TEXT NOT NULL CHECK (length(trim(description)) > 0),
  amount          NUMERIC(14, 2) NOT NULL CHECK (amount >= 0),
  markup          NUMERIC(6, 2) NOT NULL DEFAULT 0 CHECK (markup >= 0),
  billable        BOOLEAN NOT NULL DEFAULT TRUE,
  status          TEXT NOT NULL DEFAULT 'unbilled' CHECK (status IN ('unbilled', 'invoiced')),
  invoice_id      TEXT,
  sample          BOOLEAN NOT NULL DEFAULT FALSE,
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, id),
  FOREIGN KEY (workspace_id, client_id) REFERENCES clients (workspace_id, id) ON DELETE SET NULL (client_id),
  FOREIGN KEY (workspace_id, invoice_id) REFERENCES invoices (workspace_id, id)
);

CREATE INDEX tasks_by_date     ON tasks (workspace_id, date DESC);
CREATE INDEX tasks_by_project  ON tasks (workspace_id, project_id);
CREATE INDEX tasks_by_invoice  ON tasks (workspace_id, invoice_id) WHERE invoice_id IS NOT NULL;
CREATE INDEX projects_by_client ON projects (workspace_id, client_id);
CREATE INDEX invoices_by_client ON invoices (workspace_id, client_id);
CREATE INDEX expenses_by_client ON expenses (workspace_id, client_id);

-- Supabase exposes the public schema over its own REST API. Turning on row-level security
-- with no policies keeps these tables private to the TaskX API (which connects as the owner).
ALTER TABLE workspaces ENABLE ROW LEVEL SECURITY;
ALTER TABLE clients    ENABLE ROW LEVEL SECURITY;
ALTER TABLE projects   ENABLE ROW LEVEL SECURITY;
ALTER TABLE invoices   ENABLE ROW LEVEL SECURITY;
ALTER TABLE tasks      ENABLE ROW LEVEL SECURITY;
ALTER TABLE expenses   ENABLE ROW LEVEL SECURITY;
