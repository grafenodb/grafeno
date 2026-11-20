//! Fixes from the security review of 0.3.0: each test reproduces a way
//! around authorization the review found, and checks it is closed.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const OWNER: &str = "owner-secret";

async fn call(
    app: &Router,
    key: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {key}"));
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
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
}

async fn ok(app: &Router, method: &str, uri: &str, body: Value) -> Value {
    let (code, answer) = call(app, OWNER, method, uri, Some(body)).await;
    assert!(code.is_success(), "{method} {uri}: {code} {answer}");
    answer
}

fn on_ns(action: &str, ns: &str) -> Value {
    json!({ "action": action, "object": { "on": "namespace", "namespace": ns } })
}

/// A service account holding a role with `actions` on `ns`, and a key of it.
async fn principal(app: &Router, name: &str, ns: &str, actions: &[&str]) -> String {
    let privileges: Vec<Value> = actions.iter().map(|action| on_ns(action, ns)).collect();
    ok(
        app,
        "PUT",
        &format!("/v1/roles/{name}-role"),
        json!({ "privileges": privileges }),
    )
    .await;
    ok(
        app,
        "POST",
        "/v1/accounts",
        json!({ "name": name, "kind": "service", "roles": [format!("{name}-role")] }),
    )
    .await;
    let made = ok(
        app,
        "POST",
        "/v1/keys",
        json!({ "name": format!("{name}-key"), "account": name }),
    )
    .await;
    made["secret"].as_str().unwrap().to_owned()
}

fn server() -> Router {
    router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ))
}

/// Two nodes and a fact between them, written by the owner.
async fn fact(app: &Router, ns: &str) -> String {
    let a = ok(
        app,
        "POST",
        &format!("/v1/{ns}/nodes"),
        json!({ "labels": ["Doc"] }),
    )
    .await;
    let b = ok(
        app,
        "POST",
        &format!("/v1/{ns}/nodes"),
        json!({ "labels": ["Doc"] }),
    )
    .await;
    let edge = ok(
        app,
        "POST",
        &format!("/v1/{ns}/edges"),
        json!({ "from": a["id"], "label": "cites", "to": b["id"] }),
    )
    .await;
    edge["id"].as_str().unwrap().to_owned()
}

// F-018: a batch took back facts without the right to, and wrote with no
// author.
#[tokio::test]
async fn a_batch_needs_the_right_to_retract_and_records_its_author() {
    let app = server();
    let edge = fact(&app, "s").await;
    let writer = principal(&app, "writer", "s", &["read", "create", "update", "delete"]).await;

    let (code, _) = call(
        &app,
        &writer,
        "POST",
        &format!("/v1/s/edges/{edge}/retract"),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    for op in [
        json!({ "op": "retract_edge", "id": edge }),
        json!({ "op": "invalidate_edge", "id": edge, "at": "2026-01-01T00:00:00Z" }),
    ] {
        let (code, body) = call(
            &app,
            &writer,
            "POST",
            "/v1/s/batch",
            Some(json!({ "ops": [op.clone()] })),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{op}: {body}");
    }
    let (_, still) = call(&app, OWNER, "GET", &format!("/v1/s/edges/{edge}"), None).await;
    assert!(still["expired_at"].is_null(), "{still}");

    // What a batch writes is recorded as its caller's, like a write of its own.
    let (code, body) = call(
        &app,
        &writer,
        "POST",
        "/v1/s/batch",
        Some(json!({ "ops": [{ "op": "create_node", "node": { "labels": ["Note"] } }] })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let rows = ok(
        &app,
        "POST",
        "/v1/s/query",
        json!({ "query": "MATCH (n:Note) RETURN n.@recorded_by AS by" }),
    )
    .await;
    assert_eq!(rows["rows"][0][0], json!("writer"), "{rows}");

    // With the right, a batch takes a fact back.
    let editor = principal(
        &app,
        "editor",
        "s",
        &["read", "create", "update", "delete", "retract"],
    )
    .await;
    let (code, body) = call(
        &app,
        &editor,
        "POST",
        "/v1/s/batch",
        Some(json!({ "ops": [{ "op": "retract_edge", "id": edge }] })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
}

// F-020: a GQ write's idempotency key gave another writer the first
// writer's result.
#[tokio::test]
async fn an_idempotency_key_belongs_to_its_caller_and_statement() {
    let app = server();
    let ana = principal(&app, "ana", "s", &["read", "create", "update", "delete"]).await;
    let bob = principal(&app, "bob", "s", &["read", "create", "update", "delete"]).await;
    let write = |text: &str| json!({ "query": text, "idempotency_key": "k1" });

    let (code, first) = call(
        &app,
        &ana,
        "POST",
        "/v1/s/query",
        Some(write("CREATE (n:Secret {code: 'ana-only'}) RETURN n.code")),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{first}");
    // Again, the same caller and statement: the first result, written once.
    let (_, again) = call(
        &app,
        &ana,
        "POST",
        "/v1/s/query",
        Some(write("CREATE (n:Secret {code: 'ana-only'}) RETURN n.code")),
    )
    .await;
    assert_eq!(again["rows"], first["rows"]);

    // Another caller with the same key gets its own write, not ana's rows.
    let (code, theirs) = call(
        &app,
        &bob,
        "POST",
        "/v1/s/query",
        Some(write("CREATE (n:Note {code: 'bob'}) RETURN n.code")),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{theirs}");
    assert_eq!(theirs["rows"][0][0], json!("bob"), "{theirs}");
    // Even the same statement under the same key is bob's own write.
    call(
        &app,
        &bob,
        "POST",
        "/v1/s/query",
        Some(write("CREATE (n:Secret {code: 'ana-only'}) RETURN n.code")),
    )
    .await;

    let counted = ok(
        &app,
        "POST",
        "/v1/s/query",
        json!({ "query": "MATCH (n:Secret) RETURN count(n)" }),
    )
    .await;
    assert_eq!(
        counted["rows"][0][0],
        json!(2),
        "ana once, bob once: {counted}"
    );
}
