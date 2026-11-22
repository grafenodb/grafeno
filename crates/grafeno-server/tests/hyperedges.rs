//! Hyperedges over HTTP and MCP: the routes' lifecycle (create, read,
//! list, find, patch, invalidate, retract, delete), deleting a participant
//! node, namespaces and roles, GQ through the query route, and the
//! `relate_group`, `find_group` and `forget_group` tools.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
use grafeno_server::mcp::{McpAccess, McpServer};
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn node(app: &Router, ns: &str, name: &str) -> String {
    let (status, body) = call(
        app,
        OWNER,
        "POST",
        &format!("/v1/{ns}/nodes"),
        Some(json!({ "labels": ["Agent"], "props": { "name": name } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

async fn key(app: &Router, name: &str, grants: Value) -> String {
    let (status, body) = call(
        app,
        OWNER,
        "POST",
        "/v1/keys",
        Some(json!({ "name": name, "grants": grants })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["secret"].as_str().unwrap().to_owned()
}

fn app() -> Router {
    router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ))
}

#[tokio::test]
async fn the_routes_lifecycle() {
    let app = app();
    let (a, b, c) = (
        node(&app, "t", "planner").await,
        node(&app, "t", "builder").await,
        node(&app, "t", "checker").await,
    );
    let (status, body) = call(
        &app,
        OWNER,
        "POST",
        "/v1/t/hyperedges",
        Some(json!({
            "label": "decision",
            "participants": [
                { "node": a, "role": "proposer" },
                { "node": b, "role": "implementer" },
                { "node": c }
            ],
            "props": { "summary": "single writer" },
            "valid_from": "2026-01-01"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();

    let (status, edge) = call(&app, OWNER, "GET", &format!("/v1/t/hyperedges/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edge["label"], "decision");
    assert_eq!(edge["participants"].as_array().unwrap().len(), 3);
    assert_eq!(edge["props"]["summary"], "single writer");

    let (_, page) = call(&app, OWNER, "GET", "/v1/t/hyperedges?label=decision", None).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let (_, of_c) = call(
        &app,
        OWNER,
        "GET",
        &format!("/v1/t/nodes/{c}/hyperedges"),
        None,
    )
    .await;
    assert_eq!(of_c[0]["id"], id.as_str());
    let (_, found) = call(
        &app,
        OWNER,
        "POST",
        "/v1/t/hyperedges/find",
        Some(json!({ "nodes": [a, b] })),
    )
    .await;
    assert_eq!(found.as_array().unwrap().len(), 1);
    let (_, exact) = call(
        &app,
        OWNER,
        "POST",
        "/v1/t/hyperedges/find",
        Some(json!({ "nodes": [a, b], "exact": true })),
    )
    .await;
    assert!(exact.as_array().unwrap().is_empty());

    let (status, patched) = call(
        &app,
        OWNER,
        "PATCH",
        &format!("/v1/t/hyperedges/{id}"),
        Some(json!({
            "props": { "status": "done" },
            "remove_props": ["summary"],
            "participants": [{ "node": a, "role": "proposer" }, { "node": b }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["props"], json!({ "status": "done" }));
    assert_eq!(patched["participants"].as_array().unwrap().len(), 2);
    let (_, of_c) = call(
        &app,
        OWNER,
        "GET",
        &format!("/v1/t/nodes/{c}/hyperedges"),
        None,
    )
    .await;
    assert!(of_c.as_array().unwrap().is_empty());

    // Malformed: one node, an unknown field.
    let (status, _) = call(
        &app,
        OWNER,
        "POST",
        "/v1/t/hyperedges",
        Some(json!({ "label": "x", "participants": [{ "node": a }] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &app,
        OWNER,
        "POST",
        "/v1/t/hyperedges",
        Some(json!({ "label": "x", "participants": [{ "node": a }, { "node": b }], "weight": 1 })),
    )
    .await;
    assert!(status.is_client_error());

    let (status, invalidated) = call(
        &app,
        OWNER,
        "POST",
        &format!("/v1/t/hyperedges/{id}/invalidate"),
        Some(json!({ "at": "2026-06-01" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(invalidated["valid_to"].is_string() || invalidated["valid_to"].is_number());
    let (_, then) = call(
        &app,
        OWNER,
        "GET",
        &format!("/v1/t/nodes/{a}/hyperedges?valid_at=2026-12-01"),
        None,
    )
    .await;
    assert!(then.as_array().unwrap().is_empty());
    let (status, retracted) = call(
        &app,
        OWNER,
        "POST",
        &format!("/v1/t/hyperedges/{id}/retract"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!retracted["expired_at"].is_null());

    let (status, _) = call(
        &app,
        OWNER,
        "DELETE",
        &format!("/v1/t/hyperedges/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(&app, OWNER, "GET", &format!("/v1/t/hyperedges/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn deleting_a_participant_says_what_happens_to_its_hyperedges() {
    let app = app();
    let (a, b, c) = (
        node(&app, "t", "a").await,
        node(&app, "t", "b").await,
        node(&app, "t", "c").await,
    );
    let (_, body) = call(
        &app,
        OWNER,
        "POST",
        "/v1/t/hyperedges",
        Some(
            json!({ "label": "x", "participants": [{ "node": a }, { "node": b }, { "node": c }] }),
        ),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(&app, OWNER, "DELETE", &format!("/v1/t/nodes/{a}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = call(
        &app,
        OWNER,
        "DELETE",
        &format!("/v1/t/nodes/{a}?hyperedges=sideways"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &app,
        OWNER,
        "DELETE",
        &format!("/v1/t/nodes/{a}?hyperedges=detach"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, edge) = call(&app, OWNER, "GET", &format!("/v1/t/hyperedges/{id}"), None).await;
    assert_eq!(edge["participants"].as_array().unwrap().len(), 2);
    let (status, _) = call(
        &app,
        OWNER,
        "DELETE",
        &format!("/v1/t/nodes/{b}?hyperedges=cascade"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, OWNER, "GET", &format!("/v1/t/hyperedges/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn namespaces_and_roles_apply() {
    let app = app();
    let (a, b) = (node(&app, "a", "one").await, node(&app, "a", "two").await);
    let (_, body) = call(
        &app,
        OWNER,
        "POST",
        "/v1/a/hyperedges",
        Some(json!({ "label": "x", "participants": [{ "node": a }, { "node": b }] })),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_owned();
    let reader = key(
        &app,
        "reader",
        json!([{ "namespace": "a", "role": "read" }]),
    )
    .await;
    let outsider = key(
        &app,
        "outsider",
        json!([{ "namespace": "b", "role": "write" }]),
    )
    .await;

    // A reader reads, finds, and changes nothing.
    let (status, _) = call(
        &app,
        &reader,
        "GET",
        &format!("/v1/a/hyperedges/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        &reader,
        "POST",
        "/v1/a/hyperedges/find",
        Some(json!({ "nodes": [a] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for (method, uri, body) in [
        (
            "POST",
            "/v1/a/hyperedges".to_owned(),
            Some(json!({ "label": "x", "participants": [{ "node": a }, { "node": b }] })),
        ),
        (
            "PATCH",
            format!("/v1/a/hyperedges/{id}"),
            Some(json!({ "props": { "k": 1 } })),
        ),
        ("DELETE", format!("/v1/a/hyperedges/{id}"), None),
        ("POST", format!("/v1/a/hyperedges/{id}/retract"), None),
    ] {
        let (status, _) = call(&app, &reader, method, &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    // Another namespace's key reaches nothing, and its own namespace has
    // no such hyperedge.
    let (status, _) = call(
        &app,
        &outsider,
        "GET",
        &format!("/v1/a/hyperedges/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &app,
        &outsider,
        "GET",
        &format!("/v1/b/hyperedges/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        &outsider,
        "POST",
        "/v1/b/hyperedges",
        Some(json!({ "label": "x", "participants": [{ "node": a }, { "node": b }] })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // GQ through the query route, as the reader.
    let (status, rows) = call(
        &app,
        &reader,
        "POST",
        "/v1/a/query",
        Some(json!({ "query": "MATCH HYPEREDGE h:x [(n)] RETURN count(n)" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows["rows"], json!([[2]]));
    let (status, _) = call(
        &app,
        &reader,
        "POST",
        "/v1/a/query",
        Some(json!({ "query": "MATCH HYPEREDGE h:x DELETE h" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, about) = call(&app, OWNER, "GET", "/v1/about", None).await;
    assert!(about.to_string().contains("\"hyperedges\""), "{about}");
}

fn tool(server: &McpServer, name: &str, arguments: Value) -> Value {
    server.call(name, arguments).unwrap()
}

#[test]
fn the_mcp_tools() {
    let server = McpServer::new(Grafeno::in_memory().unwrap(), "agent".into());
    let mut ids = Vec::new();
    for text in ["planner agent", "builder agent", "task one"] {
        let out = tool(&server, "remember", json!({ "text": text, "name": text }));
        assert_eq!(out["isError"], false, "{out}");
        ids.push(out["structuredContent"]["id"].as_str().unwrap().to_owned());
    }
    let made = tool(
        &server,
        "relate_group",
        json!({
            "label": "technical_decision",
            "participants": [
                { "node": ids[0], "role": "proposer" },
                { "node": ids[1], "role": "implementer" },
                { "node": ids[2], "role": "subject" }
            ],
            "props": { "summary": "keep one writer" }
        }),
    );
    assert_eq!(made["isError"], false, "{made}");
    let id = made["structuredContent"]["id"].as_str().unwrap().to_owned();

    let by_node = tool(&server, "find_group", json!({ "node": ids[2] }));
    let groups = &by_node["structuredContent"]["groups"];
    assert_eq!(groups.as_array().unwrap().len(), 1, "{by_node}");
    let roles: Vec<&str> = groups[0]["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["role"].as_str().unwrap())
        .collect();
    assert!(roles.contains(&"proposer") && roles.contains(&"subject"));
    assert!(
        groups[0]["participants"]
            .to_string()
            .contains("planner agent")
    );
    let exact = tool(
        &server,
        "find_group",
        json!({ "nodes": [ids[0], ids[1]], "exact": true }),
    );
    assert!(
        exact["structuredContent"]["groups"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let both = tool(&server, "find_group", json!({ "id": id, "node": ids[0] }));
    assert_eq!(both["isError"], true);
    let one = tool(
        &server,
        "relate_group",
        json!({ "label": "x", "participants": [{ "node": ids[0] }] }),
    );
    assert_eq!(one["isError"], true);

    let gone = tool(
        &server,
        "forget_group",
        json!({ "id": id, "mode": "retract" }),
    );
    assert_eq!(gone["isError"], false, "{gone}");
    let after = tool(&server, "find_group", json!({ "node": ids[2] }));
    assert!(
        after["structuredContent"]["groups"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let deleted = tool(
        &server,
        "forget_group",
        json!({ "id": id, "mode": "delete" }),
    );
    assert_eq!(deleted["isError"], false);
    let read = tool(&server, "find_group", json!({ "id": id }));
    assert!(
        read["structuredContent"]["groups"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // A read-only server finds, and refuses the writing tools.
    let reader =
        McpServer::new(Grafeno::in_memory().unwrap(), "agent".into()).with_access(McpAccess {
            read_only: true,
            ..McpAccess::default()
        });
    let refused = tool(
        &reader,
        "relate_group",
        json!({ "label": "x", "participants": [] }),
    );
    assert_eq!(refused["isError"], true);
    assert!(refused.to_string().contains("read-only"));
    let refused = tool(&reader, "forget_group", json!({ "id": id }));
    assert_eq!(refused["isError"], true);
}

async fn batch(app: &Router, key: &str, ns: &str, ops: Value) -> (StatusCode, Value) {
    call(
        app,
        key,
        "POST",
        &format!("/v1/{ns}/batch"),
        Some(json!({ "ops": ops })),
    )
    .await
}

async fn count(app: &Router, ns: &str, what: &str) -> usize {
    let (status, body) = call(app, OWNER, "GET", &format!("/v1/{ns}/{what}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["items"].as_array().unwrap().len()
}

fn message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap()
}

#[tokio::test]
async fn a_batch_writes_nodes_and_hyperedges_together() {
    let app = app();
    let writer = key(
        &app,
        "writer",
        json!([{ "namespace": "b", "role": "write" }]),
    )
    .await;
    let (status, body) = batch(
        &app,
        &writer,
        "b",
        json!([
            { "op": "create_node", "ref": "$ana", "node": { "labels": ["Agent"] } },
            { "op": "create_node", "ref": "$bo", "node": { "labels": ["Agent"] } },
            { "op": "create_node", "ref": "$task", "node": { "labels": ["Task"] } },
            { "op": "create_hyperedge", "ref": "$decision", "hyperedge": {
                "label": "technical_decision",
                "participants": [
                    { "node": "$ana", "role": "proposer" },
                    { "node": "$bo", "role": "implementer" },
                    { "node": "$task", "role": "subject" }
                ],
                "props": { "status": "proposed" } } },
            { "op": "update_hyperedge", "id": "$decision",
              "patch": { "props": { "status": "approved" }, "remove_props": [] } },
            { "op": "create_edge", "edge": { "from": "$ana", "label": "owns", "to": "$task" } }
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["hyperedge_refs"]["$decision"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(body["results"][3]["id"], id.as_str());
    assert_eq!(body["refs"].as_object().unwrap().len(), 3);

    let (_, edge) = call(&app, OWNER, "GET", &format!("/v1/b/hyperedges/{id}"), None).await;
    assert_eq!(edge["props"]["status"], "approved");
    assert_eq!(edge["participants"].as_array().unwrap().len(), 3);
    // Written as its caller.
    assert_eq!(edge["recorded_by"], "writer", "{edge}");

    // Participants replaced, then the hyperedge taken back and deleted,
    // each by id in later batches.
    let task = body["refs"]["$task"].as_str().unwrap().to_owned();
    let ana = body["refs"]["$ana"].as_str().unwrap().to_owned();
    let (status, changed) = batch(
        &app,
        &writer,
        "b",
        json!([
            { "op": "create_node", "ref": "$cy", "node": { "labels": ["Agent"] } },
            { "op": "update_hyperedge", "id": id, "patch": { "participants": [
                { "node": ana, "role": "proposer" },
                { "node": "$cy", "role": "reviewer" },
                { "node": task, "role": "subject" } ] } },
            { "op": "invalidate_hyperedge", "id": id, "at": "2030-01-01T00:00:00Z" }
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let (_, edge) = call(&app, OWNER, "GET", &format!("/v1/b/hyperedges/{id}"), None).await;
    let roles: Vec<&str> = edge["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["role"].as_str().unwrap())
        .collect();
    assert!(
        roles.contains(&"reviewer") && !roles.contains(&"implementer"),
        "{edge}"
    );
    assert!(edge["valid_to"].is_number(), "{edge}");

    let (status, gone) = batch(
        &app,
        &writer,
        "b",
        json!([
            { "op": "retract_hyperedge", "id": id },
            { "op": "delete_hyperedge", "id": id }
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{gone}");
    let (status, _) = call(&app, OWNER, "GET", &format!("/v1/b/hyperedges/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A batch without hyperedges answers as before: no `hyperedge_refs`.
    let (_, plain) = batch(
        &app,
        &writer,
        "b",
        json!([{ "op": "create_node", "node": { "labels": ["Note"] } }]),
    )
    .await;
    assert!(plain.get("hyperedge_refs").is_none(), "{plain}");
}

#[tokio::test]
async fn a_failing_batch_leaves_nothing_behind() {
    let app = app();
    node(&app, "f", "seed").await;
    let start = totals(&app).await;

    let cases = [
        // A hyperedge with one node.
        (
            json!([
                { "op": "create_node", "ref": "$a", "node": { "labels": ["A"] } },
                { "op": "create_hyperedge", "hyperedge": { "label": "x",
                  "participants": [{ "node": "$a" }, { "node": "$a" }] } }
            ]),
            "operation 1:",
        ),
        // A reference made later in the batch.
        (
            json!([
                { "op": "create_node", "ref": "$a", "node": { "labels": ["A"] } },
                { "op": "create_hyperedge", "hyperedge": { "label": "x",
                  "participants": [{ "node": "$a" }, { "node": "$b" }] } },
                { "op": "create_node", "ref": "$b", "node": { "labels": ["A"] } }
            ]),
            "unknown reference \"$b\"",
        ),
        // A node name used as a hyperedge, and the other way round.
        (
            json!([
                { "op": "create_node", "ref": "$a", "node": { "labels": ["A"] } },
                { "op": "retract_hyperedge", "id": "$a" }
            ]),
            "names a node, not a hyperedge",
        ),
        (
            json!([
                { "op": "create_node", "ref": "$a", "node": { "labels": ["A"] } },
                { "op": "create_node", "ref": "$b", "node": { "labels": ["A"] } },
                { "op": "create_hyperedge", "ref": "$h", "hyperedge": { "label": "x",
                  "participants": [{ "node": "$a" }, { "node": "$b" }] } },
                { "op": "update_node", "id": "$h", "patch": { "props": { "k": 1 } } }
            ]),
            "names a hyperedge, not a node",
        ),
        // One name for two records.
        (
            json!([
                { "op": "create_node", "ref": "$a", "node": { "labels": ["A"] } },
                { "op": "create_node", "ref": "$b", "node": { "labels": ["A"] } },
                { "op": "create_hyperedge", "ref": "$a", "hyperedge": { "label": "x",
                  "participants": [{ "node": "$a" }, { "node": "$b" }] } }
            ]),
            "already taken",
        ),
        // A node in a hyperedge is not deleted unless the batch says how.
        (
            json!([
                { "op": "create_node", "ref": "$a", "node": { "labels": ["A"] } },
                { "op": "create_node", "ref": "$b", "node": { "labels": ["A"] } },
                { "op": "create_hyperedge", "hyperedge": { "label": "x",
                  "participants": [{ "node": "$a" }, { "node": "$b" }] } },
                { "op": "delete_node", "id": "$a" }
            ]),
            "operation 3:",
        ),
        // An unknown hyperedge id.
        (
            json!([
                { "op": "create_node", "node": { "labels": ["A"] } },
                { "op": "delete_hyperedge", "id": "0190f5c2-7a3e-7cde-8f01-00000000dead" }
            ]),
            "operation 1:",
        ),
    ];
    for (ops, expected) in cases {
        let (status, body) = batch(&app, OWNER, "f", ops.clone()).await;
        assert!(status.is_client_error(), "{ops} -> {status} {body}");
        assert!(message(&body).contains(expected), "{ops} -> {body}");
        assert_eq!(totals(&app).await, start, "{ops} left records");
    }

    // With a policy, deleting a participant in the same batch works.
    let (status, body) = batch(
        &app,
        OWNER,
        "f",
        json!([
            { "op": "create_node", "ref": "$a", "node": { "labels": ["A"] } },
            { "op": "create_node", "ref": "$b", "node": { "labels": ["A"] } },
            { "op": "create_node", "ref": "$c", "node": { "labels": ["A"] } },
            { "op": "create_hyperedge", "ref": "$h", "hyperedge": { "label": "x",
              "participants": [{ "node": "$a" }, { "node": "$b" }, { "node": "$c" }] } },
            { "op": "delete_node", "id": "$a", "hyperedges": "detach" }
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["hyperedge_refs"]["$h"].as_str().unwrap();
    let (_, edge) = call(&app, OWNER, "GET", &format!("/v1/f/hyperedges/{id}"), None).await;
    assert_eq!(edge["participants"].as_array().unwrap().len(), 2);
    let (status, body) = batch(
        &app,
        OWNER,
        "f",
        json!([{ "op": "delete_node", "id": "$x", "hyperedges": "sideways" }]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_batch_keeps_to_its_callers_rights() {
    let app = app();
    let (a, b) = (node(&app, "r", "one").await, node(&app, "r", "two").await);
    let (_, made) = call(
        &app,
        OWNER,
        "POST",
        "/v1/r/hyperedges",
        Some(json!({ "label": "x", "participants": [{ "node": a }, { "node": b }] })),
    )
    .await;
    let id = made["id"].as_str().unwrap().to_owned();

    // Writing without taking back: no retract or invalidate in a batch.
    let (status, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/roles/no-retract",
        Some(json!({ "privileges": [
            { "action": "read", "object": { "on": "namespace", "namespace": "r" } },
            { "action": "create", "object": { "on": "namespace", "namespace": "r" } },
            { "action": "update", "object": { "on": "namespace", "namespace": "r" } },
            { "action": "delete", "object": { "on": "namespace", "namespace": "r" } }
        ] })),
    )
    .await;
    assert!(status.is_success());
    let writer = account_key(&app, "no-retract").await;
    for op in [
        json!({ "op": "retract_hyperedge", "id": id }),
        json!({ "op": "invalidate_hyperedge", "id": id, "at": "2030-01-01T00:00:00Z" }),
    ] {
        let (status, body) = batch(&app, &writer, "r", json!([op])).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let (status, body) = batch(
        &app,
        &writer,
        "r",
        json!([{ "op": "update_hyperedge", "id": id, "patch": { "props": { "k": 1 } } }]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A reader writes nothing.
    let reader = key(
        &app,
        "reader",
        json!([{ "namespace": "r", "role": "read" }]),
    )
    .await;
    let (status, _) = batch(
        &app,
        &reader,
        "r",
        json!([{ "op": "create_hyperedge", "hyperedge": { "label": "x",
                 "participants": [{ "node": a }, { "node": b }] } }]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Seeing only `Public` nodes: a hidden node is no participant, and the
    // error does not say whether it exists; nothing stays.
    let (_, hidden) = call(
        &app,
        OWNER,
        "POST",
        "/v1/r/nodes",
        Some(json!({ "labels": ["Secret"] })),
    )
    .await;
    let hidden = hidden["id"].as_str().unwrap().to_owned();
    let label = |action: &str| json!({ "action": action, "object": { "on": "label", "namespace": "r", "label": "Public" } });
    let (status, role) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/roles/public-only",
        Some(json!({ "privileges": [
            label("read"),
            label("write"),
            // Hyperedge labels are granted as edge labels.
            json!({ "action": "read", "object": { "on": "label", "namespace": "r", "label": "x", "edges": true } }),
            json!({ "action": "write", "object": { "on": "label", "namespace": "r", "label": "x", "edges": true } })
        ] })),
    )
    .await;
    assert!(status.is_success(), "{role}");
    let narrow = account_key(&app, "public-only").await;
    let nodes_before = count(&app, "r", "nodes").await;
    let (status, body) = batch(
        &app,
        &narrow,
        "r",
        json!([
            { "op": "create_node", "ref": "$p", "node": { "labels": ["Public"] } },
            { "op": "create_node", "ref": "$q", "node": { "labels": ["Public"] } },
            { "op": "create_hyperedge", "hyperedge": { "label": "x",
              "participants": [{ "node": "$p" }, { "node": "$q" }, { "node": hidden }] } }
        ]),
    )
    .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(!message(&body).contains("Secret"), "{body}");
    assert_eq!(count(&app, "r", "nodes").await, nodes_before);
    // Without the hidden node it goes through.
    let (status, body) = batch(
        &app,
        &narrow,
        "r",
        json!([
            { "op": "create_node", "ref": "$p", "node": { "labels": ["Public"] } },
            { "op": "create_node", "ref": "$q", "node": { "labels": ["Public"] } },
            { "op": "create_hyperedge", "hyperedge": { "label": "x",
              "participants": [{ "node": "$p" }, { "node": "$q" }] } }
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn account_key(app: &Router, role: &str) -> String {
    let (status, body) = call(
        app,
        OWNER,
        "POST",
        "/v1/accounts",
        Some(json!({ "name": format!("{role}-account"), "kind": "service", "roles": [role] })),
    )
    .await;
    assert!(status.is_success(), "{body}");
    let (status, body) = call(
        app,
        OWNER,
        "POST",
        "/v1/keys",
        Some(json!({ "name": format!("{role}-key"), "account": format!("{role}-account") })),
    )
    .await;
    assert!(status.is_success(), "{body}");
    body["secret"].as_str().unwrap().to_owned()
}

async fn totals(app: &Router) -> (usize, usize) {
    (
        count(app, "f", "nodes").await,
        count(app, "f", "hyperedges").await,
    )
}
