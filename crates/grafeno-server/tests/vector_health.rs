//! Vector graphs worn by removals: their health over HTTP, compacting a
//! vector index by name, and the background step that compacts on its own.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::{Grafeno, IndexDef, NewNode, VectorSpec};
use grafeno_server::events::{BackgroundConfig, compact_vectors_step};
use grafeno_server::http::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or(Body::empty(), |body| Body::from(body.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// 300 items with an embedding and a property `v` that a vector index
/// covers; then the first 200 lose both.
fn worn(db: &Grafeno) {
    let ids = db
        .write(|tx| {
            (0..300)
                .map(|i| {
                    let angle = (i as f32).to_radians();
                    tx.create_node(
                        "w",
                        NewNode::new("Item")
                            .prop("v", json!([angle.cos(), angle.sin()]))
                            .embedding(vec![angle.cos(), angle.sin(), 1.0]),
                    )
                })
                .collect::<grafeno::Result<Vec<_>>>()
        })
        .unwrap();
    db.write(|tx| {
        tx.create_index(
            "w",
            IndexDef {
                name: "angle".into(),
                vector: Some(VectorSpec::new(2)),
                ..IndexDef::new("Item", "v")
            },
        )
    })
    .unwrap();
    db.write(|tx| {
        for id in &ids[..200] {
            tx.remove_embedding("w", *id)?;
            tx.remove_node_props("w", *id, &["v"])?;
        }
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn health_and_compacting_an_index_over_http() {
    let db = Arc::new(Grafeno::in_memory().unwrap());
    worn(&db);
    let app = router(AppState::new(db.clone(), None));

    let (status, health) = call(&app, "GET", "/v1/w/vectors/health", None).await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_eq!(health[0]["index"], Value::Null);
    assert_eq!(health[0]["vectors"], 100);
    assert_eq!(health[0]["removed"], 200);
    assert_eq!(health[0]["churn"], 2.0);
    assert_eq!(health[1]["index"], "angle");
    assert_eq!(health[1]["removed"], 200);

    let (status, report) = call(&app, "POST", "/v1/w/compact?index=angle", None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["vectors"], 100);
    let (_, health) = call(&app, "GET", "/v1/w/vectors/health", None).await;
    assert_eq!(
        (health[0]["removed"].clone(), health[1]["removed"].clone()),
        (json!(200), json!(0))
    );

    let (status, _) = call(&app, "POST", "/v1/w/compact?index=missing", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_server_compacts_worn_graphs_on_its_own() {
    let db = Arc::new(Grafeno::in_memory().unwrap());
    worn(&db);
    // 100 vectors left in each graph: over a minimum of 50, and worn far
    // past half.
    let config = |at: f64| BackgroundConfig {
        compact_vectors_at: Some(at),
        compact_vectors_min: Some(50),
        ..BackgroundConfig::default()
    };
    let off = AppState::new(db.clone(), None).with_background(config(0.0));
    assert!(
        compact_vectors_step(&off).unwrap().is_none(),
        "0 turns it off"
    );
    let bigger = AppState::new(db.clone(), None).with_background(BackgroundConfig {
        compact_vectors_min: Some(1_000),
        ..config(0.5)
    });
    assert!(
        compact_vectors_step(&bigger).unwrap().is_none(),
        "small graphs left alone"
    );
    let smaller = AppState::new(db.clone(), None).with_background(BackgroundConfig {
        compact_vectors_max: Some(99),
        ..config(0.5)
    });
    assert!(
        compact_vectors_step(&smaller).unwrap().is_none(),
        "large graphs too"
    );

    let state = AppState::new(db.clone(), None).with_background(config(0.5));
    let report = compact_vectors_step(&state).unwrap().expect("it compacts");
    let compacted = report["compacted"].as_array().unwrap();
    assert_eq!(compacted.len(), 2, "{report}");
    assert_eq!(compacted[1]["index"], "angle");
    assert_eq!(compacted[0]["removed"], 200);
    let read = db.begin_read().unwrap();
    assert!(
        read.vector_health("w")
            .unwrap()
            .iter()
            .all(|h| h.removed == 0)
    );
    drop(read);
    assert!(
        compact_vectors_step(&state).unwrap().is_none(),
        "nothing left to do"
    );
}
