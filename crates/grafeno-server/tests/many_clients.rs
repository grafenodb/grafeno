//! Many clients at once: a write that waits for a busy writer gives its
//! turn back, so reads go on; and the server makes room for a file
//! descriptor per connection.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_waiting_for_the_writer_leave_the_turns_to_reads() {
    // Two turns, no queue: a third request at once would get 503.
    let state = AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None)
        .with_max_requests(2)
        .with_queue(0, Duration::from_millis(1));
    let app = router(state);
    call(&app, "POST", "/v1/w/nodes", Some(json!({"labels": ["N"]}))).await;
    // A long write holds the writer; two more wait behind it.
    let long = {
        let app = app.clone();
        tokio::spawn(async move {
            call(
                &app,
                "POST",
                "/v1/w/query",
                Some(json!({"query": "UNWIND range(1, 300000) AS i CREATE (:Load {i: i})"})),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let waiting: Vec<_> = (0..2)
        .map(|n| {
            let app = app.clone();
            tokio::spawn(async move {
                call(
                    &app,
                    "POST",
                    "/v1/w/nodes",
                    Some(json!({"labels": ["N"], "props": {"n": n}})),
                )
                .await
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Every turn would be held by a waiting write; reads still get one.
    for _ in 0..5 {
        let (status, answer) = call(&app, "GET", "/v1/w/stats", None).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
    }
    assert!(!long.is_finished(), "the long write ended too soon to tell");
    let (status, _) = long.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    for write in waiting {
        assert_eq!(write.await.unwrap().0, StatusCode::CREATED);
    }
}

#[cfg(unix)]
#[test]
fn the_server_makes_room_for_a_descriptor_per_connection() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit read and write the limit given.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    if limit.rlim_max < 4096 {
        return;
    }
    // As a shell or a service manager may leave it: 1024 open files.
    let low = libc::rlimit {
        rlim_cur: 1024,
        rlim_max: limit.rlim_max,
    };
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) };
    // Not under systemd here, whatever started the tests.
    // SAFETY: no other test of this file reads the environment.
    unsafe { std::env::remove_var("INVOCATION_ID") };
    grafeno_server::http::open_files_for_connections();
    let mut now = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut now) };
    assert!(now.rlim_cur >= 4096.min(limit.rlim_max), "{}", now.rlim_cur);
}
