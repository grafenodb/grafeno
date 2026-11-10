//! Explicit transactions over HTTP (16.4): atomic, isolated, ended by
//! commit, rollback, a failed statement or a timeout, one writer at a time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
use grafeno_server::transactions::TransactionSettings;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

fn app_with(settings: TransactionSettings) -> Router {
    router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None).with_transactions(settings))
}

fn app() -> Router {
    app_with(TransactionSettings::default())
}

async fn begin(app: &Router, body: Value) -> String {
    let (status, begun) = call(app, "POST", "/v1/k/transactions", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{begun}");
    begun["id"].as_str().unwrap().to_owned()
}

async fn in_tx(app: &Router, id: &str, query: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/v1/k/transactions/{id}/query"),
        Some(json!({ "query": query })),
    )
    .await
}

async fn count(app: &Router) -> Value {
    let (_, result) = call(
        app,
        "POST",
        "/v1/k/query",
        Some(json!({ "query": "MATCH (n:Note) RETURN count(n)" })),
    )
    .await;
    result["rows"][0][0].clone()
}

#[tokio::test]
async fn statements_commit_together_and_others_see_nothing_before() {
    let app = app();
    let id = begin(&app, json!({})).await;
    let (status, created) = in_tx(&app, &id, "CREATE (:Note {n: 1}), (:Note {n: 2})").await;
    assert_eq!(status, StatusCode::OK, "{created}");
    // The transaction sees its own writes; a reader outside does not.
    let (_, inside) = in_tx(&app, &id, "MATCH (n:Note) RETURN count(n)").await;
    assert_eq!(inside["rows"][0][0], 2);
    // (The read outside runs on a snapshot: it does not wait for the writer.)
    assert_eq!(count(&app).await, 0);
    let (status, info) = call(&app, "GET", &format!("/v1/k/transactions/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["statements"], 2);
    let (status, done) = call(
        &app,
        "POST",
        &format!("/v1/k/transactions/{id}/commit"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(count(&app).await, 2);
    // Ended: gone.
    let (status, _) = in_tx(&app, &id, "RETURN 1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rollback_and_failed_statements_leave_nothing() {
    let app = app();
    let id = begin(&app, json!({})).await;
    in_tx(&app, &id, "CREATE (:Note {n: 1})").await;
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/k/transactions/{id}/rollback"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(count(&app).await, 0);

    // A statement fails after writing part of its work: the transaction
    // takes nothing more, and its commit rolls everything back.
    let id = begin(&app, json!({})).await;
    in_tx(&app, &id, "CREATE (:Note {n: 1})").await;
    let (status, _) = in_tx(
        &app,
        &id,
        "UNWIND [1, 2, 0] AS x CREATE (:Note {n: 10 / x})",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, refused) = in_tx(&app, &id, "CREATE (:Note {n: 3})").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only ROLLBACK"),
        "{refused}"
    );
    let (status, outcome) = call(
        &app,
        "POST",
        &format!("/v1/k/transactions/{id}/commit"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{outcome}");
    assert_eq!(outcome["error"]["code"], "rolled_back");
    assert_eq!(count(&app).await, 0);
}

#[tokio::test]
async fn a_reading_transaction_keeps_its_snapshot() {
    let app = app();
    call(
        &app,
        "POST",
        "/v1/k/query",
        Some(json!({ "query": "CREATE (:Note {n: 1})" })),
    )
    .await;
    let id = begin(&app, json!({ "read_only": true })).await;
    call(
        &app,
        "POST",
        "/v1/k/query",
        Some(json!({ "query": "CREATE (:Note {n: 2})" })),
    )
    .await;
    let (_, seen) = in_tx(&app, &id, "MATCH (n:Note) RETURN count(n)").await;
    assert_eq!(seen["rows"][0][0], 1, "the snapshot of its start");
    let (status, _) = in_tx(&app, &id, "CREATE (:Note {n: 3})").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/k/transactions/{id}/commit"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn one_writer_waits_and_idle_transactions_end() {
    let app = app_with(TransactionSettings {
        idle: Duration::from_millis(400),
        wait: Duration::from_millis(200),
        ..TransactionSettings::default()
    });
    let first = begin(&app, json!({})).await;
    in_tx(&app, &first, "CREATE (:Note {n: 1})").await;
    // A second writing transaction waits, then gives up.
    let (status, busy) = call(&app, "POST", "/v1/k/transactions", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{busy}");
    assert_eq!(busy["error"]["code"], "transaction_busy");
    // A plain write waits for the transaction to end (here, its timeout).
    let started = Instant::now();
    let writer = {
        let app = app.clone();
        tokio::spawn(async move {
            call(
                &app,
                "POST",
                "/v1/k/query",
                Some(json!({ "query": "CREATE (:Note {n: 9})" })),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(600)).await;
    // Past its idle time: rolled back on its next use, and the writer is free.
    let (status, _) = in_tx(&app, &first, "RETURN 1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = writer.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(count(&app).await, 1, "only the plain write");
}

/// A server killed with a transaction open: after a restart, nothing of
/// the transaction is there, and the database checks clean.
#[test]
fn a_crash_before_commit_leaves_nothing() {
    use std::process::{Command, Stdio};
    let dir = std::env::temp_dir().join(format!("grafeno-tx-crash-{}", grafeno::NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("crash.redb");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_grafeno"))
            .args([
                "serve",
                "--db",
                db.to_str().unwrap(),
                "--addr",
                &format!("127.0.0.1:{port}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the server did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let post = |path: &str, body: Value| -> Value {
        let mut response = ureq::post(&format!("{url}{path}")).send_json(body).unwrap();
        response.body_mut().read_json().unwrap()
    };
    post("/v1/k/query", json!({ "query": "CREATE (:Note {n: 1})" }));
    let begun = post("/v1/k/transactions", json!({}));
    let id = begun["id"].as_str().unwrap();
    post(
        &format!("/v1/k/transactions/{id}/query"),
        json!({ "query": "CREATE (:Note {n: 2}), (:Note {n: 3})" }),
    );
    // Killed, not stopped.
    let _ = server.0.kill();
    let _ = server.0.wait();
    drop(server);
    let reopened = Grafeno::open(&db).unwrap();
    let read = reopened.begin_read().unwrap();
    assert_eq!(read.stats("k").unwrap().nodes, 1, "only what was committed");
    assert!(read.check("k").unwrap().is_ok());
    drop((read, reopened));
    let _ = std::fs::remove_dir_all(&dir);
}
