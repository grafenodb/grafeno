use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app(token: Option<&str>) -> Router {
    router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        token,
    ))
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_with(app, method, uri, body, None).await
}

async fn call_with(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
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

fn id(value: &Value) -> String {
    value["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn health_is_public() {
    let app = app(Some("secret"));
    let (status, body) = call(&app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn token_is_required_when_configured() {
    let app = app(Some("secret"));
    let (status, body) = call(&app, "GET", "/v1/ns/nodes", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    let (status, _) = call_with(&app, "GET", "/v1/ns/nodes", None, Some("wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call_with(&app, "GET", "/v1/ns/nodes", None, Some("secret")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn node_and_edge_lifecycle() {
    let app = app(None);
    let (status, ana) = call(
        &app,
        "POST",
        "/v1/vault/nodes",
        Some(json!({ "labels": ["Person"], "props": { "name": "Ana" } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, note) = call(
        &app,
        "POST",
        "/v1/vault/nodes",
        Some(json!({ "labels": ["Note"], "props": { "title": "Rust" }, "text": "ownership and borrowing" })),
    )
    .await;
    let (ana, note) = (id(&ana), id(&note));

    let (status, edge) = call(
        &app,
        "POST",
        "/v1/vault/edges",
        Some(json!({ "from": ana, "label": "wrote", "to": note, "valid_from": "2024-01-01" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let edge = id(&edge);

    let (status, body) = call(&app, "GET", &format!("/v1/vault/nodes/{note}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "ownership and borrowing");
    assert_eq!(body["labels"], json!(["Note"]));

    let uri =
        format!("/v1/vault/nodes/{ana}/neighbors?direction=out&label=wrote&valid_at=2023-06-01");
    let (_, body) = call(&app, "GET", &uri, None).await;
    assert_eq!(body.as_array().unwrap().len(), 0, "not valid yet");
    let uri = format!("/v1/vault/nodes/{ana}/neighbors?direction=out&valid_at=2024-06-01");
    let (_, body) = call(&app, "GET", &uri, None).await;
    assert_eq!(body[0]["node"], note.as_str());
    assert_eq!(body[0]["edge"]["label"], "wrote");

    let (status, body) = call(
        &app,
        "PATCH",
        &format!("/v1/vault/nodes/{note}"),
        Some(json!({ "props": { "title": null, "pinned": true }, "add_labels": ["Pinned"], "text": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // A null is stored; `remove_props` removes.
    assert_eq!(body["props"], json!({ "pinned": true, "title": null }));
    let (_, body) = call(
        &app,
        "PATCH",
        &format!("/v1/vault/nodes/{note}"),
        Some(json!({ "remove_props": ["title"] })),
    )
    .await;
    assert_eq!(body["props"], json!({ "pinned": true }));
    assert_eq!(body["labels"], json!(["Note", "Pinned"]));
    assert!(body.get("text").is_none());

    let (status, body) = call(
        &app,
        "POST",
        &format!("/v1/vault/edges/{edge}/invalidate"),
        Some(json!({ "at": "2025-01-01T00:00:00Z" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid_to"], 1_735_689_600_000_000i64);

    let (status, _) = call(&app, "DELETE", &format!("/v1/vault/nodes/{ana}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(&app, "GET", &format!("/v1/vault/edges/{edge}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn put_upserts_with_client_ids() {
    let app = app(None);
    let uri = "/v1/vault/nodes/0190f5c2-7a3e-7cde-8f01-23456789abcd";
    let body = json!({ "labels": ["Note"], "text": "first" });
    let (status, created) = call(&app, "PUT", uri, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["created"], true);
    let (status, updated) = call(
        &app,
        "PUT",
        uri,
        Some(json!({ "labels": ["Note"], "text": "second" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["created"], false);
    let (_, node) = call(&app, "GET", uri, None).await;
    assert_eq!(node["text"], "second");
}

#[tokio::test]
async fn bad_input_is_reported_as_json() {
    let app = app(None);
    let (status, body) = call(
        &app,
        "POST",
        "/v1/vault/nodes",
        Some(json!({ "labels": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid");

    let (status, body) = call(
        &app,
        "POST",
        "/v1/vault/nodes",
        Some(json!({ "labelz": ["x"] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");

    let (status, _) = call(&app, "GET", "/v1/vault/nodes/not-a-uuid", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn batch_is_atomic_and_resolves_refs() {
    let app = app(None);
    let note_id = "0190f5c2-7a3e-7cde-8f01-000000000001";
    let ops = json!({ "ops": [
        { "op": "upsert_node", "ref": "$note", "node": { "id": note_id, "labels": ["Note"], "props": { "title": "Index" }, "text": "graph engines" } },
        { "op": "create_node", "ref": "$tag", "node": { "labels": ["Tag"], "props": { "name": "rust" } } },
        { "op": "create_edge", "edge": { "from": "$note", "label": "tagged", "to": "$tag" } },
        { "op": "delete_edges_from", "from": "$note", "label": "links_to" }
    ]});
    let (status, body) = call(&app, "POST", "/v1/vault/batch", Some(ops)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["refs"]["$note"], note_id);
    assert_eq!(body["results"][3]["deleted"], 0);
    let tag = body["refs"]["$tag"].as_str().unwrap().to_owned();

    // A failing operation rolls back the whole batch.
    let ops = json!({ "ops": [
        { "op": "create_node", "node": { "labels": ["Note"], "text": "should not stay" } },
        { "op": "create_edge", "edge": { "from": "$missing", "label": "x", "to": tag } }
    ]});
    let (status, body) = call(&app, "POST", "/v1/vault/batch", Some(ops)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("operation 1:")
    );
    let (_, nodes) = call(&app, "GET", "/v1/vault/nodes", None).await;
    assert_eq!(nodes["items"].as_array().unwrap().len(), 2);
    assert!(nodes["next"].is_null());

    let (_, tagged) = call(&app, "GET", "/v1/vault/nodes?label=Tag", None).await;
    assert_eq!(tagged["items"][0]["id"], tag.as_str());
}

#[tokio::test]
async fn listings_are_paged() {
    let app = app(None);
    let ops: Vec<Value> = (0..5)
        .map(|i| json!({ "op": "create_node", "ref": format!("$n{i}"), "node": { "labels": ["N"] } }))
        .chain((0..4).map(|i| {
            json!({ "op": "create_edge", "edge": { "from": format!("$n{i}"), "label": "next", "to": format!("$n{}", i + 1) } })
        }))
        .collect();
    call(&app, "POST", "/v1/p/batch", Some(json!({ "ops": ops }))).await;

    let (_, first) = call(&app, "GET", "/v1/p/nodes?limit=2", None).await;
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    let next = first["next"].as_str().unwrap().to_owned();
    assert_eq!(next, first["items"][1]["id"].as_str().unwrap());
    let (_, second) = call(
        &app,
        "GET",
        &format!("/v1/p/nodes?limit=2&after={next}"),
        None,
    )
    .await;
    assert_ne!(second["items"][0]["id"], first["items"][0]["id"]);
    let (_, last) = call(
        &app,
        "GET",
        &format!("/v1/p/nodes?limit=4&after={next}"),
        None,
    )
    .await;
    assert_eq!(last["items"].as_array().unwrap().len(), 3);
    assert!(last["next"].is_null());

    let (_, edges) = call(&app, "GET", "/v1/p/edges?label=next&limit=10", None).await;
    assert_eq!(edges["items"].as_array().unwrap().len(), 4);
    let (_, none) = call(&app, "GET", "/v1/p/edges?label=other", None).await;
    assert!(none["items"].as_array().unwrap().is_empty());
    let (status, _) = call(&app, "GET", "/v1/p/nodes?after=nope", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn administration_endpoints() {
    let app = app(None);
    let ops = json!({ "ops": [
        { "op": "create_node", "ref": "$a", "node": { "labels": ["Person"], "props": { "name": "Ana" }, "text": "writes Rust", "embedding": [1.0, 0.0] } },
        { "op": "create_node", "ref": "$b", "node": { "labels": ["Person"], "text": "reads Go" } },
        { "op": "create_edge", "edge": { "from": "$a", "label": "knows", "to": "$b" } }
    ]});
    call(&app, "POST", "/v1/team/batch", Some(ops)).await;
    call(
        &app,
        "POST",
        "/v1/solo/nodes",
        Some(json!({ "labels": ["Note"] })),
    )
    .await;

    let (_, namespaces) = call(&app, "GET", "/v1/namespaces", None).await;
    let counts: Vec<Value> = namespaces
        .as_array()
        .unwrap()
        .iter()
        .map(|info| json!([info["name"], info["nodes"], info["edges"]]))
        .collect();
    assert_eq!(counts, [json!(["solo", 1, 0]), json!(["team", 2, 1])]);
    assert!(namespaces[0]["created_at"].is_number());
    let (_, stats) = call(&app, "GET", "/v1/team/stats", None).await;
    assert_eq!(stats["node_labels"]["Person"], 2);
    assert_eq!(stats["vector_dimension"], 2);

    let (_, report) = call(&app, "GET", "/v1/team/check", None).await;
    assert_eq!(report["problems"], json!([]));
    let (status, report) = call(&app, "POST", "/v1/team/rebuild", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["texts"], 2);

    // Export one namespace and import it into another server.
    let request = Request::builder()
        .uri("/v1/export?ns=team")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/x-ndjson"
    );
    let dump = response.into_body().collect().await.unwrap().to_bytes();
    // The header, the namespace, two nodes and an edge.
    assert_eq!(String::from_utf8(dump.to_vec()).unwrap().lines().count(), 5);

    let other = self::app(None);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/import")
        .body(Body::from(dump))
        .unwrap();
    let response = other.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (_, namespaces) = call(&other, "GET", "/v1/namespaces", None).await;
    assert_eq!(namespaces.as_array().unwrap().len(), 1);
    assert_eq!(
        (
            &namespaces[0]["name"],
            &namespaces[0]["nodes"],
            &namespaces[0]["edges"]
        ),
        (&json!("team"), &json!(2), &json!(1))
    );
    let (_, hits) = call(
        &other,
        "POST",
        "/v1/team/search/text",
        Some(json!({ "query": "rust" })),
    )
    .await;
    assert_eq!(hits.as_array().unwrap().len(), 1);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/import")
        .body(Body::from("not an export"))
        .unwrap();
    let response = other.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_recall_and_context() {
    let app = app(None);
    let ops = json!({ "ops": [
        { "op": "create_node", "ref": "$ana", "node": { "labels": ["Person"], "props": { "name": "Ana" } } },
        { "op": "create_node", "ref": "$note", "node": { "labels": ["Note"], "props": { "title": "Rust notes" },
          "text": "the borrow checker enforces ownership", "embedding": [1.0, 0.0] } },
        { "op": "create_node", "node": { "labels": ["Note"], "text": "pasta recipes", "embedding": [0.0, 1.0] } },
        { "op": "create_edge", "edge": { "from": "$ana", "label": "wrote", "to": "$note" } }
    ]});
    let (status, body) = call(&app, "POST", "/v1/agent/batch", Some(ops)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let note = body["refs"]["$note"].as_str().unwrap().to_owned();

    let (_, hits) = call(
        &app,
        "POST",
        "/v1/agent/search/text",
        Some(json!({ "query": "borrow" })),
    )
    .await;
    assert_eq!(hits[0]["node"], note.as_str());
    let (_, hits) = call(
        &app,
        "POST",
        "/v1/agent/search/vector",
        Some(json!({ "embedding": [0.9, 0.1], "limit": 1 })),
    )
    .await;
    assert_eq!(hits[0]["node"], note.as_str());

    let (_, results) = call(
        &app,
        "POST",
        "/v1/agent/recall",
        Some(json!({ "text": "ownership" })),
    )
    .await;
    assert_eq!(results[0]["node"]["id"], note.as_str());
    assert_eq!(results[0]["signals"][0]["kind"], "text");
    assert_eq!(results[1]["node"]["props"]["name"], "Ana");
    assert_eq!(results[1]["signals"][0]["kind"], "graph");

    let (_, results) = call(
        &app,
        "POST",
        "/v1/agent/recall",
        Some(json!({ "text": "ownership", "expand": false })),
    )
    .await;
    assert_eq!(results.as_array().unwrap().len(), 1);

    let (status, context) = call(
        &app,
        "POST",
        "/v1/agent/context",
        Some(json!({ "text": "ownership", "budget": 200 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rendered = context["rendered"].as_str().unwrap();
    assert!(rendered.contains("[1] Rust notes (Note)"), "{rendered}");
    assert!(rendered.contains("- Ana wrote Rust notes"), "{rendered}");
    assert!(context["tokens"].as_u64().unwrap() <= 200);

    let (status, body) = call(
        &app,
        "POST",
        "/v1/agent/recall",
        Some(json!({ "limit": 3 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn traversals() {
    let app = app(None);
    let ops = json!({ "ops": [
        { "op": "create_node", "ref": "$a", "node": { "labels": ["N"] } },
        { "op": "create_node", "ref": "$b", "node": { "labels": ["N"] } },
        { "op": "create_node", "ref": "$c", "node": { "labels": ["N"] } },
        { "op": "create_edge", "edge": { "from": "$a", "label": "next", "to": "$b" } },
        { "op": "create_edge", "edge": { "from": "$b", "label": "next", "to": "$c" } }
    ]});
    let (_, body) = call(&app, "POST", "/v1/g/batch", Some(ops)).await;
    let (a, c) = (body["refs"]["$a"].clone(), body["refs"]["$c"].clone());

    let (_, path) = call(
        &app,
        "POST",
        "/v1/g/path",
        Some(json!({ "from": a, "to": c, "direction": "out" })),
    )
    .await;
    assert_eq!(path["path"]["nodes"].as_array().unwrap().len(), 3);
    let (_, path) = call(
        &app,
        "POST",
        "/v1/g/path",
        Some(json!({ "from": c, "to": a, "direction": "out" })),
    )
    .await;
    assert!(path["path"].is_null());

    let (_, hops) = call(
        &app,
        "POST",
        "/v1/g/k-hop",
        Some(json!({ "start": a, "depth": 2, "direction": "out" })),
    )
    .await;
    assert_eq!(hops.as_array().unwrap().len(), 2);

    let (_, sub) = call(
        &app,
        "POST",
        "/v1/g/subgraph",
        Some(json!({ "seeds": [a], "depth": 1 })),
    )
    .await;
    assert_eq!(sub["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(sub["edges"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn queries_read_and_write() {
    let app = app(None);
    let (status, body) = call(
        &app,
        "POST",
        "/v1/g/query",
        Some(json!({ "query": "CREATE (a:Person {name: $name})-[:knows]->(b:Person {name: 'Bob'}) RETURN a.name",
                     "params": { "name": "Ana" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"], json!([["Ana"]]));
    assert_eq!(body["stats"]["nodes_created"], 2);

    let (_, body) = call(
        &app,
        "POST",
        "/v1/g/query",
        Some(json!({ "query": "MATCH (a)-[r:knows]->(b) RETURN a.name, type(r), b" })),
    )
    .await;
    assert_eq!(body["columns"], json!(["a.name", "type(r)", "b"]));
    assert_eq!(body["rows"][0][2]["props"]["name"], "Bob");

    let (status, body) = call(
        &app,
        "POST",
        "/v1/g/query",
        Some(json!({ "query": "MATCH (n RETURN n" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_query");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("line 1, column")
    );
}

#[tokio::test]
async fn no_interface_and_cors_options() {
    use grafeno_server::http::{Options, router_with};
    let state = || AppState::new(Arc::new(Grafeno::in_memory().unwrap()), Some("secret"));
    let request = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();

    // The engine has no interface (15.1): `/` and the Studio's old place
    // say where it went, to anyone.
    let app = router_with(state(), &Options::default());
    for uri in ["/", "/studio", "/studio/", "/studio/explorer/a-route"] {
        let response = app.clone().oneshot(request(uri)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("installed on its own"), "{text}");
        assert!(!text.contains("<script"), "no application is served");
    }
    // No CORS headers unless origins are configured.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .header(header::ORIGIN, "http://elsewhere")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );

    let app = router_with(
        state(),
        &Options {
            cors_origins: vec!["http://studio.example".into()],
        },
    );
    let preflight = Request::builder()
        .method("OPTIONS")
        .uri("/v1/namespaces")
        .header(header::ORIGIN, "http://studio.example")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(preflight).await.unwrap();
    assert_eq!(
        response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "http://studio.example"
    );
    let other = Request::builder()
        .uri("/health")
        .header(header::ORIGIN, "http://evil.example")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(other).await.unwrap();
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );
}

#[tokio::test]
async fn memory_endpoints() {
    let app = app(None);
    let ops = json!({ "ops": [
        { "op": "create_node", "ref": "$ana", "node": { "labels": ["Person"], "props": { "name": "Ana García" } } },
        { "op": "create_node", "ref": "$dup", "node": { "labels": ["Person"], "props": { "name": "ana garcia" } } },
        { "op": "create_node", "ref": "$m1", "node": { "labels": ["Memory"], "text": "Ana likes tea" } },
        { "op": "create_node", "ref": "$m2", "node": { "labels": ["Memory"], "text": "Ana moved to Lima" } },
        { "op": "create_node", "ref": "$m3", "node": { "labels": ["Memory"], "text": "Ana works at Acme" } },
        { "op": "create_edge", "edge": { "from": "$m1", "label": "about", "to": "$ana" } },
        { "op": "create_edge", "edge": { "from": "$m2", "label": "about", "to": "$ana" } },
        { "op": "create_edge", "edge": { "from": "$m3", "label": "about", "to": "$dup" } }
    ]});
    let (status, body) = call(&app, "POST", "/v1/agent/batch", Some(ops)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let refs = |name: &str| body["refs"][name].as_str().unwrap().to_owned();
    let (ana, dup, m1) = (refs("$ana"), refs("$dup"), refs("$m1"));

    let (status, found) = call(
        &app,
        "GET",
        "/v1/agent/resolve?name=ANA%20GARCIA&label=Person",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found.as_array().unwrap().len(), 2);

    let (_, pairs) = call(&app, "GET", "/v1/agent/duplicates", None).await;
    assert_eq!(pairs[0]["score"], 1.0, "{pairs}");

    let (status, report) = call(
        &app,
        "POST",
        "/v1/agent/merge",
        Some(json!({ "keep": ana, "merge": dup })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["edges_moved"], 1);
    let (_, pairs) = call(&app, "GET", "/v1/agent/duplicates", None).await;
    assert!(pairs.as_array().unwrap().is_empty());

    // Recorded, and undone.
    let (status, merges) = call(&app, "GET", &format!("/v1/agent/merges?node={dup}"), None).await;
    assert_eq!(status, StatusCode::OK, "{merges}");
    assert_eq!(merges[0]["id"], report["id"]);
    let (status, undone) = call(
        &app,
        "POST",
        &format!("/v1/agent/merges/{}/undo", report["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{undone}");
    assert!(!undone["undone_at"].is_null(), "{undone}");
    let (status, _) = call(&app, "GET", &format!("/v1/agent/nodes/{dup}"), None).await;
    assert_eq!(status, StatusCode::OK);

    // Two emails: refused with the conflicts, listed by a dry run, merged
    // with force.
    let (_, made) = call(
        &app,
        "POST",
        "/v1/agent/batch",
        Some(json!({ "ops": [
            { "op": "create_node", "ref": "$x", "node": { "labels": ["Person"], "props": { "name": "Eva", "email": "eva@a.com" } } },
            { "op": "create_node", "ref": "$y", "node": { "labels": ["Person"], "props": { "name": "Eva", "email": "eva@b.com" } } }
        ]})),
    )
    .await;
    let (x, y) = (made["refs"]["$x"].clone(), made["refs"]["$y"].clone());
    let (status, refused) = call(
        &app,
        "POST",
        "/v1/agent/merge",
        Some(json!({ "keep": x, "merge": y })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"]["code"], "conflict");
    assert_eq!(refused["error"]["conflicts"][0]["kind"], "strong_key");
    assert_eq!(refused["error"]["conflicts"][0]["key"], "email");
    let (status, dry) = call(
        &app,
        "POST",
        "/v1/agent/merge",
        Some(json!({ "keep": x, "merge": y, "dry_run": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dry["conflicts"].as_array().unwrap().len(), 1);
    let (status, forced) = call(
        &app,
        "POST",
        "/v1/agent/merge",
        Some(json!({ "keep": x, "merge": y, "force": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{forced}");
    assert_eq!(forced["conflicts"][0]["key"], "email");
    // Back to the first merge, for what follows.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/agent/merge",
        Some(json!({ "keep": ana, "merge": dup })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Importance through PATCH, accesses through touch and recall.
    let (status, node) = call(
        &app,
        "PATCH",
        &format!("/v1/agent/nodes/{m1}"),
        Some(json!({ "importance": 0.9 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{node}");
    let (status, _) = call(
        &app,
        "POST",
        "/v1/agent/touch",
        Some(json!({ "ids": [m1] })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, results) = call(
        &app,
        "POST",
        "/v1/agent/recall",
        Some(json!({ "text": "tea", "touch": true, "expand": false })),
    )
    .await;
    assert_eq!(results[0]["node"]["id"], m1.as_str());
    assert!(
        results[0]["signals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "memory")
    );
    let (_, node) = call(&app, "GET", &format!("/v1/agent/nodes/{m1}"), None).await;
    assert_eq!(node["memory"]["accesses"], 2, "{node}");
    assert!((node["memory"]["importance"].as_f64().unwrap() - 0.9).abs() < 1e-6);
    let (status, _) = call(
        &app,
        "PATCH",
        &format!("/v1/agent/nodes/{m1}"),
        Some(json!({ "importance": 3 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, weak) = call(&app, "GET", "/v1/agent/weak?threshold=0.2", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(weak.as_array().unwrap().is_empty(), "{weak}");

    let (_, groups) = call(
        &app,
        "GET",
        "/v1/agent/consolidation?older_than_days=0",
        None,
    )
    .await;
    assert_eq!(groups.as_array().unwrap().len(), 1, "{groups}");
    assert_eq!(groups[0]["entity"]["id"], ana.as_str());
    let sources: Vec<Value> = groups[0]["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].clone())
        .collect();
    assert_eq!(sources.len(), 3);

    let (status, summary) = call(
        &app,
        "POST",
        "/v1/agent/summaries",
        Some(json!({ "text": "Ana likes tea, lives in Lima and works at Acme.", "sources": sources, "about": ana })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{summary}");
    let (_, results) = call(
        &app,
        "POST",
        "/v1/agent/recall",
        Some(json!({ "text": "Lima", "expand": false })),
    )
    .await;
    assert_eq!(results.as_array().unwrap().len(), 1);
    assert_eq!(results[0]["node"]["id"], summary["id"]);
    let (_, results) = call(
        &app,
        "POST",
        "/v1/agent/recall",
        Some(
            json!({ "text": "Lima", "expand": false, "include_archived": true, "ranking": false }),
        ),
    )
    .await;
    assert_eq!(results.as_array().unwrap().len(), 2);

    let (status, archived) = call(
        &app,
        "POST",
        "/v1/agent/archive",
        Some(json!({ "ids": [summary["id"]] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(archived["archived"], 1);
    let (_, check) = call(&app, "GET", "/v1/agent/check", None).await;
    assert_eq!(check["problems"], json!([]), "{check}");
}

#[tokio::test]
async fn queries_over_their_limits_fail_cleanly() {
    let app = router(
        AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None).with_limits(
            grafeno_query::Limits {
                max_reads: Some(50),
                ..grafeno_query::Limits::none()
            },
        ),
    );
    let (status, _) = call(
        &app,
        "POST",
        "/v1/q/query",
        Some(json!({ "query": "UNWIND range(1, 200) AS i CREATE (:N {i: i})" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &app,
        "POST",
        "/v1/q/query",
        Some(json!({ "query": "MATCH (n:N) RETURN count(n)" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "query_limit");
    // An index makes the same question cheap enough.
    call(
        &app,
        "POST",
        "/v1/q/query",
        Some(json!({ "query": "CREATE INDEX FOR (n:N) ON (n.i)" })),
    )
    .await;
    let (status, body) = call(
        &app,
        "POST",
        "/v1/q/query",
        Some(json!({ "query": "MATCH (n:N) WHERE n.i > 195 RETURN count(n)" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"], json!([[5]]));
}

#[tokio::test]
async fn concurrent_writes_share_commits() {
    let app = app(None);
    let requests = (0..64).map(|i| {
        let app = app.clone();
        async move {
            call(
                &app,
                "POST",
                "/v1/c/nodes",
                Some(json!({ "labels": ["Note"], "props": { "i": i } })),
            )
            .await
        }
    });
    let responses = futures_join_all(requests).await;
    assert!(
        responses
            .iter()
            .all(|(status, _)| *status == StatusCode::CREATED)
    );
    // A failing write among them does not affect the others.
    let (status, _) = call(&app, "POST", "/v1/c/nodes", Some(json!({ "labels": [] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, stats) = call(&app, "GET", "/v1/c/stats", None).await;
    assert_eq!(stats["nodes"], 64);
    // A GQ write goes through the same writer, and its errors keep their code.
    let (status, body) = call(
        &app,
        "POST",
        "/v1/c/query",
        Some(json!({ "query": "MATCH (n:Note) SET n.seen = true" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app,
        "POST",
        "/v1/c/query",
        Some(json!({ "query": "CREATE (n:X) RETURN 1 / 0" })),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_query"))
    );
    let (_, stats) = call(&app, "GET", "/v1/c/stats", None).await;
    assert_eq!(stats["nodes"], 64, "the failed query left nothing");
}

/// Awaits every future (in order), without another dependency.
async fn futures_join_all<F>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futures.into_iter().map(tokio::spawn).collect();
    let mut out = Vec::new();
    for handle in handles {
        out.push(handle.await.unwrap());
    }
    out
}

#[tokio::test]
async fn a_full_server_answers_503() {
    let state = AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None).with_max_requests(1);
    // Hold the only slot, as a slow request would.
    let _busy = state.in_flight.clone().try_acquire_owned().unwrap();
    let app = router(state);
    let request = Request::builder()
        .uri("/v1/namespaces")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    // Health checks stay outside the limit.
    let (status, _) = call(&app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn graph_algorithms() {
    let app = app(None);
    let (status, body) = call(
        &app,
        "POST",
        "/v1/g/query",
        Some(json!({ "query": "CREATE (a:Person {name: 'Ana'})-[:knows {w: 1}]->(b:Person {name: 'Bob'})-[:knows {w: 1}]->(c:Person {name: 'Cy'}), (a)-[:knows {w: 5}]->(c) RETURN a.@id, c.@id" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (ana, cy) = (body["rows"][0][0].clone(), body["rows"][0][1].clone());

    let (status, list) = call(&app, "GET", "/v1/algorithms", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "algo.pageRank")
    );

    let (status, body) = call(
        &app,
        "POST",
        "/v1/g/algo/shortestPath",
        Some(json!({ "args": [ana, cy], "settings": { "weight": "w" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["columns"], json!(["nodes", "edges", "cost"]));
    let names: Vec<&str> = body["rows"][0][0]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Ana", "Bob", "Cy"]);
    assert_eq!(body["rows"][0][2], 2.0);

    let (status, body) = call(
        &app,
        "POST",
        "/v1/g/algo/pageRank",
        Some(json!({ "settings": { "direction": "both" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"].as_array().unwrap().len(), 3);
    assert!(body["rows"][0][0]["id"].is_string());

    // Write mode stores the score as a property.
    let (status, body) = call(
        &app,
        "POST",
        "/v1/g/algo/louvain",
        Some(json!({ "write": "community" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"], json!([[3]]));
    let (_, body) = call(
        &app,
        "POST",
        "/v1/g/query",
        Some(json!({ "query": "MATCH (p:Person) WHERE p.community IS NOT NULL RETURN count(p)" })),
    )
    .await;
    assert_eq!(body["rows"], json!([[3]]));

    for (uri, input) in [
        ("/v1/g/algo/nope", json!({})),
        ("/v1/g/algo/pageRank", json!({ "write": "bad name" })),
        ("/v1/g/algo/shortestPath", json!({ "write": "x" })),
        (
            "/v1/g/algo/pageRank",
            json!({ "settings": { "dampng": 1 } }),
        ),
    ] {
        let (status, body) = call(&app, "POST", uri, Some(input)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
}

#[tokio::test]
async fn namespaces_are_created_and_dropped() {
    let app = app(None);
    let (status, created) = call(
        &app,
        "POST",
        "/v1/namespaces",
        Some(json!({ "name": "demo", "description": "First try" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["name"], "demo");
    assert_eq!(created["nodes"], 0);
    let (status, _) = call(
        &app,
        "POST",
        "/v1/namespaces",
        Some(json!({ "name": "demo2" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // Both stay listed while empty.
    let (_, list) = call(&app, "GET", "/v1/namespaces", None).await;
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["demo", "demo2"]);
    // Creating it again is a conflict, not a silent switch.
    let (status, body) = call(
        &app,
        "POST",
        "/v1/namespaces",
        Some(json!({ "name": "demo" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "conflict");
    let (status, _) = call(
        &app,
        "POST",
        "/v1/namespaces",
        Some(json!({ "name": "a/b" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, info) = call(
        &app,
        "PATCH",
        "/v1/namespaces/demo2",
        Some(json!({ "description": "Second" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["description"], "Second");
    let (_, info) = call(&app, "GET", "/v1/namespaces/demo", None).await;
    assert_eq!(info["description"], "First try");

    call(
        &app,
        "POST",
        "/v1/demo/nodes",
        Some(json!({ "labels": ["Note"] })),
    )
    .await;
    let (_, shown) = call(
        &app,
        "POST",
        "/v1/demo/query",
        Some(json!({ "query": "SHOW NAMESPACES" })),
    )
    .await;
    assert_eq!(
        shown["columns"],
        json!(["name", "nodes", "edges", "created_at", "description"])
    );
    assert_eq!(shown["rows"][0][1], 1);

    let (status, dropped) = call(&app, "DELETE", "/v1/namespaces/demo", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dropped["nodes"], 1);
    let (status, _) = call(&app, "GET", "/v1/namespaces/demo", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, "DELETE", "/v1/namespaces/demo", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn metrics_count_requests_writes_and_limits() {
    let app = router(
        AppState::new(Arc::new(Grafeno::in_memory().unwrap()), Some("secret")).with_limits(
            grafeno_query::Limits {
                max_rows: Some(10),
                ..grafeno_query::Limits::standard()
            },
        ),
    );
    let auth = |request: axum::http::request::Builder| {
        request.header(header::AUTHORIZATION, "Bearer secret")
    };
    for i in 0..3 {
        let request = auth(Request::builder().method("POST").uri("/v1/m/nodes"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "labels": ["Note"], "props": { "i": i } }).to_string(),
            ))
            .unwrap();
        assert!(
            app.clone()
                .oneshot(request)
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }
    let request = auth(Request::builder().method("POST").uri("/v1/m/query"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "query": "UNWIND range(1, 100) AS x RETURN x" }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    // Without a secret: refused, like the rest of /v1.
    let anonymous = Request::builder()
        .uri("/v1/metrics")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(anonymous).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let request = auth(Request::builder().uri("/v1/metrics"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    // Three nodes, and the audit event of the refused request (14.5).
    for line in [
        "grafeno_writes_total 4",
        "grafeno_write_groups_total",
        "grafeno_write_queue{lane=\"interactive\"} 0",
        "grafeno_queries_total 1",
        "grafeno_query_limits_total{limit=\"max_rows\"} 1",
        "grafeno_http_requests_total{status=\"4xx\"} 2",
        "grafeno_commit_seconds_bucket{le=\"+Inf\"}",
        "grafeno_write_seconds_count{lane=\"interactive\"} 4",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
}

#[tokio::test]
async fn entity_resolution_endpoints() {
    let app = app(None);
    let ops = json!({ "ops": [
        { "op": "create_node", "ref": "$john", "node": { "labels": ["Person"], "props": { "name": "John Smith", "born_on": "1980-03-12" } } },
        { "op": "create_node", "node": { "labels": ["Person"], "props": { "name": "Jane Smith", "born_on": "1982-07-01" } } },
        { "op": "create_node", "node": { "labels": ["Person"], "props": { "name": "Pedro López", "born_on": "1990-01-05" } } }
    ]});
    let (_, made) = call(&app, "POST", "/v1/er/batch", Some(ops)).await;
    let john = made["refs"]["$john"].clone();
    let (status, _) = call(
        &app,
        "POST",
        "/v1/er/query",
        Some(json!({ "query": "CREATE FUZZY INDEX FOR (p:Person) ON (p.name)" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, hits) = call(
        &app,
        "GET",
        "/v1/er/fuzzy?label=Person&key=name&q=jonh%20smith&limit=2",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert_eq!(hits[0]["node"], john);

    let rule = json!({
        "strong_keys": ["email"],
        "fields": [
            { "key": "name", "measure": "jaro_winkler", "weight": 2 },
            { "key": "born_on", "measure": "date", "days": 365 }
        ],
        "same": 0.9,
        "review": 0.75
    });
    let (status, stored) = call(&app, "PUT", "/v1/er/match-rules/Person", Some(rule)).await;
    assert_eq!(status, StatusCode::OK, "{stored}");
    assert_eq!(stored["label"], "Person");
    let (_, rules) = call(&app, "GET", "/v1/er/match-rules", None).await;
    assert_eq!(rules.as_array().unwrap().len(), 1);

    let (status, found) = call(
        &app,
        "POST",
        "/v1/er/match",
        Some(
            json!({ "label": "Person", "props": { "name": "Jon Smyth", "born_on": "1980-03-12" } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found[0]["node"], john);
    assert_eq!(found[0]["decision"], "same");
    assert_eq!(found[0]["fields"][0]["measure"], "jaro_winkler");

    let (status, query) = call(
        &app,
        "POST",
        "/v1/er/query",
        Some(json!({ "query": "CALL db.match('Person', {name: 'Jon Smyth', born_on: '1980-03-12'}, 1) YIELD node, decision RETURN node.name, decision" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{query}");
    assert_eq!(query["rows"][0], json!(["John Smith", "same"]));

    let (status, pairs) = call(&app, "GET", "/v1/er/matches?label=Person", None).await;
    assert_eq!(status, StatusCode::OK, "{pairs}");
    assert!(
        pairs.as_array().unwrap().is_empty(),
        "no duplicates yet: {pairs}"
    );
    let (status, _) = call(&app, "DELETE", "/v1/er/match-rules/Person", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        "POST",
        "/v1/er/match",
        Some(json!({ "label": "Person", "props": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn extraction_endpoints() {
    let app = app(None);
    let ops = json!({ "ops": [
        { "op": "create_node", "ref": "$ana", "node": { "labels": ["Person"], "props": { "name": "Ana García" } } },
        { "op": "create_node", "ref": "$j1", "node": { "labels": ["Person"], "props": { "name": "Juan Pérez" } } },
        { "op": "create_node", "ref": "$j2", "node": { "labels": ["Person"], "props": { "name": "Juan Pérez" } } },
        { "op": "create_node", "ref": "$m1", "node": { "labels": ["Memory"], "text": "Ana García habló con Juan Pérez sobre #ventas." } },
        { "op": "create_node", "ref": "$m2", "node": { "labels": ["Memory"], "text": "Reunión con Ana García el 2026-06-01." } }
    ]});
    let (_, made) = call(&app, "POST", "/v1/ex/batch", Some(ops)).await;
    let refs = |name: &str| made["refs"][name].as_str().unwrap().to_owned();
    let (status, report) = call(&app, "POST", "/v1/ex/extract", None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["read"], 2);
    assert_eq!(report["queued"], 1, "Juan Pérez is two people: {report}");
    let (status, again) = call(&app, "POST", "/v1/ex/extract", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (again["read"].clone(), again["unchanged"].clone()),
        (json!(0), json!(2))
    );

    let (status, queue) = call(&app, "GET", "/v1/ex/review", None).await;
    assert_eq!(status, StatusCode::OK, "{queue}");
    assert_eq!(queue[0]["kind"], "ambiguous");
    let key = queue[0]["key"].as_str().unwrap().replace(' ', "%20");
    let (status, settled) = call(
        &app,
        "POST",
        &format!("/v1/ex/review/{key}"),
        Some(json!({ "action": "link", "node": refs("$j2") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settled}");
    assert_eq!(settled["node"], refs("$j2"));
    let (_, queue) = call(&app, "GET", "/v1/ex/review", None).await;
    assert!(queue.as_array().unwrap().is_empty());

    // An agent's own mentions.
    let (status, ingested) = call(
        &app,
        "POST",
        &format!("/v1/ex/nodes/{}/mentions", refs("$m2")),
        Some(json!({ "mentions": [
            { "surface": "Ana García", "label": "Person" },
            { "surface": "Banco Central", "label": "Organization" }
        ] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ingested}");
    assert_eq!(
        (ingested["linked"].clone(), ingested["created"].clone()),
        (json!(1), json!(1))
    );
    let (_, ana) = call(
        &app,
        "GET",
        &format!("/v1/ex/nodes/{}/neighbors?direction=in", refs("$ana")),
        None,
    )
    .await;
    assert!(ana.as_array().unwrap().len() >= 2, "{ana}");
}

#[tokio::test]
async fn constraint_endpoints() {
    let app = app(None);
    let q = |query: &str| json!({ "query": query });
    for query in [
        "CREATE (:Person {name: 'Ana', city: 'Lima'}), (:Company {name: 'Acme'}), (:Company {name: 'Globex'})",
        "CREATE CONSTRAINT FOR ()-[r:works_at]->() REQUIRE ONE PER SOURCE",
        "CREATE CONSTRAINT FOR (p:Person) REQUIRE p.city IS FUNCTIONAL",
        "MATCH (p:Person), (c:Company {name: 'Acme'}) CREATE (p)-[:works_at]->(c)",
        "MATCH (p:Person), (c:Company {name: 'Globex'}) CREATE (p)-[:works_at]->(c)",
        "MATCH (p:Person) SET p.city = 'Quito'",
    ] {
        let (status, body) = call(&app, "POST", "/v1/k/query", Some(q(query))).await;
        assert_eq!(status, StatusCode::OK, "{query}: {body}");
    }
    let (status, constraints) = call(&app, "GET", "/v1/k/constraints", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(constraints.as_array().unwrap().len(), 2, "{constraints}");
    let (_, events) = call(&app, "GET", "/v1/k/contradictions", None).await;
    assert_eq!(events[0]["action"], "invalidated", "{events}");
    assert!(events[0]["replacement"].is_string());
    let (_, found) = call(
        &app,
        "POST",
        "/v1/k/query",
        Some(q("MATCH (p:Person) RETURN p.@id")),
    )
    .await;
    let ana = found["rows"][0][0].as_str().unwrap().to_owned();
    let (status, history) = call(
        &app,
        "GET",
        &format!("/v1/k/nodes/{ana}/history/city"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history[0]["value"], "Lima");
}

#[tokio::test]
async fn community_endpoints() {
    let app = app(None);
    let (status, body) = call(
        &app,
        "POST",
        "/v1/k/query",
        Some(json!({ "query": "CREATE (a:Person {name: 'Ana'}), (b:Person {name: 'Bruno'}), (c:Person {name: 'Carla'}), (d:Person {name: 'Dario'}), \
         (a)<-[:works_with]-(b), (a)<-[:works_with]-(c), (a)<-[:works_with]-(d), (b)<-[:works_with]-(c), (b)<-[:works_with]-(d), (c)<-[:works_with]-(d), \
         (e:Person {name: 'Eva'}), (f:Person {name: 'Fabio'}), (g:Person {name: 'Gala'}), (h:Person {name: 'Hugo'}), \
         (e)<-[:works_with]-(f), (e)<-[:works_with]-(g), (e)<-[:works_with]-(h), (f)<-[:works_with]-(g), (f)<-[:works_with]-(h), (g)<-[:works_with]-(h), \
         (b)-[:knows]->(f)" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, report) = call(&app, "POST", "/v1/k/communities/refresh", None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["created"], 2, "{report}");
    let (status, again) = call(
        &app,
        "POST",
        "/v1/k/communities/refresh",
        Some(json!({ "min_size": 3 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["unchanged"], 2, "{again}");

    let (status, hits) = call(&app, "GET", "/v1/k/communities?q=Ana", None).await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert!(hits[0]["text"].as_str().unwrap().contains("Ana"), "{hits}");
    let id = hits[0]["id"].as_str().unwrap().to_owned();
    let (status, opened) = call(
        &app,
        "GET",
        &format!("/v1/k/communities/{id}?limit=2"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{opened}");
    assert_eq!(opened["members"].as_array().unwrap().len(), 2);
    assert_eq!(opened["community"]["size"], 4);

    let (status, _) = call(
        &app,
        "PUT",
        &format!("/v1/k/communities/{id}/text"),
        Some(json!({ "text": "The compiler team." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, top) = call(&app, "GET", "/v1/k/communities", None).await;
    let custom = top
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["id"] == id.as_str())
        .unwrap();
    assert_eq!(custom["text"], "The compiler team.");
    assert_eq!(custom["custom"], true);
}

async fn raw(app: &Router, method: &str, uri: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (
        status,
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

#[tokio::test]
async fn csv_and_graphml_endpoints() {
    let app = app(None);
    let (status, body) = call(
        &app,
        "POST",
        "/v1/k/query",
        Some(json!({ "query": "CREATE (a:Person {name: 'Ana'})-[:knows {since: 2020}]->(b:Person {name: 'Bob'})" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, graphml) = raw(&app, "GET", "/v1/k/export.graphml", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&graphml).contains("<graphml"));
    let (status, report) = raw(&app, "POST", "/v1/copy/import/graphml", graphml).await;
    assert_eq!(status, StatusCode::OK);
    let report: Value = serde_json::from_slice(&report).unwrap();
    assert_eq!(
        (report["nodes"].as_u64(), report["edges"].as_u64()),
        (Some(2), Some(1))
    );

    let (_, nodes) = raw(&app, "GET", "/v1/k/export/nodes.csv", Vec::new()).await;
    let (_, edges) = raw(&app, "GET", "/v1/k/export/edges.csv", Vec::new()).await;
    assert!(String::from_utf8_lossy(&edges).contains("since:int"));
    raw(&app, "POST", "/v1/csv/import/nodes.csv", nodes).await;
    let (status, report) = raw(&app, "POST", "/v1/csv/import/edges.csv", edges).await;
    assert_eq!(status, StatusCode::OK);
    let report: Value = serde_json::from_slice(&report).unwrap();
    assert_eq!(report["edges"], 1, "{report}");
    let (_, found) = call(
        &app,
        "POST",
        "/v1/csv/query",
        Some(json!({ "query": "MATCH (a)-[r:knows]->(b) RETURN a.name, r.since, b.name" })),
    )
    .await;
    assert_eq!(found["rows"], json!([["Ana", 2020, "Bob"]]));
}

#[tokio::test]
async fn read_many_keeps_positions_text_and_memory_like_individual_reads() {
    let app = app(None);
    let (_, a) = call(
        &app,
        "POST",
        "/v1/lab/nodes",
        Some(json!({"labels":["Person"],"text":"Memoria de Ana"})),
    )
    .await;
    let (_, b) = call(
        &app,
        "POST",
        "/v1/lab/nodes",
        Some(json!({"labels":["Person"]})),
    )
    .await;
    let (_, edge) = call(
        &app,
        "POST",
        "/v1/lab/edges",
        Some(json!({"from":id(&a),"to":id(&b),"label":"knows"})),
    )
    .await;
    let (_, full) = call(&app, "GET", &format!("/v1/lab/nodes/{}", id(&a)), None).await;
    let missing = grafeno::NodeId::new().to_string();
    let (status, result) = call(
        &app,
        "POST",
        "/v1/lab/read-many",
        Some(json!({"nodes":[id(&a),missing,id(&a)],"edges":[id(&edge)]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["nodes"], json!([full, null, full]));
    let (_, full_edge) = call(&app, "GET", &format!("/v1/lab/edges/{}", id(&edge)), None).await;
    assert_eq!(result["edges"], json!([full_edge]));
    let (_, other) = call(
        &app,
        "POST",
        "/v1/otro/read-many",
        Some(json!({"nodes":[id(&a)],"edges":[id(&edge)]})),
    )
    .await;
    assert_eq!(other, json!({"nodes":[null],"edges":[null]}));
    let (_, empty) = call(&app, "POST", "/v1/lab/read-many", Some(json!({}))).await;
    assert_eq!(empty, json!({"nodes":[],"edges":[]}));
}

#[tokio::test]
async fn read_many_rejects_bad_ids_unknown_fields_and_oversized_combined_lists() {
    let app = app(None);
    let node = grafeno::NodeId::new().to_string();
    let edge = grafeno::EdgeId::new().to_string();
    let (status, result) = call(
        &app,
        "POST",
        "/v1/lab/read-many",
        Some(json!({"nodes":vec![&node;511],"edges":[edge]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["nodes"].as_array().unwrap().len(), 511);
    for input in [
        json!({"nodes":vec![&node;512],"edges":[edge]}),
        json!({"nodes":["invalid"]}),
        json!({"edges":["invalid"]}),
        json!({"unknown":[]}),
    ] {
        let (status, result) = call(&app, "POST", "/v1/lab/read-many", Some(input)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    }
}

#[tokio::test]
async fn neighborhoods_match_individual_subgraphs_with_directions_and_filters() {
    let app = app(None);
    let (_, a) = call(
        &app,
        "POST",
        "/v1/lab/nodes",
        Some(json!({"labels":["Person"]})),
    )
    .await;
    let (_, b) = call(
        &app,
        "POST",
        "/v1/lab/nodes",
        Some(json!({"labels":["Person"]})),
    )
    .await;
    let (a, b) = (id(&a), id(&b));
    call(&app, "POST", "/v1/lab/edges", Some(json!({"from":a,"to":b,"label":"knows","valid_from":"2020-01-01","valid_to":"2027-01-01"}))).await;
    for direction in ["out", "in", "both"] {
        for filter in [
            json!({}),
            json!({"valid_at":"2026-06-01"}),
            json!({"valid_at":"2030-01-01"}),
            json!({"known_at":"2019-01-01"}),
            json!({"labels":["other"]}),
        ] {
            let (status, actual) = call(
                &app,
                "POST",
                "/v1/lab/neighborhoods",
                Some(json!({"seeds":[a,b,a],"direction":direction,"filter":filter})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{actual}");
            let mut expected = Vec::new();
            for seed in [&a, &b, &a] {
                let (status, graph) = call(
                    &app,
                    "POST",
                    "/v1/lab/subgraph",
                    Some(json!({"seeds":[seed],"depth":1,"direction":direction,"filter":filter})),
                )
                .await;
                assert_eq!(status, StatusCode::OK);
                expected.push(graph);
            }
            assert_eq!(actual, json!(expected), "{direction} {filter}");
        }
    }
    let (status, _) = call(
        &app,
        "POST",
        "/v1/otro/neighborhoods",
        Some(json!({"seeds":[a]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn neighborhoods_validate_root_limits_ids_and_options() {
    let app = app(None);
    let (_, root) = call(
        &app,
        "POST",
        "/v1/lab/nodes",
        Some(json!({"labels":["Person"]})),
    )
    .await;
    let root = id(&root);
    let (status, result) = call(
        &app,
        "POST",
        "/v1/lab/neighborhoods",
        Some(json!({"seeds":vec![&root;8]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result.as_array().unwrap().len(), 8);
    let (_, empty) = call(
        &app,
        "POST",
        "/v1/lab/neighborhoods",
        Some(json!({"seeds":[]})),
    )
    .await;
    assert_eq!(empty, json!([]));
    for input in [
        json!({"seeds":vec![&root;9]}),
        json!({"seeds":["invalid"]}),
        json!({"seeds":[root],"depth":2}),
        json!({"seeds":[root],"filter":{"valid_at":"invalid"}}),
    ] {
        let (status, result) = call(&app, "POST", "/v1/lab/neighborhoods", Some(input)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    }
}

#[tokio::test]
async fn nulls_are_stored_and_removed_by_name() {
    let app = app(None);
    let (_, about) = call(&app, "GET", "/v1/about", None).await;
    assert!(
        about["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("null_values"))
    );
    let (status, created) = call(
        &app,
        "POST",
        "/v1/k/nodes",
        Some(json!({ "labels": ["P"], "props": { "a": 1, "b": null } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = id(&created);
    let (status, _) = call(
        &app,
        "PATCH",
        &format!("/v1/k/nodes/{id}"),
        Some(json!({ "props": { "a": null, "c": 3 }, "remove_props": ["b"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, node) = call(&app, "GET", &format!("/v1/k/nodes/{id}"), None).await;
    let props = if node.get("props").is_some() {
        &node["props"]
    } else {
        &node["node"]["props"]
    };
    assert_eq!(props, &json!({ "a": null, "c": 3 }), "{node}");
}

/// What an interface reads over the API: a palace is ordinary nodes and
/// edges, so it comes back from the subgraph and query routes as any
/// other part of the graph, with labels that tell its pieces apart.
#[tokio::test]
async fn a_palace_is_read_as_ordinary_nodes_and_edges() {
    use grafeno_memory::{NewRoom, anchor, create_palace, create_room, find_locus};

    let db = Arc::new(Grafeno::in_memory().unwrap());
    let (palace, room, note) = db
        .write(|tx| {
            let palace = create_palace(tx, "g", "Work", Some("Projects"))?.id;
            let room = create_room(tx, "g", palace, NewRoom::new("Storage").locus("Decisions"))?.id;
            let note = tx.create_node(
                "g",
                grafeno::NewNode::new("Note")
                    .prop("title", "One writer")
                    .text("Only one transaction writes at a time"),
            )?;
            Ok((palace, room, note))
        })
        .unwrap();
    let decisions = find_locus(&db.begin_read().unwrap(), "g", room, "Decisions")
        .unwrap()
        .unwrap();
    db.write(|tx| anchor(tx, "g", note, decisions, Some("why")))
        .unwrap();
    let app = router(AppState::new(db, None));

    let (status, graph) = call(
        &app,
        "POST",
        "/v1/g/subgraph",
        Some(json!({ "seeds": [palace], "depth": 3 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{graph}");
    let mut nodes: Vec<(String, String)> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| {
            (
                node["labels"][0].as_str().unwrap().to_owned(),
                node["props"]["name"]
                    .as_str()
                    .or(node["props"]["title"].as_str())
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect();
    nodes.sort();
    assert_eq!(
        nodes,
        [
            ("Locus".to_owned(), "Decisions".to_owned()),
            ("Locus".to_owned(), "entrance".to_owned()),
            ("Note".to_owned(), "One writer".to_owned()),
            ("Palace".to_owned(), "Work".to_owned()),
            ("Room".to_owned(), "Storage".to_owned()),
        ]
    );
    let mut edges: Vec<&str> = graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| edge["label"].as_str().unwrap())
        .collect();
    edges.sort();
    assert_eq!(edges, ["anchored_at", "has_locus", "has_locus", "has_room"]);
    let anchor = graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["label"] == "anchored_at")
        .unwrap();
    assert_eq!(anchor["from"], note.to_string());
    assert_eq!(anchor["props"]["note"], "why");

    // And by a query, as a path from the palace to the memory.
    let (status, body) = call(
        &app,
        "POST",
        "/v1/g/query",
        Some(json!({ "query": "MATCH (p:Palace)-[:has_room]->(r:Room)-[:has_locus]->(l:Locus)<-[:anchored_at]-(m) RETURN p.name, r.name, l.name, m.title" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["rows"],
        json!([["Work", "Storage", "Decisions", "One writer"]])
    );
}

/// Found by an outside review: an ingested line held 40 MiB, past every
/// body limit, and a vector search with a limit of 10^12 reserved for that
/// many results.
#[tokio::test]
async fn ingest_lines_and_search_limits_are_bounded() {
    let app = app(None);
    // A line too long is refused; the lines before it are written.
    let mut body = String::from("{\"labels\": [\"Before\"]}\n");
    body.push_str(&format!(
        "{{\"labels\": [\"Long\"], \"props\": {{\"text\": \"{}\"}}}}",
        "x".repeat(5 * 1024 * 1024)
    ));
    let request = Request::builder()
        .method("POST")
        .uri("/v1/g/ingest")
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let answer: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(answer.to_string().contains("line 2"), "{answer}");
    // A line of a normal size goes in as before.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/g/ingest")
        .body(Body::from(format!(
            "{{\"labels\": [\"Fine\"], \"props\": {{\"text\": \"{}\"}}}}\n",
            "y".repeat(1024 * 1024)
        )))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // A vector search with a limit of 10^12, over vectors that exist.
    for i in 0..5 {
        let (status, _) = call(
            &app,
            "POST",
            "/v1/g/nodes",
            Some(json!({ "labels": ["V"], "embedding": [1.0, i as f32, 0.5] })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, found) = call(
        &app,
        "POST",
        "/v1/g/search/vector",
        Some(json!({ "embedding": [1.0, 2.0, 0.5], "limit": 1_000_000_000_000u64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found.as_array().map(Vec::len), Some(5), "{found}");
}
