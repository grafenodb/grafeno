//! Phase 14.2: roles over HTTP. Defining roles (and refusing cycles,
//! unknown roles and privileges that do not fit their object); granting
//! them to accounts; a role reached through another; patterns with a
//! denial that wins; a custom editor without `delete` (in GQ too, change
//! by change, 14.4); server privileges
//! (metrics, managing roles, managing accounts but not superusers); the
//! `public` role; and keys whose scope still narrows what roles give.

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

async fn ok(app: &Router, method: &str, uri: &str, body: Value) -> Value {
    let (code, answer) = call(app, OWNER, method, uri, Some(body)).await;
    assert!(code.is_success(), "{method} {uri}: {code} {answer}");
    answer
}

/// A service account with `roles` and a key of it.
async fn account(app: &Router, name: &str, roles: Value) -> String {
    ok(
        app,
        "POST",
        "/v1/accounts",
        json!({ "name": name, "kind": "service", "roles": roles }),
    )
    .await;
    key(app, name, None).await
}

async fn key(app: &Router, account: &str, scope: Option<Value>) -> String {
    let name = format!("{account}-{}", scope.is_some());
    let made = ok(
        app,
        "POST",
        "/v1/keys",
        json!({ "name": name, "account": account, "scope": scope }),
    )
    .await;
    made["secret"].as_str().unwrap().to_owned()
}

fn on_ns(action: &str, ns: &str) -> Value {
    json!({ "action": action, "object": { "on": "namespace", "namespace": ns } })
}

fn deny_ns(action: &str, ns: &str) -> Value {
    json!({ "action": action, "object": { "on": "namespace", "namespace": ns }, "deny": true })
}

async fn node(app: &Router, key: &str, ns: &str) -> StatusCode {
    call(
        app,
        key,
        "POST",
        &format!("/v1/{ns}/nodes"),
        Some(json!({ "labels": ["Doc"] })),
    )
    .await
    .0
}

async fn stats(app: &Router, key: &str, ns: &str) -> StatusCode {
    call(app, key, "GET", &format!("/v1/{ns}/stats"), None)
        .await
        .0
}

#[tokio::test]
async fn roles_grant_deny_and_nest() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    for ns in ["team-a", "team-secret", "other"] {
        assert_eq!(node(&app, OWNER, ns).await, StatusCode::CREATED);
    }

    // The catalog names every action by what it applies to.
    let (code, catalog) = call(&app, OWNER, "GET", "/v1/privileges", None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        catalog["server"]
            .as_array()
            .unwrap()
            .contains(&json!("manage_roles"))
    );
    assert!(
        catalog["namespace"]
            .as_array()
            .unwrap()
            .contains(&json!("merge"))
    );

    // Reading every team but the secret one; a role reached through
    // another gives the same.
    let made = ok(
        &app,
        "PUT",
        "/v1/roles/team-reader",
        json!({ "privileges": [on_ns("read", "team-*"), deny_ns("read", "team-secret")],
                "description": "reads the teams" }),
    )
    .await;
    assert_eq!(made["name"], "team-reader");
    ok(
        &app,
        "PUT",
        "/v1/roles/analyst",
        json!({ "roles": ["team-reader"], "privileges": [on_ns("algorithms", "team-*")] }),
    )
    .await;
    let analyst = account(&app, "ana", json!(["analyst"])).await;
    assert_eq!(stats(&app, &analyst, "team-a").await, StatusCode::OK);
    assert_eq!(
        stats(&app, &analyst, "team-secret").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(stats(&app, &analyst, "other").await, StatusCode::FORBIDDEN);
    assert_eq!(node(&app, &analyst, "team-a").await, StatusCode::FORBIDDEN);
    let (_, me) = call(&app, &analyst, "GET", "/v1/whoami", None).await;
    assert_eq!(me["roles"], json!(["analyst"]));
    assert_eq!(me["effective_roles"], json!(["analyst", "team-reader"]));
    // Listing namespaces shows what it may read.
    let (_, listed) = call(&app, &analyst, "GET", "/v1/namespaces", None).await;
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["name"].as_str())
        .collect();
    assert_eq!(names, ["team-a"], "{listed}");

    // Event subscriptions and their values need `events` (16.10).
    let show = json!({ "query": "SHOW EVENT SUBSCRIPTIONS" });
    let (code, _) = call(
        &app,
        &analyst,
        "POST",
        "/v1/team-a/query",
        Some(show.clone()),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(&app, &analyst, "GET", "/v1/team-a/events/x", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    ok(
        &app,
        "PUT",
        "/v1/roles/watcher",
        json!({ "privileges": [on_ns("read", "team-a"), on_ns("events", "team-a")] }),
    )
    .await;
    let watcher = account(&app, "wes", json!(["watcher"])).await;
    let (code, body) = call(&app, &watcher, "POST", "/v1/team-a/query", Some(show)).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (code, _) = call(&app, &watcher, "GET", "/v1/team-a/events/x", None).await;
    assert_eq!(
        code,
        StatusCode::BAD_REQUEST,
        "allowed, and no such subscription"
    );

    // A denial added to the inner role reaches the account at once.
    ok(
        &app,
        "PUT",
        "/v1/roles/team-reader",
        json!({ "privileges": [on_ns("read", "team-*"), deny_ns("read", "team-secret"),
                               deny_ns("read", "team-a")] }),
    )
    .await;
    assert_eq!(stats(&app, &analyst, "team-a").await, StatusCode::FORBIDDEN);

    // Cycles, unknown roles and misplaced privileges are refused.
    let (code, body) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/roles/team-reader",
        Some(json!({ "roles": ["analyst"] })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    let (code, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/roles/x",
        Some(json!({ "roles": ["nobody"] })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/roles/x",
        Some(json!({ "privileges": [{ "action": "backup", "object": { "on": "namespace", "namespace": "a" } }] })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(&app, OWNER, "PUT", "/v1/roles/editor", Some(json!({}))).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "built-in names are taken");
    let (code, _) = call(
        &app,
        OWNER,
        "POST",
        "/v1/accounts",
        Some(json!({ "name": "ghost", "kind": "service", "roles": ["nobody"] })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);

    // An editor that may not delete: creating passes, deleting does not,
    // nor does a writing query (it needs every write until 14.4).
    ok(
        &app,
        "PUT",
        "/v1/roles/careful-editor",
        json!({ "grants": [{ "namespace": "other", "role": "write" }],
                "privileges": [deny_ns("delete", "other")] }),
    )
    .await;
    let careful = account(&app, "carl", json!(["careful-editor"])).await;
    let (code, made) = call(
        &app,
        &careful,
        "POST",
        "/v1/other/nodes",
        Some(json!({ "labels": ["Doc"] })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let id = made["id"].as_str().unwrap().to_owned();
    let (code, body) = call(
        &app,
        &careful,
        "DELETE",
        &format!("/v1/other/nodes/{id}"),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`delete`"),
        "{body}"
    );
    // In GQ each change is checked (14.4): creating passes, deleting not.
    let (code, body) = call(
        &app,
        &careful,
        "POST",
        "/v1/other/query",
        Some(json!({ "query": "CREATE (:Note {title: 'x'})" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (code, body) = call(
        &app,
        &careful,
        "POST",
        "/v1/other/query",
        Some(json!({ "query": "MATCH (n:Note) DELETE n" })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`delete`"),
        "{body}"
    );
    let (code, _) = call(&app, &careful, "POST", "/v1/other/rebuild", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "schema is not part of write");

    // Deleting a role takes it from every account.
    let (code, _) = call(&app, OWNER, "DELETE", "/v1/roles/careful-editor", None).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    assert_eq!(node(&app, &careful, "other").await, StatusCode::FORBIDDEN);
    let (_, carl) = call(&app, OWNER, "GET", "/v1/accounts/carl", None).await;
    assert_eq!(carl["roles"], json!([]));
}

#[tokio::test]
async fn server_privileges_public_and_scopes() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    assert_eq!(node(&app, OWNER, "a").await, StatusCode::CREATED);
    assert_eq!(node(&app, OWNER, "b").await, StatusCode::CREATED);

    // Metrics only.
    ok(
        &app,
        "PUT",
        "/v1/roles/monitor",
        json!({ "privileges": [{ "action": "metrics", "object": { "on": "server" } }] }),
    )
    .await;
    let monitor = account(&app, "mon", json!(["monitor"])).await;
    let (code, _) = call(&app, &monitor, "GET", "/v1/metrics", None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = call(&app, &monitor, "GET", "/v1/roles", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert_eq!(stats(&app, &monitor, "a").await, StatusCode::FORBIDDEN);

    // Managing roles and accounts, but not superusers.
    ok(
        &app,
        "PUT",
        "/v1/roles/people",
        json!({ "privileges": [{ "action": "manage_accounts", "object": { "on": "server" } },
                               { "action": "manage_roles", "object": { "on": "server" } }] }),
    )
    .await;
    let people = account(&app, "hr", json!(["people"])).await;
    let (code, roles) = call(&app, &people, "GET", "/v1/roles", None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(roles.as_array().unwrap().len(), 2);
    let (code, _) = call(
        &app,
        &people,
        "PUT",
        "/v1/roles/reader-a",
        Some(json!({ "grants": [{ "namespace": "a", "role": "read" }] })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let (code, _) = call(
        &app,
        &people,
        "POST",
        "/v1/accounts",
        Some(json!({ "name": "rita", "kind": "service", "roles": ["reader-a"] })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let (code, _) = call(
        &app,
        &people,
        "POST",
        "/v1/accounts",
        Some(json!({ "name": "boss", "kind": "service", "superuser": true })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(
        &app,
        &people,
        "PATCH",
        "/v1/accounts/hr",
        Some(json!({ "superuser": true })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    ok(
        &app,
        "POST",
        "/v1/accounts",
        json!({ "name": "root", "kind": "service", "superuser": true }),
    )
    .await;
    let (code, _) = call(&app, &people, "DELETE", "/v1/accounts/root", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // Granting a role to an account takes effect on its next request.
    let rita = key(&app, "rita", None).await;
    assert_eq!(stats(&app, &rita, "a").await, StatusCode::OK);
    assert_eq!(stats(&app, &rita, "b").await, StatusCode::FORBIDDEN);
    let (code, _) = call(
        &app,
        &people,
        "PATCH",
        "/v1/accounts/rita",
        Some(json!({ "roles": [] })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(stats(&app, &rita, "a").await, StatusCode::FORBIDDEN);

    // `public` is every account's, once defined.
    ok(
        &app,
        "PUT",
        "/v1/roles/public",
        json!({ "grants": [{ "namespace": "b", "role": "read" }] }),
    )
    .await;
    assert_eq!(stats(&app, &rita, "b").await, StatusCode::OK);
    assert_eq!(stats(&app, &monitor, "b").await, StatusCode::OK);

    // A key's scope still narrows what roles give, and a scoped key has
    // no server privileges.
    ok(
        &app,
        "PUT",
        "/v1/roles/writer",
        json!({ "grants": [{ "namespace": "*", "role": "write" }],
                "privileges": [{ "action": "metrics", "object": { "on": "server" } }] }),
    )
    .await;
    ok(
        &app,
        "PATCH",
        "/v1/accounts/rita",
        json!({ "roles": ["writer"] }),
    )
    .await;
    let scoped = key(
        &app,
        "rita",
        Some(json!({ "namespaces": ["a"], "read_only": true })),
    )
    .await;
    assert_eq!(node(&app, &rita, "b").await, StatusCode::CREATED);
    assert_eq!(stats(&app, &scoped, "a").await, StatusCode::OK);
    assert_eq!(node(&app, &scoped, "a").await, StatusCode::FORBIDDEN);
    assert_eq!(stats(&app, &scoped, "b").await, StatusCode::FORBIDDEN);
    let (code, _) = call(&app, &rita, "GET", "/v1/metrics", None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = call(&app, &scoped, "GET", "/v1/metrics", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
}
