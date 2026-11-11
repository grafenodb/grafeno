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

#[tokio::test]
async fn vector_indexes_over_http() {
    let app = router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None));
    let mut ids = Vec::new();
    for i in 0..60 {
        let angle = (i as f32).to_radians() * 6.0;
        let (status, made) = call(
            &app,
            "/v1/s/nodes",
            json!({"labels": ["Item"], "props": {"i": i, "tier": i % 3, "v": [angle.cos(), angle.sin()]}}),
        )
        .await;
        assert!(status.is_success(), "{made}");
        ids.push(made["id"].clone());
    }
    let (status, made) = call(
        &app,
        "/v1/s/query",
        json!({"query": "CREATE VECTOR INDEX angle FOR (n:Item) ON (n.v) OPTIONS {dimension: 2}"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let (status, found) = call(
        &app,
        "/v1/s/search/vector",
        json!({"embedding": [1.0, 0.0], "index": "angle", "limit": 4, "filter": {"props": {"tier": 1}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found.as_array().unwrap().len(), 4);
    let (status, many) = call(
        &app,
        "/v1/s/search/vector",
        json!({"embeddings": [[1.0, 0.0], [0.0, 1.0]], "index": "angle", "limit": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{many}");
    assert_eq!(many.as_array().unwrap().len(), 2);
    assert_eq!(many[0][0]["node"], ids[0]);
    let (status, similar) = call(
        &app,
        "/v1/s/similar",
        json!({"node": ids[10], "index": "angle", "limit": 2}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{similar}");
    let near: Vec<&Value> = similar
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| &hit["node"])
        .collect();
    assert!(
        near.contains(&&ids[9]) && near.contains(&&ids[11]),
        "{similar}"
    );
    let (status, error) = call(&app, "/v1/s/search/vector", json!({"limit": 1})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
}

/// A server builds a background index on its own.
#[test]
fn a_server_builds_background_indexes() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let dir = std::env::temp_dir().join(format!("grafeno-vbuild-{}", grafeno::NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
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
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_grafeno"))
            .args([
                "serve",
                "--db",
                dir.join("v.redb").to_str().unwrap(),
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
    let query = |text: &str| -> Value {
        let mut response = ureq::post(&format!("{url}/v1/b/query"))
            .send_json(json!({ "query": text }))
            .unwrap();
        response.body_mut().read_json().unwrap()
    };
    query("UNWIND range(1, 5000) AS i CREATE (:Item {v: [toFloat(i), 1.0]})");
    let made = query(
        "CREATE VECTOR INDEX later FOR (n:Item) ON (n.v) OPTIONS {dimension: 2, background: true}",
    );
    assert_eq!(made["rows"][0][9], "building");
    loop {
        let shown = query("SHOW INDEXES");
        if shown["rows"][0][9] == "online" {
            assert_eq!(shown["rows"][0][4], 5000);
            break;
        }
        assert!(Instant::now() < deadline, "not built: {shown}");
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(_server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn new_algorithms_over_http() {
    let app = router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None));
    let (status, made) = call(
        &app,
        "/v1/s/query",
        json!({"query": "CREATE (a:R {name: 'a'})-[:D]->(b:R {name: 'b'})-[:D]->(c:R {name: 'c'})"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let (status, cores) = call(&app, "/v1/s/algo/kCore", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{cores}");
    assert_eq!(cores["rows"].as_array().unwrap().len(), 3);
    let (status, points) = call(&app, "/v1/s/algo/articulationPoints", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{points}");
    assert_eq!(points["rows"][0][0]["name"], "b");
    // One column per node: nothing to write.
    let (status, _) = call(
        &app,
        "/v1/s/algo/articulationPoints",
        json!({"write": "cut"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, written) = call(&app, "/v1/s/algo/kCore", json!({"write": "core"})).await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["rows"][0][0], 3);
    let (status, bridges) = call(&app, "/v1/s/algo/bridges", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{bridges}");
    assert_eq!(bridges["rows"].as_array().unwrap().len(), 2);
    assert!(bridges["rows"][0][0].is_string(), "{bridges}");
}

#[tokio::test]
async fn algorithm_statistics_over_http() {
    let app = router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None));
    call(
        &app,
        "/v1/s/query",
        json!({"query": "CREATE (a:R {name: 'a'})-[:D {w: 2}]->(b:R {name: 'b'})-[:D {w: 4}]->(c:R {name: 'c'}), (x:Q)"}),
    )
    .await;
    let (status, stats) = call(
        &app,
        "/v1/s/algo/kCore",
        json!({"settings": {"labels": "R"}, "stats": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    assert_eq!(
        stats["columns"],
        json!(["column", "count", "min", "max", "mean", "sum"])
    );
    assert_eq!(stats["rows"][0][1], 3, "three nodes with the label");
    assert_eq!(stats["rows"][1][0], "core");
    for body in [
        json!({"settings": {"mode": "stats"}}),
        json!({"settings": {"write": "x"}}),
        json!({"stats": true, "write": "x"}),
        json!({"stats": true, "settings": {"bad key": 1}}),
    ] {
        let (status, error) = call(&app, "/v1/s/algo/kCore", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {error}");
    }
}

#[tokio::test]
async fn traced_queries_over_http() {
    let app = router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None));
    call(
        &app,
        "/v1/s/query",
        json!({"query": "CREATE (:P {n: 1})-[:K]->(:P {n: 2})"}),
    )
    .await;
    let (status, traced) = call(
        &app,
        "/v1/s/query",
        json!({"query": "MATCH (a:P {n: 1})-->(b) RETURN b.n", "trace": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{traced}");
    assert_eq!(traced["trace"]["format"], 1);
    assert_eq!(traced["trace"]["events"][0]["kind"], "anchor");
    let (_, plain) = call(&app, "/v1/s/query", json!({"query": "RETURN 1"})).await;
    assert!(plain.get("trace").is_none());
}
