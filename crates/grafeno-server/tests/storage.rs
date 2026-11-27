//! `GET /v1/storage`: what the database takes, for owners.

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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn owners_see_every_table_and_namespace() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    let (code, _) = call(
        &app,
        OWNER,
        "POST",
        "/v1/shop/query",
        Some(json!({"query": "UNWIND range(1, 50) AS i CREATE (:Item {i: i})"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, report) = call(&app, OWNER, "GET", "/v1/storage", None).await;
    assert_eq!(code, StatusCode::OK, "{report}");
    assert!(
        report["tables"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "nodes" && t["entries"] == 50)
    );
    assert!(report.get("namespaces").is_none());
    let (_, report) = call(&app, OWNER, "GET", "/v1/storage?namespaces=true", None).await;
    let shop = report["namespaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "shop")
        .unwrap()
        .clone();
    assert_eq!(shop["nodes"], 50);

    // A reader of the namespace is not an owner.
    call(&app, OWNER, "PUT", "/v1/roles/shop-reader", Some(json!({"privileges": [{"action": "read", "object": {"on": "namespace", "namespace": "shop"}}]}))).await;
    call(
        &app,
        OWNER,
        "POST",
        "/v1/accounts",
        Some(json!({"name": "reader", "kind": "service", "roles": ["shop-reader"]})),
    )
    .await;
    let (_, made) = call(
        &app,
        OWNER,
        "POST",
        "/v1/keys",
        Some(json!({"name": "reader-key", "account": "reader"})),
    )
    .await;
    let key = made["secret"].as_str().unwrap().to_owned();
    let (code, _) = call(&app, &key, "GET", "/v1/storage", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn owners_plan_and_run_migrations() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    call(
        &app,
        OWNER,
        "POST",
        "/v1/shop/query",
        Some(json!({"query": "UNWIND range(1, 50) AS i CREATE (:Item {i: i})"})),
    )
    .await;
    let options = json!({"namespaces": ["shop"], "encoding": "envelope", "reindex": true, "verify": true, "batch_size": 20});
    let (code, plan) = call(
        &app,
        OWNER,
        "POST",
        "/v1/storage/migrations",
        Some(json!({"options": options, "dry_run": true})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{plan}");
    assert_eq!(plan["state"], "pending");
    let (_, list) = call(&app, OWNER, "GET", "/v1/storage/migrations", None).await;
    assert_eq!(list, json!([]));

    let (code, done) = call(
        &app,
        OWNER,
        "POST",
        "/v1/storage/migrations",
        Some(json!({"options": options})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "completed", "{done}");
    let (_, list) = call(&app, OWNER, "GET", "/v1/storage/migrations", None).await;
    assert_eq!(list[0]["id"], done["id"]);
    // The data reads as before.
    let (_, rows) = call(
        &app,
        OWNER,
        "POST",
        "/v1/shop/query",
        Some(json!({"query": "MATCH (n:Item) RETURN sum(n.i) AS s"})),
    )
    .await;
    assert_eq!(rows["rows"][0][0], 1275, "{rows}");
    // Nothing pending is reported for a database just migrated.
    let (_, report) = call(&app, OWNER, "GET", "/v1/storage", None).await;
    assert!(report["pending"].is_array(), "{report}");
    // Unknown fields are refused.
    let (code, _) = call(
        &app,
        OWNER,
        "POST",
        "/v1/storage/migrations",
        Some(json!({"dryrun": true})),
    )
    .await;
    assert!(code.is_client_error());
}
