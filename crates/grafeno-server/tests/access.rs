//! Isolation between namespaces and principals: every route, called with
//! keys of the wrong namespace or the wrong role, is refused.

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

async fn key(app: &Router, name: &str, grants: Value, own_only: bool) -> String {
    let (status, body) = call(
        app,
        OWNER,
        "POST",
        "/v1/keys",
        Some(json!({ "name": name, "grants": grants, "own_only": own_only })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["secret"].as_str().unwrap().to_owned()
}

/// Two namespaces with two people, a text, an embedding and a fact each.
async fn world() -> (Router, Vec<(String, String, String)>) {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    let mut ids = Vec::new();
    for ns in ["a", "b"] {
        let (_, one) = call(
            &app,
            OWNER,
            "POST",
            &format!("/v1/{ns}/nodes"),
            Some(json!({ "labels": ["Person"], "props": { "name": format!("Ana {ns}") }, "text": "secret plans", "embedding": [1.0, 0.0] })),
        )
        .await;
        let (_, two) = call(
            &app,
            OWNER,
            "POST",
            &format!("/v1/{ns}/nodes"),
            Some(json!({ "labels": ["Person"], "props": { "name": format!("Bob {ns}") } })),
        )
        .await;
        let from = one["id"].as_str().unwrap().to_owned();
        let to = two["id"].as_str().unwrap().to_owned();
        let (_, edge) = call(
            &app,
            OWNER,
            "POST",
            &format!("/v1/{ns}/edges"),
            Some(json!({ "from": from, "label": "knows", "to": to })),
        )
        .await;
        ids.push((from, to, edge["id"].as_str().unwrap().to_owned()));
    }
    (app, ids)
}

/// Every route under a namespace, with a body that would work, and whether
/// it writes.
fn routes(
    ns: &str,
    node: &str,
    other: &str,
    edge: &str,
) -> Vec<(&'static str, String, Option<Value>, bool)> {
    let p = |rest: &str| format!("/v1/{ns}/{rest}");
    vec![
        ("GET", p("stats"), None, false),
        (
            "POST",
            p("neighborhoods"),
            Some(json!({"seeds":[node,other]})),
            false,
        ),
        (
            "POST",
            p("read-many"),
            Some(json!({"nodes":[node,other],"edges":[edge]})),
            false,
        ),
        ("GET", p("check"), None, false),
        ("POST", p("rebuild"), None, true),
        ("GET", p("nodes"), None, false),
        (
            "POST",
            p("nodes"),
            Some(json!({ "labels": ["Note"] })),
            true,
        ),
        ("GET", p(&format!("nodes/{node}")), None, false),
        (
            "PATCH",
            p(&format!("nodes/{node}")),
            Some(json!({ "props": { "x": 1 } })),
            true,
        ),
        ("GET", p(&format!("nodes/{node}/neighbors")), None, false),
        ("GET", p("edges"), None, false),
        (
            "POST",
            p("edges"),
            Some(json!({ "from": node, "label": "likes", "to": other })),
            true,
        ),
        ("GET", p(&format!("edges/{edge}")), None, false),
        (
            "POST",
            p(&format!("edges/{edge}/invalidate")),
            Some(json!({ "at": "2030-01-01" })),
            true,
        ),
        (
            "POST",
            p("k-hop"),
            Some(json!({ "start": node, "depth": 1 })),
            false,
        ),
        (
            "POST",
            p("path"),
            Some(json!({ "from": node, "to": other })),
            false,
        ),
        (
            "POST",
            p("subgraph"),
            Some(json!({ "seeds": [node] })),
            false,
        ),
        (
            "POST",
            p("search/text"),
            Some(json!({ "query": "secret" })),
            false,
        ),
        (
            "POST",
            p("search/vector"),
            Some(json!({ "embedding": [1.0, 0.0] })),
            false,
        ),
        (
            "POST",
            p("recall"),
            Some(json!({ "text": "secret" })),
            false,
        ),
        (
            "POST",
            p("context"),
            Some(json!({ "text": "secret" })),
            false,
        ),
        ("POST", p("batch"), Some(json!({ "ops": [] })), true),
        (
            "POST",
            p("query"),
            Some(json!({ "query": "MATCH (n) RETURN n.name" })),
            false,
        ),
        ("POST", p("algo/pageRank"), Some(json!({})), false),
        ("GET", p("resolve?name=Ana"), None, false),
        ("GET", p("duplicates"), None, false),
        ("POST", p("touch"), Some(json!({ "ids": [node] })), true),
        ("GET", p("weak"), None, false),
        ("GET", p("consolidation"), None, false),
        ("GET", format!("/v1/namespaces/{ns}"), None, false),
        (
            "PATCH",
            format!("/v1/namespaces/{ns}"),
            Some(json!({ "description": "x" })),
            true,
        ),
    ]
}

#[tokio::test]
async fn keys_see_only_their_namespaces() {
    let (app, ids) = world().await;
    let alice = key(
        &app,
        "alice",
        json!([{ "namespace": "a", "role": "write" }]),
        false,
    )
    .await;
    let reader = key(
        &app,
        "reader",
        json!([{ "namespace": "a", "role": "read" }]),
        false,
    )
    .await;
    let (a, b) = (&ids[0], &ids[1]);

    // Nothing of b, by any route: not even whether its ids exist.
    for (method, uri, body, _) in routes("b", &b.0, &b.1, &b.2) {
        let (status, answer) = call(&app, &alice, method, &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {answer}");
        let text = answer.to_string();
        assert!(
            !text.contains("Ana b") && !text.contains("secret plans"),
            "{method} {uri} leaks: {answer}"
        );
    }
    // Ids of b asked through a: not found there.
    let (status, _) = call(&app, &alice, "GET", &format!("/v1/a/nodes/{}", b.0), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A reader of a reads everything there and writes nothing.
    for (method, uri, body, writes) in routes("a", &a.0, &a.1, &a.2) {
        let (status, answer) = call(&app, &reader, method, &uri, body).await;
        if writes {
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {answer}");
        } else {
            assert!(status.is_success(), "{method} {uri}: {status} {answer}");
        }
    }
    let write_query = json!({ "query": "CREATE (:Note {title: 'x'})" });
    let (status, _) = call(
        &app,
        &reader,
        "POST",
        "/v1/a/query",
        Some(write_query.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a writing query needs write");
    let (status, _) = call(
        &app,
        &reader,
        "POST",
        "/v1/a/algo/pageRank",
        Some(json!({ "write": "rank" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "storing an algorithm's result needs write"
    );
    let (status, _) = call(&app, &alice, "POST", "/v1/a/query", Some(write_query)).await;
    assert_eq!(status, StatusCode::OK);
    let index = json!({ "query": "CREATE INDEX FOR (p:Person) ON (p.name)" });
    let (status, _) = call(&app, &alice, "POST", "/v1/a/query", Some(index)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "indexes need admin");
    let (status, _) = call(&app, &alice, "POST", "/v1/a/rebuild", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, &alice, "DELETE", "/v1/namespaces/a", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "dropping needs admin");

    // Listings show only what the key may read.
    let (_, list) = call(&app, &alice, "GET", "/v1/namespaces", None).await;
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a"]);
    let (_, shown) = call(
        &app,
        &alice,
        "POST",
        "/v1/a/query",
        Some(json!({ "query": "SHOW NAMESPACES" })),
    )
    .await;
    assert_eq!(shown["rows"].as_array().unwrap().len(), 1, "{shown}");
    let (status, dump) = call(&app, &alice, "GET", "/v1/export", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !dump.to_string().contains("Ana b"),
        "exporting everything is what it may read"
    );
    let (status, _) = call(&app, &alice, "GET", "/v1/export?ns=a,b", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &app,
        &alice,
        "POST",
        "/v1/namespaces",
        Some(json!({ "name": "c" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An account lists its own keys only (14.1), and manages no others.
    let (status, keys) = call(&app, &alice, "GET", "/v1/keys", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = keys
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|k| k["name"].as_str())
        .collect();
    assert_eq!(names, ["alice"], "{keys}");
    // Owner-only routes, and other accounts' keys.
    for (method, uri) in [
        ("POST", "/v1/import"),
        ("DELETE", "/v1/keys/reader"),
        ("GET", "/v1/accounts"),
    ] {
        let (status, _) = call(&app, &alice, method, uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    let (_, me) = call(&app, &alice, "GET", "/v1/whoami", None).await;
    assert_eq!(me["name"], "alice");
    assert!(me.get("secret_hash").is_none());

    // Wrong or revoked keys are not callers at all.
    let (status, _) = call(&app, "gfk_nope", "GET", "/v1/namespaces", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&app, OWNER, "DELETE", "/v1/keys/reader", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, &reader, "GET", "/v1/a/stats", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, keys) = call(&app, OWNER, "GET", "/v1/keys", None).await;
    assert!(!keys.to_string().contains("secret_hash"));
}

#[tokio::test]
async fn writes_record_who_made_them_and_own_only_keys_keep_to_theirs() {
    let (app, _) = world().await;
    let alice = key(
        &app,
        "alice",
        json!([{ "namespace": "a", "role": "write" }]),
        false,
    )
    .await;
    let bob = key(
        &app,
        "bob",
        json!([{ "namespace": "a", "role": "write" }]),
        true,
    )
    .await;
    let teams = key(
        &app,
        "teams",
        json!([{ "namespace": "team-*", "role": "admin" }]),
        false,
    )
    .await;

    let (_, note) = call(
        &app,
        &alice,
        "POST",
        "/v1/a/nodes",
        Some(json!({ "labels": ["Note"], "props": { "title": "by alice" } })),
    )
    .await;
    let note = note["id"].as_str().unwrap().to_owned();
    let (_, who) = call(
        &app,
        &alice,
        "POST",
        "/v1/a/query",
        Some(json!({ "query": "MATCH (n:Note) RETURN n.@recorded_by" })),
    )
    .await;
    assert_eq!(who["rows"], json!([["alice"]]));

    // Bob may change only what he recorded: not alice's note, not the
    // owner's people, by any route.
    let (status, body) = call(
        &app,
        &bob,
        "PATCH",
        &format!("/v1/a/nodes/{note}"),
        Some(json!({ "props": { "x": 1 } })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "forbidden");
    let (status, _) = call(&app, &bob, "DELETE", &format!("/v1/a/nodes/{note}"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &app,
        &bob,
        "POST",
        "/v1/a/query",
        Some(json!({ "query": "MATCH (n:Note) SET n.title = 'mine'" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "GQ writes obey the same rule"
    );
    let (status, _) = call(
        &app,
        &bob,
        "POST",
        "/v1/a/query",
        Some(json!({ "query": "MATCH ()-[r:knows]->() RETRACT r" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // His own: fine, and he may connect his node to others'.
    let (_, mine) = call(
        &app,
        &bob,
        "POST",
        "/v1/a/nodes",
        Some(json!({ "labels": ["Note"], "props": { "title": "by bob" } })),
    )
    .await;
    let mine = mine["id"].as_str().unwrap().to_owned();
    let (status, _) = call(
        &app,
        &bob,
        "PATCH",
        &format!("/v1/a/nodes/{mine}"),
        Some(json!({ "props": { "x": 1 } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        &bob,
        "POST",
        "/v1/a/edges",
        Some(json!({ "from": mine, "label": "cites", "to": note })),
    )
    .await;
    assert!(status.is_success());
    // The refused writes left nothing behind.
    let (_, titles) = call(
        &app,
        OWNER,
        "POST",
        "/v1/a/query",
        Some(json!({ "query": "MATCH (n:Note) RETURN n.title ORDER BY n.title" })),
    )
    .await;
    assert_eq!(titles["rows"], json!([["by alice"], ["by bob"]]));

    // Prefix grants.
    let (status, _) = call(
        &app,
        &teams,
        "POST",
        "/v1/namespaces",
        Some(json!({ "name": "team-ops" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        &app,
        &teams,
        "POST",
        "/v1/namespaces",
        Some(json!({ "name": "teams" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "team-* does not cover teams");
    let (status, _) = call(
        &app,
        OWNER,
        "POST",
        "/v1/namespaces",
        Some(json!({ "name": "keys" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "route names are reserved");
}

#[tokio::test]
async fn an_open_server_stays_open_until_a_key_exists() {
    let app = router(AppState::new(Arc::new(Grafeno::in_memory().unwrap()), None));
    let (status, _) = call(&app, "anything", "GET", "/v1/namespaces", None).await;
    assert_eq!(status, StatusCode::OK);
    let grants = json!([{ "namespace": "a", "role": "read" }]);
    let agent = json!({ "name": "agent", "grants": grants });
    let (status, body) = call(&app, "anything", "POST", "/v1/keys", Some(agent.clone())).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the first key must be an owner: {body}"
    );
    let (status, body) = call(
        &app,
        "anything",
        "POST",
        "/v1/keys",
        Some(json!({ "name": "admin", "owner": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let admin = body["secret"].as_str().unwrap().to_owned();
    let (status, body) = call(&app, &admin, "POST", "/v1/keys", Some(agent)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let secret = body["secret"].as_str().unwrap().to_owned();
    // With a key, anonymous callers are refused; the key works.
    let (status, _) = call(&app, "anything", "GET", "/v1/namespaces", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&app, &secret, "GET", "/v1/a/stats", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, keys) = call(&app, &secret, "GET", "/v1/keys", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(keys.as_array().unwrap().len(), 1, "only its own: {keys}");
    let (status, keys) = call(&app, &admin, "GET", "/v1/keys", None).await;
    assert_eq!(status, StatusCode::OK, "owner keys manage keys");
    assert_eq!(keys.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn owners_download_a_backup_others_do_not() {
    let (app, _) = world().await;
    let alice = key(
        &app,
        "alice",
        json!([{ "namespace": "a", "role": "admin" }]),
        false,
    )
    .await;
    let (status, _) = call(&app, &alice, "GET", "/v1/backup", None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a backup holds every namespace"
    );
    let request = Request::builder()
        .uri("/v1/backup")
        .header(header::AUTHORIZATION, format!("Bearer {OWNER}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let path = std::env::temp_dir().join(format!(
        "grafeno-http-backup-{}.redb",
        grafeno::NodeId::new()
    ));
    std::fs::write(&path, &bytes).unwrap();
    {
        let copy = Grafeno::open(&path).unwrap();
        let read = copy.begin_read().unwrap();
        for ns in ["a", "b"] {
            assert_eq!(read.stats(ns).unwrap().nodes, 2);
            assert!(read.check(ns).unwrap().is_ok());
        }
        assert_eq!(read.api_keys().unwrap().len(), 1, "with its keys");
    }
    std::fs::remove_file(&path).unwrap();
}

#[tokio::test]
async fn vector_recall_and_compact_over_http() {
    let (app, _) = world().await;
    let (status, body) = call(&app, OWNER, "GET", "/v1/a/vectors/recall?sample=5", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sample"], 5);
    let writer = key(
        &app,
        "writer",
        json!([{ "namespace": "a", "role": "write" }]),
        false,
    )
    .await;
    let (status, _) = call(&app, &writer, "POST", "/v1/a/compact", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "compacting needs admin");
    let (status, report) = call(&app, OWNER, "POST", "/v1/a/compact", None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["vectors"], 1);
    assert_eq!(report["unreachable_after"], 0);
}

#[tokio::test]
async fn namespace_statements_need_rights_on_the_namespace_they_name() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    let query = |q: &str| json!({ "query": q });
    // The owner creates, describes and drops through any namespace.
    let (status, body) = call(
        &app,
        OWNER,
        "POST",
        "/v1/home/query",
        Some(query("CREATE NAMESPACE 'team-a' COMMENT 'first team'")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"][0][0], "team-a");
    assert_eq!(body["rows"][0][4], "first team");
    let (_, info) = call(&app, OWNER, "GET", "/v1/namespaces/team-a", None).await;
    assert_eq!(info["description"], "first team", "{info}");

    // Admin of `team-*` (which includes creating namespaces that match),
    // reading `home`: may create team-b, not other.
    let team = key(
        &app,
        "team-admin",
        json!([{ "namespace": "team-*", "role": "admin" }, { "namespace": "home", "role": "read" }]),
        false,
    )
    .await;
    let (status, body) = call(
        &app,
        &team,
        "POST",
        "/v1/home/query",
        Some(query("CREATE NAMESPACE 'team-b'")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app,
        &team,
        "POST",
        "/v1/home/query",
        Some(query("CREATE NAMESPACE other")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = call(
        &app,
        &team,
        "POST",
        "/v1/home/query",
        Some(query("ALTER NAMESPACE 'team-a' SET COMMENT 'renamed'")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A reader may not drop.
    let reader = key(
        &app,
        "reader",
        json!([{ "namespace": "team-*", "role": "read" }]),
        false,
    )
    .await;
    let (status, _) = call(
        &app,
        &reader,
        "POST",
        "/v1/team-a/query",
        Some(query("DROP NAMESPACE 'team-a'")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &app,
        &team,
        "POST",
        "/v1/home/query",
        Some(query("DROP NAMESPACE 'team-a'")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(&app, OWNER, "GET", "/v1/namespaces/team-a", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
