//! Full-text and hybrid search over HTTP (16.6): a full-text index declared
//! in GQ, searched at `/search/text` with fragments, at `/search` with a
//! mode and a fusion, and by recall.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
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

async fn setup() -> Router {
    let app = router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None));
    for (i, (title, body)) in [
        ("Graph memory", "Agents keep memories in a graph."),
        ("Cooking", "Rice and beans."),
        ("Vector search", "Embeddings and nearest neighbours."),
    ]
    .iter()
    .enumerate()
    {
        let angle = i as f32;
        let (status, created) = call(
            &app,
            "/v1/s/nodes",
            json!({
                "labels": ["Note"],
                "props": {"title": title, "body": body},
                "embedding": [angle.cos(), angle.sin(), 1.0]
            }),
        )
        .await;
        assert!(status.is_success(), "{created}");
    }
    let (status, made) = call(
        &app,
        "/v1/s/query",
        json!({"query": "CREATE FULLTEXT INDEX notes FOR (n:Note) ON EACH [n.title, n.body] OPTIONS {analyzer: 'english', weights: {title: 2}}"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    app
}

#[tokio::test]
async fn fulltext_and_hybrid_search() {
    let app = setup().await;
    let (status, hits) = call(
        &app,
        "/v1/s/search/text",
        json!({"query": "+memory graph*", "index": "notes", "fragments": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["fragments"][0]["field"], "title");
    // Without an index: the node text, as before, with no fragments.
    let (status, plain) = call(&app, "/v1/s/search/text", json!({"query": "memory"})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        plain
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit.get("fragments").is_none())
    );

    let (status, hybrid) = call(
        &app,
        "/v1/s/search",
        json!({"text": "graph", "index": "notes", "embedding": [1.0, 0.0, 1.0], "mode": "hybrid", "limit": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hybrid}");
    assert_eq!(hybrid[0]["text_rank"], 1);
    assert_eq!(hybrid[0]["vector_rank"], 1);
    let (status, vector) = call(
        &app,
        "/v1/s/search",
        json!({"embedding": [0.0, 1.0, 1.0], "mode": "vector", "limit": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{vector}");
    assert!(vector[0]["similarity"].as_f64().unwrap() > 0.9);
    for (body, expected) in [
        (json!({"text": "x", "mode": "hybrid"}), "needs an embedding"),
        (json!({"text": "x", "fusion": "max"}), "unknown fusion"),
        (json!({"text": "x", "index": "nothing"}), "no index named"),
    ] {
        let (status, error) = call(&app, "/v1/s/search", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
        assert!(error.to_string().contains(expected), "{error}");
    }

    // Recall through the index, with weighted fusion.
    let (status, recalled) = call(
        &app,
        "/v1/s/recall",
        json!({"text": "\"nearest neighbours\"", "index": "notes", "fusion": "weighted", "text_weight": 1.0, "expand": false, "limit": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{recalled}");
    assert_eq!(recalled[0]["node"]["props"]["title"], "Vector search");
}
