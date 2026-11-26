//! Namespace aliases: `/v1/{alias}/…` serves the namespace it points at,
//! moving it is one commit, rights apply to the namespace, and GQ, MCP and
//! the engine know them too.

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

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (code, answer) = call(app, OWNER, method, uri, body).await;
    assert!(code.is_success(), "{method} {uri}: {code} {answer}");
    answer
}

async fn query(app: &Router, key: &str, ns: &str, text: &str) -> (StatusCode, Value) {
    call(
        app,
        key,
        "POST",
        &format!("/v1/{ns}/query"),
        Some(json!({ "query": text })),
    )
    .await
}

async fn names(app: &Router, ns: &str) -> Vec<String> {
    let (code, answer) = query(
        app,
        OWNER,
        ns,
        "MATCH (p:Person) RETURN p.name ORDER BY p.name",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{answer}");
    answer["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[0].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn an_alias_serves_its_namespace_and_moves_in_one_step() {
    let db = Arc::new(Grafeno::in_memory().unwrap());
    let app = router(AppState::new(db.clone(), Some(OWNER)));
    for (ns, name) in [("people_v1", "Ana"), ("people_v2", "Bea")] {
        let (code, answer) = query(
            &app,
            OWNER,
            ns,
            &format!("CREATE (:Person {{name: '{name}'}})"),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{answer}");
    }

    // An alias needs a namespace to point at, and cannot take a namespace's
    // name or point at another alias.
    let (code, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/aliases/people",
        Some(json!({"namespace": "nowhere"})),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/aliases/people_v2",
        Some(json!({"namespace": "people_v1"})),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let made = ok(
        &app,
        "PUT",
        "/v1/aliases/people",
        Some(json!({"namespace": "people_v1"})),
    )
    .await;
    assert_eq!(made["namespace"], "people_v1");
    let (code, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/aliases/other",
        Some(json!({"namespace": "people"})),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);

    // Every route under the alias is its namespace's: GQ, nodes, lookups.
    assert_eq!(names(&app, "people").await, ["Ana"]);
    let (code, page) = call(
        &app,
        OWNER,
        "POST",
        "/v1/people/nodes/find",
        Some(json!({"label": "Person"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(page["items"][0]["props"]["name"], "Ana");
    // A write through it lands in the namespace.
    let (code, _) = query(&app, OWNER, "people", "CREATE (:Person {name: 'Ada'})").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(names(&app, "people_v1").await, ["Ada", "Ana"]);
    // No namespace called `people` came to be.
    assert!(
        db.begin_read()
            .unwrap()
            .namespace("people")
            .unwrap()
            .is_none()
    );
    let request = Request::builder()
        .uri("/v1/people/stats")
        .header(header::AUTHORIZATION, format!("Bearer {OWNER}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()["grafeno-namespace"], "people_v1");

    // Moving it: the next request reads the new namespace.
    let moved = ok(
        &app,
        "PUT",
        "/v1/aliases/people",
        Some(json!({"namespace": "people_v2"})),
    )
    .await;
    assert_eq!(moved["previous"], "people_v1");
    assert_eq!(names(&app, "people").await, ["Bea"]);
    let listed = ok(&app, "GET", "/v1/aliases", None).await;
    assert_eq!(listed[0]["name"], "people");
    assert_eq!(listed[0]["namespace"], "people_v2");

    // A namespace an alias points at is not dropped, nor a namespace made
    // with an alias's name.
    let (code, _) = call(&app, OWNER, "DELETE", "/v1/namespaces/people_v2", None).await;
    assert_eq!(code, StatusCode::CONFLICT);
    let (code, _) = call(
        &app,
        OWNER,
        "POST",
        "/v1/namespaces",
        Some(json!({"name": "people"})),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT);
    let (code, _) = call(&app, OWNER, "DELETE", "/v1/aliases/people", None).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let (code, _) = call(&app, OWNER, "GET", "/v1/aliases/people", None).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _) = call(&app, OWNER, "DELETE", "/v1/namespaces/people_v1", None).await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test]
async fn gq_makes_moves_and_drops_aliases() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    for (ns, name) in [("crm_v1", "Ana"), ("crm_v2", "Bea")] {
        query(
            &app,
            OWNER,
            ns,
            &format!("CREATE (:Person {{name: '{name}'}})"),
        )
        .await;
    }
    let (code, made) = query(
        &app,
        OWNER,
        "crm_v1",
        "CREATE ALIAS crm FOR NAMESPACE crm_v1",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{made}");
    assert_eq!(made["rows"][0][1], "crm_v1");
    let (code, _) = query(
        &app,
        OWNER,
        "crm_v1",
        "CREATE ALIAS crm FOR NAMESPACE crm_v2",
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, same) = query(
        &app,
        OWNER,
        "crm_v1",
        "CREATE ALIAS crm IF NOT EXISTS FOR NAMESPACE crm_v2",
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(same["rows"], json!([]));
    assert_eq!(names(&app, "crm").await, ["Ana"]);
    let (code, moved) = query(&app, OWNER, "crm", "ALTER ALIAS crm SET NAMESPACE crm_v2").await;
    assert_eq!(code, StatusCode::OK, "{moved}");
    assert_eq!(moved["rows"][0][2], "crm_v1");
    assert_eq!(names(&app, "crm").await, ["Bea"]);
    let (_, shown) = query(&app, OWNER, "crm", "SHOW ALIASES").await;
    assert_eq!(shown["rows"][0][0], "crm");
    assert_eq!(shown["rows"][0][1], "crm_v2");
    let (code, _) = query(&app, OWNER, "crm", "DROP NAMESPACE crm_v2").await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = query(&app, OWNER, "crm_v1", "DROP ALIAS crm").await;
    assert_eq!(code, StatusCode::OK);
    let (code, none) = query(&app, OWNER, "crm_v1", "DROP ALIAS crm IF EXISTS").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(none["rows"], json!([]));
}

/// A role with `privileges` and a key of an account that has it.
async fn key_with(app: &Router, name: &str, privileges: Value) -> String {
    ok(
        app,
        "PUT",
        &format!("/v1/roles/{name}-role"),
        Some(json!({ "privileges": privileges })),
    )
    .await;
    ok(
        app,
        "POST",
        "/v1/accounts",
        Some(json!({ "name": name, "kind": "service", "roles": [format!("{name}-role")] })),
    )
    .await;
    let made = ok(
        app,
        "POST",
        "/v1/keys",
        Some(json!({ "name": format!("{name}-key"), "account": name })),
    )
    .await;
    made["secret"].as_str().unwrap().to_owned()
}

fn on_ns(action: &str, ns: &str) -> Value {
    json!({ "action": action, "object": { "on": "namespace", "namespace": ns } })
}

#[tokio::test]
async fn rights_are_the_namespaces_not_the_aliases() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    for ns in ["public_v1", "secret"] {
        query(&app, OWNER, ns, "CREATE (:Person {name: 'X'})").await;
    }
    ok(
        &app,
        "PUT",
        "/v1/aliases/public",
        Some(json!({"namespace": "public_v1"})),
    )
    .await;
    let reader = key_with(&app, "reader", json!([on_ns("read", "public_v1")])).await;
    // Reading through the alias is reading the namespace.
    let (code, _) = query(&app, &reader, "public", "MATCH (n) RETURN count(n)").await;
    assert_eq!(code, StatusCode::OK);
    // A grant on the alias's name gives nothing.
    let by_name = key_with(&app, "by-name", json!([on_ns("read", "public")])).await;
    let (code, _) = query(&app, &by_name, "public", "MATCH (n) RETURN count(n)").await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // Pointing it elsewhere needs `schema` on both namespaces: a reader
    // cannot move it, nor can someone with `schema` on the target alone.
    let (code, _) = call(
        &app,
        &reader,
        "PUT",
        "/v1/aliases/public",
        Some(json!({"namespace": "secret"})),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let schemer = key_with(&app, "schemer", json!([on_ns("schema", "secret")])).await;
    let (code, _) = call(
        &app,
        &schemer,
        "PUT",
        "/v1/aliases/public",
        Some(json!({"namespace": "secret"})),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = query(
        &app,
        &schemer,
        "secret",
        "ALTER ALIAS public SET NAMESPACE secret",
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // Moving it changes nothing for the reader's rights: the secret
    // namespace stays closed through the alias.
    ok(
        &app,
        "PUT",
        "/v1/aliases/public",
        Some(json!({"namespace": "secret"})),
    )
    .await;
    let (code, _) = query(&app, &reader, "public", "MATCH (n) RETURN count(n)").await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // The list shows only aliases whose namespace the caller reads.
    let (_, listed) = call(&app, &reader, "GET", "/v1/aliases", None).await;
    assert_eq!(listed, json!([]));
}

#[test]
fn mcp_tools_take_an_alias_for_their_namespace() {
    let db = Arc::new(Grafeno::in_memory().unwrap());
    db.write(|tx| {
        tx.create_namespace("notes_v1", None)?;
        tx.set_alias("notes", "notes_v1")?;
        Ok(())
    })
    .unwrap();
    let server = grafeno_server::mcp::McpServer::shared(db.clone(), "notes".into());
    let made = server
        .call("remember", json!({ "text": "aliases resolve in MCP" }))
        .unwrap();
    assert_eq!(made["isError"], false, "{made}");
    let read = db.begin_read().unwrap();
    assert_eq!(read.stats("notes_v1").unwrap().nodes, 1);
    assert!(read.namespace("notes").unwrap().is_none());
}

#[test]
fn the_engine_keeps_aliases_with_the_graph() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        tx.create_namespace("a", None)?;
        tx.create_namespace("b", None)?;
        assert_eq!(tx.set_alias("x", "a")?, None);
        Ok(())
    })
    .unwrap();
    assert_eq!(db.resolve_alias("x").unwrap().as_deref(), Some("a"));
    // A write that fails leaves the alias where it was.
    let failed = db.write(|tx| {
        tx.set_alias("x", "b")?;
        Err::<(), _>(grafeno::Error::Invalid("no".into()))
    });
    assert!(failed.is_err());
    assert_eq!(db.resolve_alias("x").unwrap().as_deref(), Some("a"));
    db.write(|tx| tx.set_alias("x", "b")).unwrap();
    assert_eq!(db.resolve_alias("x").unwrap().as_deref(), Some("b"));
    assert_eq!(
        db.begin_read().unwrap().resolve_namespace("x").unwrap(),
        "b"
    );
    assert_eq!(
        db.begin_read().unwrap().resolve_namespace("b").unwrap(),
        "b"
    );
    // Writing to an alias's name does not make a namespace of it.
    let shadow = db.write(|tx| tx.create_node("x", grafeno::NewNode::new("N")));
    assert!(shadow.is_err());
    // Aliases are not namespaces.
    let names: Vec<String> = db
        .begin_read()
        .unwrap()
        .namespaces()
        .unwrap()
        .into_iter()
        .map(|info| info.name)
        .collect();
    assert_eq!(names, ["a", "b"]);
    assert!(db.write(|tx| tx.drop_alias("x")).unwrap());
    assert_eq!(db.resolve_alias("x").unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readers_see_one_namespace_or_the_other_while_it_moves() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    for (ns, n) in [("live_v1", 10), ("live_v2", 20)] {
        query(
            &app,
            OWNER,
            ns,
            &format!("UNWIND range(1, {n}) AS i CREATE (:Person {{name: 'P' + toString(i)}})"),
        )
        .await;
    }
    ok(
        &app,
        "PUT",
        "/v1/aliases/live",
        Some(json!({"namespace": "live_v1"})),
    )
    .await;
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move {
                let mut seen = std::collections::BTreeSet::new();
                for _ in 0..60 {
                    let (code, answer) =
                        query(&app, OWNER, "live", "MATCH (p:Person) RETURN count(p)").await;
                    assert_eq!(code, StatusCode::OK, "{answer}");
                    seen.insert(answer["rows"][0][0].as_i64().unwrap());
                }
                seen
            })
        })
        .collect();
    for round in 0..20 {
        let target = if round % 2 == 0 { "live_v2" } else { "live_v1" };
        ok(
            &app,
            "PUT",
            "/v1/aliases/live",
            Some(json!({"namespace": target})),
        )
        .await;
        tokio::task::yield_now().await;
    }
    for reader in readers {
        let seen = reader.await.unwrap();
        // Every answer is a whole namespace: 10 or 20 people, never a mix.
        assert!(seen.iter().all(|n| *n == 10 || *n == 20), "{seen:?}");
    }
}
