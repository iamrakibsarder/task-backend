-- Client logo: a small image as a data URL (made by the app, ~10-40 KB) or an https link.
ALTER TABLE clients ADD COLUMN logo TEXT NOT NULL DEFAULT '' CHECK (length(logo) <= 400000);
