# TaskX — backend

The REST API for TaskX, written in Rust with Axum, SQLx and PostgreSQL. It stores clients, projects, tasks, invoices and expenses, enforces the billing rules, and saves timer screenshots.

The frontend lives in a separate repo (`taskx-frontend`).

## Run it

You need Rust (install it with https://rustup.rs) and a Postgres database.

```bash
cp .env.example .env      # then set DATABASE_URL
cargo run                 # http://127.0.0.1:8080, tables are created on first start
```

Then point the frontend at it. In `taskx-frontend/.env`:

```
VITE_API_URL=http://localhost:8080
```

To move what you already have in the browser to the server:

1. Before setting `VITE_API_URL`, go to **Settings → Export backup**.
2. Set `VITE_API_URL` and restart the frontend.
3. Go to **Settings → Import backup** and choose the file you exported.

### Use Supabase as the database

1. In Supabase, open **Project Settings → Database → Connection string**.
2. Pick **Session pooler**, which uses port 5432. Don't use the transaction pooler on port 6543: it breaks the prepared statements SQLx uses.
3. Put the connection string in `.env` and add `sslmode=require` to it:
   ```
   DATABASE_URL=postgres://postgres.<project-ref>:<password>@aws-0-<region>.pooler.supabase.com:5432/postgres?sslmode=require
   ```
4. Run `cargo run`. The migrations in `migrations/` create the tables.

Row-level security is turned on for every table with no policies. That keeps the data away from Supabase's public REST API, so only this server, which connects as the owner, can read it.

### Local Postgres instead

On macOS with Homebrew:

```bash
brew install postgresql@16
brew services start postgresql@16
createdb taskx
```

Then set `DATABASE_URL=postgres://<your-mac-username>@localhost:5432/taskx`.

## Configuration (`.env`)

| Variable | Default | What it does |
|---|---|---|
| `DATABASE_URL` | required | Postgres connection string |
| `HOST` / `PORT` | `127.0.0.1` / `8080` | Where the API listens |
| `CORS_ORIGINS` | `http://localhost:5173,http://127.0.0.1:5173` | Browser origins allowed to call the API |
| `WORKSPACE_ID` | `default` | All data is stored under this workspace until sign-in exists |
| `SCREENSHOT_DIR` | `./storage/screenshots` | Where timer screenshots are written (git-ignored) |
| `RUST_LOG` | `taskx_backend=info,tower_http=info` | Log level |

## API

Every body is JSON in the frontend's shapes, which are defined in `taskx-frontend/src/types/domain.ts`: camelCase fields, dates as `YYYY-MM-DD`, months as `YYYY-MM`. Errors come back as `{ "error": "message" }`, written so the app can show them to a person as is.

| Method | Path | |
|---|---|---|
| GET | `/health` | `{ "ok": true }` when the database answers |
| GET | `/api/bootstrap` | The whole workspace: `{ workspace, clients, projects, tasks, invoices, expenses }` |
| POST | `/api/import` | Replace everything with a backup, in one transaction |
| GET / PUT | `/api/workspace` | Read or replace the business settings |
| GET | `/api/{collection}` | List one collection |
| GET / PUT / PATCH / DELETE | `/api/{collection}/{id}` | Read, create-or-replace, merge fields, or delete one record |
| POST | `/api/screenshots?user=&client=&project=&takenAt=` | Save one timer screenshot (`image/jpeg`, `png` or `webp` body, 8 MB max) |

`collection` is one of `clients`, `projects`, `tasks`, `invoices` or `expenses`.

- `PATCH` works like `{ ...current, ...fields }` in the app.
- `DELETE` on something that doesn't exist returns `204`.

### Rules the server enforces

- **Tasks**
  - Time is a positive multiple of 15 minutes, 24 hours at most.
  - A task has an `invoiceId` if and only if its status is `invoiced`.
- **Locked months** (`workspace.lockedThrough`): tasks in them can't be created, edited or deleted. They can still be approved, waived, invoiced and released.
- **Invoiced tasks**: can't be edited or deleted. Void the invoice first.
- **Invoices**
  - Once an invoice is sent, its lines, discount, tax, number and client are frozen. Only its status, payment and credit notes can change.
  - A void invoice stays void.
  - Only drafts can be deleted, and only after their tasks and expenses are released.
- **Clients and projects** that have logged time can't be deleted. Make them inactive or archive them instead.
- **Sample records** (`sample: true`) can always be cleared.

The database enforces the same rules again with foreign keys and checks.

### Screenshots

Files are saved as:

```
<SCREENSHOT_DIR>/<workspace>/<user>/clients/<client>/<project>/<YYYY-MM>/week-<n>/<YYYY-MM-DD_HH-MM-SS>.jpg
<SCREENSHOT_DIR>/<workspace>/<user>/internal/<project>/…
```

- `takenAt` is the user's local time, so the folders follow their day.
- Weeks run Monday to Sunday, and week 1 is the week that holds the 1st of the month.
- Names are cleaned so nothing can escape the folder.

Storage sits behind the `ScreenshotStore` trait in `src/screenshots.rs`. Moving to Google Drive means adding a Drive store next to `DiskStore`, with no frontend change.

## Tests

```bash
cargo test                                                         # unit tests
TEST_DATABASE_URL=postgres://localhost/taskx_test cargo test      # + API tests on a real database
```

The API tests cover these flows end to end:

- Task rules
- Locked months
- The invoice lifecycle: create, link tasks, send, credit, void
- Delete protection
- Import
- Screenshot folders
- CORS

Each test uses its own workspace.

## Structure

```
migrations/0001_init.sql   Tables, keys, checks, indexes, row-level security
src/main.rs                Config, database pool, migrations, server
src/lib.rs                 Router + middleware (CORS, body limits, tracing)
src/routes.rs              Handlers; `Ws` extractor = the workspace for a request
src/db.rs                  `Entity` trait: SQL and business rules per table
src/models.rs              API shapes and validation
src/screenshots.rs         Screenshot paths and storage
src/error.rs               Error → HTTP status + readable message
tests/api.rs               API tests
```

## Next

- **Sign-in:** Supabase Auth. Verify the JWT in the `Ws` extractor and map the user to their workspace. That's the only place that changes.
- **Email from the server:** send invoices with a PDF attached.
- **Stripe** payment links on invoices.
- **Google Drive** storage for screenshots.
