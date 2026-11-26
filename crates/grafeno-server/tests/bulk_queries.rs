//! `CALL { … } IN TRANSACTIONS` over HTTP: batches through the writer's
//! bulk lane, with interactive writes between them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batches_leave_room_for_interactive_writes() {
    let app = router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None));
    let load = {
        let app = app.clone();
        tokio::spawn(async move {
            post(
                &app,
                "/v1/bulk/query",
                json!({"query": "UNWIND range(1, 60000) AS i CALL { WITH i CREATE (:Reading {i: i}) } IN TRANSACTIONS OF 2000 ROWS"}),
            )
            .await
        })
    };
    // While it loads, an agent's writes keep going through.
    let mut slowest = Duration::ZERO;
    let mut written = 0;
    while !load.is_finished() {
        let started = Instant::now();
        let (status, made) = post(
            &app,
            "/v1/agent/nodes",
            json!({"labels": ["Memory"], "props": {"n": written}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        slowest = slowest.max(started.elapsed());
        written += 1;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (status, done) = load.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["stats"]["nodes_created"], 60000);
    assert_eq!(done["stats"]["transactions"], 30);
    assert!(written > 0);
    let (_, counted) = post(
        &app,
        "/v1/bulk/query",
        json!({"query": "MATCH (r:Reading) RETURN count(r)"}),
    )
    .await;
    assert_eq!(counted["rows"], json!([[60000]]));
    eprintln!("{written} interactive writes during the load, the slowest {slowest:?}");

    // One transaction per key: refused with several.
    let (status, _) = post(
        &app,
        "/v1/bulk/query",
        json!({"query": "UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS", "idempotency_key": "k1"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn csv_and_graphml_imports_go_through_the_bulk_lane() {
    let state = AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None);
    let writer = state.writer.clone();
    let app = router(state);
    let before = writer.metrics().bulk_latency_seconds.count();
    let mut csv = String::from("id,i:int\n");
    for i in 0..5_000 {
        csv.push_str(&format!("r{i},{i}\n"));
    }
    let request = Request::builder()
        .method("POST")
        .uri("/v1/csv/import/nodes.csv")
        .header(header::CONTENT_TYPE, "text/csv")
        .body(Body::from(csv))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Three chunks of up to 2,000 rows, each a bulk transaction: an
    // import no longer writes around the writer.
    assert_eq!(writer.metrics().bulk_latency_seconds.count() - before, 3);
    let (_, counted) = post(
        &app,
        "/v1/csv/query",
        json!({"query": "MATCH (n) RETURN count(n)"}),
    )
    .await;
    assert_eq!(counted["rows"], json!([[5000]]));
}
