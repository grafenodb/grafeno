//! A namespace in Spanish over HTTP: recall finds a memory by another form
//! of its words, once the namespace says its language.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
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

async fn recalled(app: &Router, ns: &str, text: &str) -> Vec<String> {
    let (status, results) = call(
        app,
        "POST",
        &format!("/v1/{ns}/recall"),
        json!({ "text": text, "expand": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{results}");
    results
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["node"]["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn recall_in_spanish_finds_other_forms_of_the_words() {
    let app = router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None));
    let (status, body) = call(
        &app,
        "POST",
        "/v1/namespaces",
        json!({ "name": "es", "language": "es" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["language"], "es");
    let (_, made) = call(
        &app,
        "POST",
        "/v1/es/nodes",
        json!({ "labels": ["Decision"], "text": "Decidimos revisar los cambios antes de publicarlos" }),
    )
    .await;
    let id = made["id"].as_str().unwrap().to_owned();
    assert_eq!(
        recalled(&app, "es", "¿qué decidimos sobre la revisión?").await,
        vec![id.clone()]
    );
    assert_eq!(
        recalled(&app, "es", "publicar cambio").await,
        vec![id.clone()]
    );

    // Without a language, the same text needs the same words.
    let (_, made) = call(
        &app,
        "POST",
        "/v1/plain/nodes",
        json!({ "labels": ["Decision"], "text": "Decidimos revisar los cambios antes de publicarlos" }),
    )
    .await;
    let plain = made["id"].as_str().unwrap().to_owned();
    assert!(recalled(&app, "plain", "publicar cambio").await.is_empty());

    // Saying it later indexes what is there.
    let (status, body) = call(
        &app,
        "PATCH",
        "/v1/namespaces/plain",
        json!({ "language": "es" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        recalled(&app, "plain", "publicar cambio").await,
        vec![plain]
    );
    // The description is left as it was when only the language changes.
    let (status, body) = call(
        &app,
        "PATCH",
        "/v1/namespaces/plain",
        json!({ "description": "notas" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["language"], "es", "{body}");
    let (status, body) = call(
        &app,
        "PATCH",
        "/v1/namespaces/plain",
        json!({ "language": "latin" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
