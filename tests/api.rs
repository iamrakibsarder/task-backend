//! End-to-end API tests against a real Postgres.
//! Run with: TEST_DATABASE_URL=postgres://… cargo test
//! Each test uses its own workspace id, so tests don't see each other's data.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use taskx_backend::{
    app,
    auth::LoginLimiter,
    connect,
    mail::{token_after, MemoryMailer},
    screenshots::DiskStore,
    AppState, MIGRATOR,
};
use tower::ServiceExt;

struct T {
    app: Router,
    shots: std::path::PathBuf,
    token: Option<String>,
    mail: Arc<MemoryMailer>,
}

async fn setup(name: &str) -> Option<T> {
    setup_with(name, false).await
}

/// `auth`: accounts on, like production. Off: single-business mode on a per-test workspace.
async fn setup_with(name: &str, auth: bool) -> Option<T> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set; skipping {name}");
        return None;
    };
    let db = connect(&url).await.expect("connect");
    MIGRATOR.run(&db).await.expect("migrate");
    let ws = format!("test-{name}-{}", std::process::id());
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(&ws)
        .execute(&db)
        .await
        .unwrap();
    let shots = std::env::temp_dir().join(format!("taskx-shots-{ws}"));
    let _ = std::fs::remove_dir_all(&shots);
    let mail = Arc::new(MemoryMailer::default());
    let state = AppState {
        mailer: mail.clone(),
        app_url: "http://app.test".into(),
        db,
        workspace_id: ws,
        auth_required: auth,
        limiter: Arc::new(LoginLimiter::new(3, std::time::Duration::from_secs(60))),
        screenshots: Arc::new(DiskStore {
            root: shots.clone(),
        }),
    };
    Some(T {
        app: app(state, &["http://localhost:5173".into()]),
        shots,
        token: None,
        mail,
    })
}

impl T {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(tok) = &self.token {
            req = req.header("authorization", format!("Bearer {tok}"));
        }
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
            .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    /// A GET that returns the raw body and content type (for files).
    async fn get_raw(&self, path: &str) -> (StatusCode, String, Vec<u8>) {
        let mut req = Request::builder().method(Method::GET).uri(path);
        if let Some(tok) = &self.token {
            req = req.header("authorization", format!("Bearer {tok}"));
        }
        let res = self
            .app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let ct = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (
            status,
            ct,
            res.into_body().collect().await.unwrap().to_bytes().to_vec(),
        )
    }
    async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::PUT, path, Some(body)).await
    }
    async fn patch(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::PATCH, path, Some(body)).await
    }
    /// The newest email to `to`, waiting briefly because mail is sent in the background.
    async fn email_to(&self, to: &str) -> Option<taskx_backend::mail::Email> {
        for _ in 0..50 {
            if let Some(e) = self.mail.last_to(to) {
                return Some(e);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    }
    async fn signup(
        &mut self,
        email: &str,
        name: &str,
        invite: Option<&str>,
    ) -> (StatusCode, Value) {
        self.token = None;
        let (s, v) = self
            .post(
                "/api/auth/signup",
                json!({"email": email, "password": "long enough", "name": name, "invite": invite}),
            )
            .await;
        if s == StatusCode::OK {
            self.token = Some(v["token"].as_str().unwrap().to_string());
        }
        (s, v)
    }
    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, path, Some(body)).await
    }
    async fn del(&self, path: &str) -> StatusCode {
        self.call(Method::DELETE, path, None).await.0
    }
    async fn seed(&self) {
        assert_eq!(
            self.put("/api/clients/c1", client()).await.0,
            StatusCode::OK
        );
        assert_eq!(self.put("/api/projects/p1", json!({"id":"p1","clientId":"c1","name":"Website","rate":null,"internal":false,"archived":false,"budgetHours":10})).await.0, StatusCode::OK);
        assert_eq!(
            self.put("/api/tasks/t1", task("t1", "2026-10-05", 90))
                .await
                .0,
            StatusCode::OK
        );
    }
}

fn client() -> Value {
    json!({"id":"c1","name":"Northwind","color":"#ff8a3d","contact":"","email":"a@b.co","address":"","billing":"hourly","rate":65,
           "retainerFee":0,"includedHours":0,"cap":0,"active":true,"createdAt":1})
}
fn task(id: &str, date: &str, minutes: i32) -> Value {
    json!({"id":id,"date":date,"projectId":"p1","memo":"Homepage","link":"","minutes":minutes,"status":"review","by":"Alex","invoiceId":null,"createdAt":2})
}
fn invoice() -> Value {
    json!({"id":"i1","seq":1,"number":"INV-0001","clientId":"c1","client":{"name":"Northwind","contact":"","email":"a@b.co","address":""},
           "period":"2026-10","issueDate":"2026-11-01","dueDate":"2026-11-15","currency":"USD",
           "lines":[{"kind":"hours","desc":"Website","detail":"1:30 h","minutes":90,"rate":65,"amount":97.5,"taskIds":["t1"]}],
           "discount":0,"taxRate":0,"taxLabel":"Tax","notes":"","status":"draft","createdAt":3})
}

#[tokio::test]
async fn bootstrap_creates_a_workspace_with_defaults() {
    let Some(t) = setup("boot").await else { return };
    let (s, v) = t.call(Method::GET, "/api/bootstrap", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["workspace"]["currency"], "USD");
    assert_eq!(v["workspace"]["screenshotMinutes"], 5);
    assert_eq!(v["workspace"]["lockedThrough"], "");
    assert_eq!(v["tasks"], json!([]));
}

#[tokio::test]
async fn tasks_round_trip_in_the_app_shape() {
    let Some(t) = setup("roundtrip").await else {
        return;
    };
    t.seed().await;
    let (_, v) = t.call(Method::GET, "/api/tasks/t1", None).await;
    assert_eq!(v["projectId"], "p1");
    assert_eq!(v["by"], "Alex");
    assert_eq!(v["minutes"], 90);
    assert_eq!(v["date"], "2026-10-05");
    assert!(
        v.get("sample").is_none(),
        "false flags are left out like in the app"
    );
    let (_, c) = t.call(Method::GET, "/api/clients/c1", None).await;
    assert_eq!(c["rate"].as_f64(), Some(65.0));
    assert_eq!(c["logo"], "");
    let logo = "data:image/png;base64,iVBORw0KGgo=";
    let (s, c) = t.patch("/api/clients/c1", json!({ "logo": logo })).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(c["logo"], logo);
    assert_eq!(
        t.patch("/api/clients/c1", json!({ "logo": "javascript:alert(1)" }))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn rejects_time_that_isnt_in_15_minute_steps() {
    let Some(t) = setup("minutes").await else {
        return;
    };
    t.seed().await;
    let (s, v) = t.put("/api/tasks/t2", task("t2", "2026-10-06", 20)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(v["error"].as_str().unwrap().contains("15"));
    assert_eq!(
        t.put("/api/tasks/t3", task("t3", "2026-10-06", 0)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        t.put("/api/tasks/other", task("t4", "2026-10-06", 15))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "id must match URL"
    );
}

#[tokio::test]
async fn patch_merges_fields() {
    let Some(t) = setup("patch").await else {
        return;
    };
    t.seed().await;
    let (s, v) = t
        .patch(
            "/api/tasks/t1",
            json!({"status":"waived","waiveReason":"Goodwill"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "waived");
    assert_eq!(v["memo"], "Homepage");
    assert_eq!(
        t.patch("/api/tasks/nope", json!({"memo":"x"})).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn locked_months_allow_approval_but_not_edits() {
    let Some(t) = setup("lock").await else { return };
    t.seed().await;
    let (_, mut w) = t.call(Method::GET, "/api/workspace", None).await;
    w["lockedThrough"] = json!("2026-10");
    assert_eq!(t.put("/api/workspace", w).await.0, StatusCode::OK);
    assert_eq!(
        t.patch("/api/tasks/t1", json!({"memo":"Changed"})).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        t.put("/api/tasks/t9", task("t9", "2026-10-20", 30)).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        t.patch("/api/tasks/t1", json!({"status":"billable"}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(t.del("/api/tasks/t1").await, StatusCode::CONFLICT);
    assert_eq!(
        t.put("/api/tasks/t10", task("t10", "2026-11-02", 30))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn invoice_flow_create_link_void() {
    let Some(t) = setup("invoice").await else {
        return;
    };
    t.seed().await;
    t.patch("/api/tasks/t1", json!({"status":"billable"})).await;
    assert_eq!(t.put("/api/invoices/i1", invoice()).await.0, StatusCode::OK);
    assert_eq!(
        t.patch(
            "/api/tasks/t1",
            json!({"status":"invoiced","invoiceId":"i1"})
        )
        .await
        .0,
        StatusCode::OK
    );

    // Invoiced work is protected.
    assert_eq!(
        t.patch("/api/tasks/t1", json!({"minutes":120})).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(t.del("/api/tasks/t1").await, StatusCode::CONFLICT);
    assert_eq!(
        t.del("/api/invoices/i1").await,
        StatusCode::CONFLICT,
        "still has tasks"
    );

    // Sending freezes the money but allows status, payment and credits.
    assert_eq!(
        t.patch(
            "/api/invoices/i1",
            json!({"status":"sent","sentAt":"2026-11-01","from":{"businessName":"Studio"}})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        t.patch("/api/invoices/i1", json!({"discount":10})).await.0,
        StatusCode::CONFLICT
    );
    let (s, v) = t.patch("/api/invoices/i1", json!({"credits":[{"id":"cn","number":"CN-0001","date":"2026-11-03","amount":20,"reason":"x"}]})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["from"]["businessName"], "Studio");
    assert_eq!(v["credits"][0]["amount"].as_f64(), Some(20.0));
    assert_eq!(
        t.del("/api/invoices/i1").await,
        StatusCode::CONFLICT,
        "only drafts can be deleted"
    );

    // Void: release the task, mark void, can't reopen.
    assert_eq!(
        t.patch(
            "/api/tasks/t1",
            json!({"status":"billable","invoiceId":null})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        t.patch("/api/invoices/i1", json!({"status":"void"}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        t.patch("/api/invoices/i1", json!({"status":"sent"}))
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn clients_with_time_cant_be_deleted() {
    let Some(t) = setup("delclient").await else {
        return;
    };
    t.seed().await;
    let (s, v) = t.call(Method::DELETE, "/api/clients/c1", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v["error"].as_str().unwrap().contains("inactive"));
    assert_eq!(t.del("/api/projects/p1").await, StatusCode::CONFLICT);
    // Deleting what isn't there is fine.
    assert_eq!(t.del("/api/expenses/ghost").await, StatusCode::NO_CONTENT);
    assert_eq!(
        t.call(Method::GET, "/api/widgets", None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn import_replaces_everything() {
    let Some(t) = setup("import").await else {
        return;
    };
    t.seed().await;
    let (_, mut data) = t.call(Method::GET, "/api/bootstrap", None).await;
    data["workspace"]["businessName"] = json!("Imported Co");
    data["tasks"] = json!([task("n1", "2026-11-03", 45), task("n2", "2026-11-04", 60)]);
    let (s, v) = t.call(Method::POST, "/api/import", Some(data)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["workspace"]["businessName"], "Imported Co");
    let ids: Vec<_> = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, ["n2", "n1"]);
}

#[tokio::test]
async fn screenshots_are_saved_in_the_folder_layout() {
    let Some(t) = setup("shots").await else {
        return;
    };
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/screenshots?user=Alex&client=Northwind%20Bakery&project=Website&takenAt=2026-11-06T02:15:09")
        .header("content-type", "image/jpeg")
        .body(Body::from(vec![0xFF, 0xD8, 0xFF, 0xD9]))
        .unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let rel = "Alex/clients/Northwind Bakery/Website/2026-11/week-2/2026-11-06_02-15-09.jpg";
    let ws_dir = std::fs::read_dir(&t.shots)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read(ws_dir.join(rel)).unwrap(),
        vec![0xFF, 0xD8, 0xFF, 0xD9]
    );

    let bad = Request::builder()
        .method(Method::POST)
        .uri("/api/screenshots?takenAt=2026-11-06T02:15:09")
        .header("content-type", "text/plain")
        .body(Body::from("x"))
        .unwrap();
    assert_eq!(
        t.app.clone().oneshot(bad).await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn cors_allows_the_frontend() {
    let Some(t) = setup("cors").await else { return };
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/tasks/t1")
        .header("origin", "http://localhost:5173")
        .header("access-control-request-method", "PATCH")
        .body(Body::empty())
        .unwrap();
    let res = t.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        res.headers()["access-control-allow-origin"],
        "http://localhost:5173"
    );
    assert_eq!(res.headers()["access-control-allow-credentials"], "true");
}

/* ---------------- Accounts ---------------- */

fn email(tag: &str) -> String {
    format!("{tag}-{}@example.com", std::process::id())
}

#[tokio::test]
async fn accounts_sign_up_sign_in_and_stay_separate() {
    let Some(mut t) = setup_with("accounts", true).await else {
        return;
    };
    // Nothing works without signing in.
    assert_eq!(
        t.call(Method::GET, "/api/bootstrap", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(t.call(Method::GET, "/health", None).await.0, StatusCode::OK);

    let (a_mail, b_mail) = (email("alex"), email("blair"));
    for m in [&a_mail, &b_mail] {
        sqlx_cleanup(m).await;
    }
    let (s, v) = t
        .post("/api/auth/signup", json!({"email": a_mail.to_uppercase(), "password": "correct horse", "name": "Alex", "businessName": "Alex Studio"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["user"]["email"], a_mail, "emails are stored lowercase");
    let a_token = v["token"].as_str().unwrap().to_string();
    assert_eq!(
        t.post(
            "/api/auth/signup",
            json!({"email": a_mail, "password": "another one"})
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "one account per email"
    );
    assert_eq!(
        t.post(
            "/api/auth/signup",
            json!({"email": "x@y.co", "password": "short"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    t.token = Some(a_token.clone());
    let (s, me) = t.call(Method::GET, "/api/auth/me", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["authRequired"], true);
    assert_eq!(me["user"]["name"], "Alex");
    t.seed().await;

    // A second account gets its own empty workspace.
    t.token = None;
    let (_, v) = t
        .post(
            "/api/auth/signup",
            json!({"email": b_mail, "password": "battery staple", "name": "Blair"}),
        )
        .await;
    t.token = Some(v["token"].as_str().unwrap().to_string());
    let (_, data) = t.call(Method::GET, "/api/bootstrap", None).await;
    assert_eq!(data["tasks"], json!([]), "Blair can't see Alex's tasks");
    assert_eq!(
        t.call(Method::GET, "/api/tasks/t1", None).await.0,
        StatusCode::NOT_FOUND
    );

    // Sign in again as Alex and see the data.
    t.token = None;
    let (s, v) = t
        .post(
            "/api/auth/login",
            json!({"email": a_mail, "password": "correct horse"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    t.token = Some(v["token"].as_str().unwrap().to_string());
    let (_, data) = t.call(Method::GET, "/api/bootstrap", None).await;
    assert_eq!(data["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(data["workspace"]["ownerName"], "Alex");

    // Sign out ends that session only.
    assert_eq!(
        t.call(Method::POST, "/api/auth/logout", None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        t.call(Method::GET, "/api/bootstrap", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    t.token = Some(a_token);
    assert_eq!(
        t.call(Method::GET, "/api/bootstrap", None).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn wrong_passwords_are_limited_and_passwords_can_change() {
    let Some(mut t) = setup_with("passwords", true).await else {
        return;
    };
    let m = email("casey");
    sqlx_cleanup(&m).await;
    let (_, v) = t
        .post(
            "/api/auth/signup",
            json!({"email": m, "password": "first password"}),
        )
        .await;
    let first = v["token"].as_str().unwrap().to_string();

    // Second session, then change the password from the first: the second is signed out.
    let (_, v) = t
        .post(
            "/api/auth/login",
            json!({"email": m, "password": "first password"}),
        )
        .await;
    let second = v["token"].as_str().unwrap().to_string();
    t.token = Some(first.clone());
    assert_eq!(
        t.post(
            "/api/auth/password",
            json!({"current": "nope nope", "next": "second password"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        t.post(
            "/api/auth/password",
            json!({"current": "first password", "next": "second password"})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        t.call(Method::GET, "/api/auth/me", None).await.0,
        StatusCode::OK,
        "this session stays"
    );
    t.token = Some(second);
    assert_eq!(
        t.call(Method::GET, "/api/auth/me", None).await.0,
        StatusCode::UNAUTHORIZED
    );

    // Old password fails; after 3 failures even the right one is refused for a while.
    t.token = None;
    for _ in 0..3 {
        assert_eq!(
            t.post(
                "/api/auth/login",
                json!({"email": m, "password": "first password"})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        t.post(
            "/api/auth/login",
            json!({"email": m, "password": "second password"})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Unknown emails get the same answer as wrong passwords.
    let (s, v) = t
        .post(
            "/api/auth/login",
            json!({"email": email("nobody"), "password": "whatever1"}),
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], "Wrong email or password.");
}

#[tokio::test]
async fn accounts_off_reports_it() {
    let Some(t) = setup("authoff").await else {
        return;
    };
    let (s, v) = t.call(Method::GET, "/api/auth/me", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["authRequired"], false);
    assert_eq!(
        t.post(
            "/api/auth/login",
            json!({"email": "a@b.co", "password": "whatever1"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

async fn sqlx_cleanup(email: &str) {
    let db = connect(&std::env::var("TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    // Removing the workspace cascades to the user, sessions and activity.
    sqlx::query("DELETE FROM workspaces WHERE id IN (SELECT workspace_id FROM users WHERE email = $1) AND id NOT LIKE 'test-%'")
        .bind(email)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE email = $1")
        .bind(email)
        .execute(&db)
        .await
        .unwrap();
}

/* ---------------- Payments & activity ---------------- */

#[tokio::test]
async fn partial_payments_add_up_to_the_total() {
    let Some(t) = setup("payments").await else {
        return;
    };
    t.seed().await;
    t.patch("/api/tasks/t1", json!({"status":"billable"})).await;
    let mut inv = invoice();
    inv["payments"] = json!([{"id":"p1","date":"2026-11-02","amount":10}]);
    assert_eq!(
        t.put("/api/invoices/i1", inv).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "drafts can't take payments"
    );
    assert_eq!(t.put("/api/invoices/i1", invoice()).await.0, StatusCode::OK);
    t.patch(
        "/api/invoices/i1",
        json!({"status":"sent","sentAt":"2026-11-01"}),
    )
    .await;

    let pay = |amount: f64| json!({"id": format!("p{amount}"), "date":"2026-11-05", "amount": amount, "method":"Bank transfer"});
    let (s, v) = t
        .patch("/api/invoices/i1", json!({"payments":[pay(40.0)]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["payments"][0]["method"], "Bank transfer");
    let (s, v) = t
        .patch(
            "/api/invoices/i1",
            json!({"payments":[pay(40.0), pay(60.0)]}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "97.50 total, 100 paid");
    assert!(v["error"].as_str().unwrap().contains("97.50"));
    assert_eq!(
        t.patch(
            "/api/invoices/i1",
            json!({"payments":[pay(40.0), pay(57.5)], "status":"paid", "paidAt":"2026-11-06"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        t.patch("/api/invoices/i1", json!({"payments":[pay(-1.0)]}))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn changes_are_recorded_in_the_activity_log() {
    let Some(t) = setup("activity").await else {
        return;
    };
    t.seed().await;
    t.patch("/api/tasks/t1", json!({"status":"billable"})).await;
    t.del("/api/expenses/ghost").await; // not there: nothing logged
    let (s, v) = t.call(Method::GET, "/api/activity?limit=10", None).await;
    assert_eq!(s, StatusCode::OK);
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 4, "client, project, task, task update: {v}");
    assert_eq!(rows[0]["action"], "update");
    assert_eq!(rows[0]["entity"], "task");
    assert!(rows[0]["summary"]
        .as_str()
        .unwrap()
        .contains("review → billable"));
    assert_eq!(rows[3]["summary"], "Client Northwind");

    let before = rows[1]["id"].as_i64().unwrap();
    let (_, older) = t
        .call(Method::GET, &format!("/api/activity?before={before}"), None)
        .await;
    assert_eq!(older.as_array().unwrap().len(), 2);
}

/* ---------------- Teams ---------------- */

#[tokio::test]
async fn teams_invite_roles_and_removal() {
    let Some(mut t) = setup_with("teams", true).await else {
        return;
    };
    let (owner, mate, other) = (email("t-owner"), email("t-mate"), email("t-other"));
    for m in [&owner, &mate, &other] {
        sqlx_cleanup(m).await;
    }
    t.signup(&owner, "Olive", None).await;
    let owner_tok = t.token.clone().unwrap();
    let (_, me) = t.call(Method::GET, "/api/auth/me", None).await;
    assert_eq!(me["user"]["role"], "owner");
    let owner_id = me["user"]["id"].as_str().unwrap().to_string();
    t.seed().await;

    // Invite: link returned to the sender and emailed to the invitee.
    let (s, v) = t
        .post(
            "/api/team/invites",
            json!({"email": mate.to_uppercase(), "role": "member"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let link = v["link"].as_str().unwrap().to_string();
    assert!(link.starts_with("http://app.test/invite/"));
    let mail = t.email_to(&mate).await.expect("invite email");
    assert!(mail.body.contains(&link));
    let token = link.rsplit('/').next().unwrap().to_string();
    assert_eq!(
        t.post(
            "/api/team/invites",
            json!({"email": owner, "role": "member"})
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "already a member"
    );
    assert_eq!(
        t.post(
            "/api/team/invites",
            json!({"email": "x@y.co", "role": "owner"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "owners aren't invited"
    );

    // Anyone with the link can see what it's for.
    t.token = None;
    let (s, info) = t
        .call(Method::GET, &format!("/api/invites/{token}"), None)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(info["email"], mate);
    assert_eq!(info["invitedBy"], "Olive");
    assert_eq!(info["hasAccount"], false);

    // Signing up with the wrong email doesn't use the invite.
    assert_eq!(
        t.signup(&other, "Oscar", Some(&token)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // The right one joins the team directly.
    let (s, _) = t.signup(&mate, "Max", Some(&token)).await;
    assert_eq!(s, StatusCode::OK);
    let mate_tok = t.token.clone().unwrap();
    let (_, me) = t.call(Method::GET, "/api/auth/me", None).await;
    assert_eq!(me["user"]["role"], "member");
    let mate_id = me["user"]["id"].as_str().unwrap().to_string();
    let (_, data) = t.call(Method::GET, "/api/bootstrap", None).await;
    assert_eq!(
        data["tasks"].as_array().unwrap().len(),
        1,
        "sees the team's tasks"
    );
    assert_eq!(
        t.call(Method::GET, &format!("/api/invites/{token}"), None)
            .await
            .0,
        StatusCode::CONFLICT,
        "used"
    );

    // Members work with data but can't change settings or invite.
    assert_eq!(
        t.put("/api/tasks/m1", task("m1", "2026-11-03", 30)).await.0,
        StatusCode::OK
    );
    let (_, w) = t.call(Method::GET, "/api/workspace", None).await;
    assert_eq!(t.put("/api/workspace", w).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        t.post("/api/team/invites", json!({"email": other})).await.0,
        StatusCode::FORBIDDEN
    );
    let (_, act) = t.call(Method::GET, "/api/activity?limit=1", None).await;
    assert_eq!(act[0]["actor"], "Max", "the log says who did it");

    // The owner promotes them; now they can invite.
    t.token = Some(owner_tok.clone());
    assert_eq!(
        t.patch(
            &format!("/api/team/members/{mate_id}"),
            json!({"role": "admin"})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    t.token = Some(mate_tok.clone());
    let (s, v) = t
        .post(
            "/api/team/invites",
            json!({"email": other, "role": "member"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let other_token = v["link"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(
        t.patch(
            &format!("/api/team/members/{owner_id}"),
            json!({"role": "member"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "only owners change roles"
    );
    assert_eq!(
        t.del(&format!("/api/team/members/{owner_id}")).await,
        StatusCode::FORBIDDEN
    );

    // Someone with their own account accepts; they now belong to two workspaces.
    t.signup(&other, "Oscar", None).await;
    let other_tok = t.token.clone().unwrap();
    let (_, mine) = t.call(Method::GET, "/api/auth/me", None).await;
    let own_ws = mine["user"]["workspaceId"].as_str().unwrap().to_string();
    t.token = Some(mate_tok.clone());
    assert_eq!(
        t.call(
            Method::POST,
            &format!("/api/invites/{other_token}/accept"),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "invite is for someone else"
    );
    t.token = Some(other_tok.clone());
    assert_eq!(
        t.call(
            Method::POST,
            &format!("/api/invites/{other_token}/accept"),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, list) = t.call(Method::GET, "/api/workspaces", None).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    let (_, team) = t.call(Method::GET, "/api/team", None).await;
    assert_eq!(team["members"].as_array().unwrap().len(), 3);
    assert_eq!(team["invites"], json!([]), "members don't see invites");
    assert_eq!(
        t.post("/api/workspaces/switch", json!({"workspaceId": own_ws}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (_, data) = t.call(Method::GET, "/api/bootstrap", None).await;
    assert_eq!(data["tasks"], json!([]));
    assert_eq!(
        t.post("/api/workspaces/switch", json!({"workspaceId": "w-nope"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // A workspace always keeps an owner.
    t.token = Some(owner_tok.clone());
    assert_eq!(
        t.patch(
            &format!("/api/team/members/{owner_id}"),
            json!({"role": "admin"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        t.del(&format!("/api/team/members/{owner_id}")).await,
        StatusCode::CONFLICT
    );

    // Removed: their next request lands in a fresh workspace of their own.
    assert_eq!(
        t.del(&format!("/api/team/members/{mate_id}")).await,
        StatusCode::NO_CONTENT
    );
    t.token = Some(mate_tok);
    let (s, me) = t.call(Method::GET, "/api/auth/me", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["user"]["role"], "owner");
    let (_, data) = t.call(Method::GET, "/api/bootstrap", None).await;
    assert_eq!(data["tasks"], json!([]), "no access to the old team's data");
}

#[tokio::test]
async fn password_reset_by_email_link() {
    let Some(mut t) = setup_with("reset", true).await else {
        return;
    };
    let m = email("forgetful");
    sqlx_cleanup(&m).await;
    t.signup(&m, "Fern", None).await;
    let old = t.token.clone();
    t.token = None;

    // Unknown emails get the same answer and nothing is sent.
    let nobody = email("nobody-reset");
    assert_eq!(
        t.post("/api/auth/forgot", json!({"email": nobody})).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        t.post("/api/auth/forgot", json!({"email": m.to_uppercase()}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let mail = t.email_to(&m).await.expect("reset email");
    assert!(t.mail.last_to(&nobody).is_none());
    assert!(mail.body.contains("http://app.test/reset-password?token="));
    let token = token_after(&mail.body, "token=").unwrap();

    assert_eq!(
        t.post(
            "/api/auth/reset",
            json!({"token": token, "password": "short"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (s, v) = t
        .post(
            "/api/auth/reset",
            json!({"token": token, "password": "brand new secret"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["token"].is_string(), "signed in straight away");
    assert_eq!(
        t.post(
            "/api/auth/reset",
            json!({"token": token, "password": "another secret"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "links work once"
    );

    // Old sessions are gone; the new password works, the old one doesn't.
    t.token = old;
    assert_eq!(
        t.call(Method::GET, "/api/auth/me", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    t.token = None;
    assert_eq!(
        t.post(
            "/api/auth/login",
            json!({"email": m, "password": "long enough"})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        t.post(
            "/api/auth/login",
            json!({"email": m, "password": "brand new secret"})
        )
        .await
        .0,
        StatusCode::OK
    );

    // A newer link replaces an older one.
    t.post("/api/auth/forgot", json!({"email": m})).await;
    let first = token_after(&t.email_to(&m).await.unwrap().body, "token=").unwrap();
    t.mail.sent.lock().unwrap().clear();
    t.post("/api/auth/forgot", json!({"email": m})).await;
    let second = token_after(&t.email_to(&m).await.unwrap().body, "token=").unwrap();
    assert_ne!(first, second);
    assert_eq!(
        t.post(
            "/api/auth/reset",
            json!({"token": first, "password": "third secret!"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

/* ---------------- Invoice PDF & email ---------------- */

#[tokio::test]
async fn invoices_download_as_pdf_and_send_by_email() {
    let Some(t) = setup("invoice-mail").await else {
        return;
    };
    t.seed().await;
    let (_, mut w) = t.call(Method::GET, "/api/workspace", None).await;
    w["businessName"] = json!("Northwind Studio");
    w["email"] = json!("hello@northwind.test");
    w["paymentInfo"] = json!("IBAN DE00 1234");
    assert_eq!(t.put("/api/workspace", w).await.0, StatusCode::OK);
    assert_eq!(t.put("/api/invoices/i1", invoice()).await.0, StatusCode::OK);

    // PDF download.
    let (s, ct, pdf) = t.get_raw("/api/invoices/i1/pdf").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ct, "application/pdf");
    assert!(pdf.starts_with(b"%PDF-1.4"));
    if let Ok(dir) = std::env::var("PDF_OUT") {
        std::fs::write(format!("{dir}/invoice.pdf"), &pdf).unwrap();
    }
    assert_eq!(
        t.get_raw("/api/invoices/nope/pdf").await.0,
        StatusCode::NOT_FOUND
    );

    // Bad input is refused and nothing is sent.
    assert_eq!(
        t.post("/api/invoices/i1/send", json!({"to": "not an email"}))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        t.post(
            "/api/invoices/i1/send",
            json!({"to": "x@y.co", "subject": "two\nlines"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(t.mail.sent.lock().unwrap().is_empty());

    // Send: the draft becomes sent, with the PDF attached and replies going to the business.
    let (s, v) = t
        .post(
            "/api/invoices/i1/send",
            json!({"to": "Billing@Client.test", "subject": "Your invoice", "body": "Hi there", "sentAt": "2026-11-02"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "sent");
    assert_eq!(v["sentAt"], "2026-11-02");
    assert_eq!(v["client"]["email"], "billing@client.test");
    assert_eq!(v["from"]["businessName"], "Northwind Studio");
    let mail = t.email_to("billing@client.test").await.expect("email sent");
    assert_eq!(mail.subject, "Your invoice");
    assert_eq!(mail.body, "Hi there");
    assert_eq!(mail.reply_to.as_deref(), Some("hello@northwind.test"));
    assert_eq!(mail.from_name.as_deref(), Some("Northwind Studio"));
    assert_eq!(mail.attachments.len(), 1);
    assert_eq!(mail.attachments[0].filename, "INV-0001 Northwind.pdf");
    assert!(mail.attachments[0].bytes.starts_with(b"%PDF"));
    let (_, act) = t.call(Method::GET, "/api/activity?limit=1", None).await;
    assert_eq!(
        act[0]["summary"],
        "INV-0001 emailed to billing@client.test (now sent)"
    );

    // Resending without a message uses the default one, and keeps the status.
    t.mail.sent.lock().unwrap().clear();
    let (s, v) = t
        .post(
            "/api/invoices/i1/send",
            json!({"to": "billing@client.test"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "sent");
    let mail = t.email_to("billing@client.test").await.unwrap();
    assert!(
        mail.subject
            .starts_with("Invoice INV-0001 from Northwind Studio"),
        "{}",
        mail.subject
    );
    assert!(mail.body.contains("IBAN DE00 1234"));

    // Void invoices can't be sent.
    let mut voided = v.clone();
    voided["status"] = json!("void");
    assert_eq!(t.put("/api/invoices/i1", voided).await.0, StatusCode::OK);
    assert_eq!(
        t.post(
            "/api/invoices/i1/send",
            json!({"to": "billing@client.test"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}
