//! `POST /v1/{ns}/nodes/find`: nodes by their properties, sorted and paged
//! with a cursor.

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

#[tokio::test]
async fn people_of_a_country_by_name_page_after_page() {
    let app = router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None));
    let (status, made) = call(
        &app,
        "/v1/crm/query",
        json!({"query": "UNWIND range(0, 499) AS i CREATE (:Person {name: 'N' + toString((i * 37) % 500), country: CASE WHEN i % 5 = 0 THEN 'PE' ELSE 'AR' END, age: i % 60})"}),
    )
    .await;
    assert!(status.is_success(), "{made}");
    let (status, made) = call(
        &app,
        "/v1/crm/query",
        json!({"query": "CREATE INDEX FOR (p:Person) ON (p.country, p.name)"}),
    )
    .await;
    assert!(status.is_success(), "{made}");

    let mut names: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut body = json!({
            "label": "Person",
            "where": {"country": "PE", "age": {"gte": 10}},
            "order_by": "name",
            "limit": 15
        });
        if let Some(after) = &after {
            body["after"] = json!(after);
        }
        let (status, page) = call(&app, "/v1/crm/nodes/find", body).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(
            page["plan"].as_str().unwrap().contains("in order"),
            "{page}"
        );
        for node in page["items"].as_array().unwrap() {
            assert_eq!(node["props"]["country"], "PE");
            assert!(node["props"]["age"].as_i64().unwrap() >= 10);
            names.push(node["props"]["name"].as_str().unwrap().to_owned());
        }
        match page["next"].as_str() {
            Some(next) => after = Some(next.to_owned()),
            None => break,
        }
    }
    let expected = (0..500).filter(|i| i % 5 == 0 && i % 60 >= 10).count();
    assert_eq!(names.len(), expected);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);

    // Descending, with `in` and `prefix`.
    let (status, page) = call(
        &app,
        "/v1/crm/nodes/find",
        json!({
            "label": "Person",
            "where": {"country": {"in": ["PE"]}, "name": {"prefix": "N1"}},
            "order_by": "age",
            "descending": true,
            "limit": 3
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let ages: Vec<i64> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["props"]["age"].as_i64().unwrap())
        .collect();
    assert_eq!(ages.len(), 3);
    assert!(ages.windows(2).all(|pair| pair[0] >= pair[1]), "{ages:?}");

    // Mistakes are 400s.
    for body in [
        json!({"label": "Person", "where": {"name": {"prefix": 3}}}),
        json!({"label": "Person", "descending": true}),
        json!({"label": "Person", "after": "nope"}),
        json!({"label": "Person", "sort": "name"}),
    ] {
        let (status, error) = call(&app, "/v1/crm/nodes/find", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} → {error}");
    }
}
