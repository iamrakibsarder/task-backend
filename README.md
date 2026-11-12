# TaskX — backend

The REST API for TaskX, written in Rust with Axum, SQLx and PostgreSQL. It handles accounts and sign-in, stores clients, projects, tasks, invoices and expenses, enforces the billing rules, keeps an activity log, and saves timer screenshots.

The frontend is in `../taskx-frontend`.

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

Or run everything with Docker from the repo root: `docker compose up --build`.

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
| `AUTH` | `on` | `on`: people sign up and sign in; each account has its own workspace. `off`: no sign-in, everything goes to `WORKSPACE_ID` (private machines only) |
| `WORKSPACE_ID` | `default` | With accounts off, the one workspace. With accounts on, the first account created adopts it, so earlier data carries over |
| `APP_URL` | `http://localhost:5173` | The web app's address, used in reset and invite links |
| `SMTP_URL` | empty | `smtps://user:pass@host:465` or `smtp://host:25`. Empty: emails are written to the log instead (local use only) |
| `MAIL_FROM` | `TaskX <no-reply@localhost>` | Sender of reset and invite emails |
| `SCREENSHOT_DIR` | `./storage/screenshots` | Where timer screenshots are written (git-ignored) |
| `RUST_LOG` | `taskx_backend=info,tower_http=info` | Log level |

## API

Every body is JSON in the frontend's shapes, which are defined in `taskx-frontend/src/types/domain.ts`: camelCase fields, dates as `YYYY-MM-DD`, months as `YYYY-MM`. Errors come back as `{ "error": "message" }`, written so the app can show them to a person as is.

| Method | Path | |
|---|---|---|
| GET | `/health` | `{ "ok": true, "version": "…" }` when the database answers |
| POST | `/api/auth/signup` | `{ email, password, name?, businessName?, invite? }` → `{ token, user }`. With an invite token, joins that team |
| POST | `/api/auth/login` | `{ email, password }` → `{ token, user }` |
| POST | `/api/auth/logout` | Ends this session |
| GET | `/api/auth/me` | `{ authRequired, user: { …, role, workspaceName }, mailEnabled }` |
| POST | `/api/auth/password` | `{ current, next }`; signs out the account's other sessions |
| POST | `/api/auth/forgot` | `{ email }` → always `204`; emails a 1-hour reset link if the account exists |
| POST | `/api/auth/reset` | `{ token, password }` → `{ token, user }`; signs out every other session |
| GET / POST | `/api/workspaces` | Workspaces you belong to / create one `{ name }` (you own it and switch to it) |
| POST | `/api/workspaces/switch` | `{ workspaceId }` |
| GET | `/api/team` | `{ members, invites, role }` (invites only for owners and admins) |
| POST | `/api/team/invites` | `{ email, role: admin\|member }` → `{ invite, link, emailed }` (admin+) |
| DELETE | `/api/team/invites/{id}` | Cancel an invite (admin+) |
| PATCH | `/api/team/members/{userId}` | `{ role }` (owners only) |
| DELETE | `/api/team/members/{userId}` | Remove someone (admin+), or leave with your own id |
| GET | `/api/invites/{token}` | Public: who invited you, to which workspace, as what |
| POST | `/api/invites/{token}/accept` | Signed in with the invited email: join and switch to it |
| GET | `/api/activity?limit=&before=` | The change log, newest first. `before` = an entry id, for paging |
| GET | `/api/bootstrap` | The whole workspace: `{ workspace, clients, projects, tasks, invoices, expenses }` |
| POST | `/api/import` | Replace everything with a backup, in one transaction |
| GET / PUT | `/api/workspace` | Read or replace the business settings |
| GET | `/api/{collection}` | List one collection |
| GET / PUT / PATCH / DELETE | `/api/{collection}/{id}` | Read, create-or-replace, merge fields, or delete one record |
| POST | `/api/screenshots?user=&client=&project=&takenAt=` | Save one timer screenshot (`image/jpeg`, `png` or `webp` body, 8 MB max) |

`collection` is one of `clients`, `projects`, `tasks`, `invoices` or `expenses`.

With accounts on, every `/api` route except `/api/auth/*` needs `Authorization: Bearer <token>`. A missing or expired token is a `401`.

- `PATCH` works like `{ ...current, ...fields }` in the app.
- `DELETE` on something that doesn't exist returns `204`.

### Accounts

- Passwords: at least 8 characters, hashed with Argon2id.
- Sign-in returns a random token. Only its SHA-256 is stored, in `sessions`.
- Sessions last 30 days from last use.
- After 8 wrong passwords for one email in 15 minutes, sign-in for it is refused (`429`) until the window passes. Unknown emails take as long to check as known ones.

### Teams

- Roles: **owner** (everything, including roles), **admin** (invite and remove members, settings, import), **member** (clients, projects, tasks, invoices, expenses). Others get `403`.
- A workspace always keeps at least one owner; only owners remove owners.
- Invites are for one email, single use, valid 7 days; inviting again replaces the link. The link is returned to the sender too, so it works without email set up.
- Someone removed from the workspace they're in moves to another of theirs on their next request, or gets a new empty one.

### Password reset

- Links are single use, valid one hour, stored hashed; a new request replaces older links.
- The reply is the same whether or not the email has an account, and requests are rate limited per email.

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
- Accounts: sign up, sign in, separate workspaces, sign out, password change, sign-in limits
- Partial payments
- The activity log

Each test uses its own workspace.

## Structure

```
migrations/0001_init.sql   Tables, keys, checks, indexes, row-level security
migrations/0003_…sql       Users, sessions, activity log, invoice payments
migrations/0004_…sql       Memberships, invites, password resets
src/main.rs                Config, database pool, migrations, server
src/auth.rs                Accounts, sessions, password hashing, sign-in limits, reset
src/team.rs                Workspaces, members, roles, invites
src/mail.rs                SMTP / log / in-memory mailers
src/lib.rs                 Router + middleware (CORS, body limits, tracing)
src/routes.rs              Handlers; `Ws` extractor = the signed-in user's workspace
src/db.rs                  `Entity` trait: SQL and business rules per table
src/models.rs              API shapes and validation
src/screenshots.rs         Screenshot paths and storage
src/error.rs               Error → HTTP status + readable message
tests/api.rs               API tests
```

## Next

- **Email from the server:** send invoices with a PDF attached.
- **Stripe** payment links on invoices.
- **Google Drive** storage for screenshots.
