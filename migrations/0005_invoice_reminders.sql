-- Payment reminders for unpaid invoices.

-- Off until the business turns it on. Days are relative to the due date:
-- -3 = three days before, 0 = on the day, 7 = a week late.
ALTER TABLE workspaces ADD COLUMN reminders_on  BOOLEAN   NOT NULL DEFAULT false;
ALTER TABLE workspaces ADD COLUMN reminder_days INTEGER[] NOT NULL DEFAULT '{-3,0,7,14}';

-- Per client: some clients shouldn't get automatic emails.
ALTER TABLE clients ADD COLUMN reminders BOOLEAN NOT NULL DEFAULT true;

-- Every reminder sent. Kept apart from `invoices` so saving an invoice from the app never
-- overwrites the history. `stage` = the day it was scheduled for; NULL = sent by hand.
CREATE TABLE invoice_reminders (
  id            BIGSERIAL PRIMARY KEY,
  workspace_id  TEXT NOT NULL,
  invoice_id    TEXT NOT NULL,
  stage         INTEGER,
  sent_to       TEXT NOT NULL,
  sent_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  actor         TEXT NOT NULL DEFAULT '',
  FOREIGN KEY (workspace_id, invoice_id) REFERENCES invoices (workspace_id, id) ON DELETE CASCADE
);
-- Each scheduled reminder goes out once, even with two servers running the job.
CREATE UNIQUE INDEX invoice_reminders_once ON invoice_reminders (workspace_id, invoice_id, stage)
  WHERE stage IS NOT NULL;
CREATE INDEX invoice_reminders_by_invoice ON invoice_reminders (workspace_id, invoice_id, sent_at DESC);

ALTER TABLE invoice_reminders ENABLE ROW LEVEL SECURITY;
