//! Phase 14.5: the audit log over HTTP. Each security event appears once:
//! sign-ins and their failures, a lockout, sessions, keys, changes to
//! accounts and roles (over the API and in GQ), refused requests (for a
//! credential, for a privilege, by a handler); with who, how and from
//! where. Reading it needs the `audit` privilege: a page, a stream, JSON
//! Lines, and `db.audit()` in GQ. Managing accounts lists and ends the
//! sessions of other accounts, not of superusers'.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const OWNER: &str = "owner-secret";

async fn call(
    app: &Router,
    key: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let mut request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let address: SocketAddr = "192.0.2.7:4000".parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(address));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
}

async fn events(app: &Router) -> Vec<Value> {
    let (code, page) = call(app, Some(OWNER), "GET", "/v1/audit?limit=10000", None).await;
    assert_eq!(code, StatusCode::OK, "{page}");
    page["events"].as_array().unwrap().clone()
}

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|e| e["kind"] == kind).count()
}

#[tokio::test]
async fn every_security_event_appears_once() {
    let mut state = AppState::new(Arc::new(Grafeno::in_memory().unwrap()), Some(OWNER));
    state.auth.lockout.account_attempts = 3;
    let app = router(state);
    // A user, a role and a grant, over the API and in GQ.
    let (code, _) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/accounts",
        Some(json!({ "name": "ana", "kind": "user", "password": "correct horse battery" })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    for query in [
        "CREATE ROLE auditor",
        "GRANT audit, manage_accounts ON SERVER TO auditor",
        "GRANT reader ON NAMESPACE default TO auditor",
        "GRANT ROLE auditor TO ana",
        "CREATE SERVICE ACCOUNT bot",
    ] {
        let (code, body) = call(
            &app,
            Some(OWNER),
            "POST",
            "/v1/default/query",
            Some(json!({ "query": query })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{query}: {body}");
    }
    // Sign-ins: one that works, failures, and the lockout they cause.
    let (code, session) = call(
        &app,
        None,
        "POST",
        "/v1/auth/signin",
        Some(json!({ "name": "ana", "password": "correct horse battery" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{session}");
    let ana = session["access_token"].as_str().unwrap().to_owned();
    for _ in 0..3 {
        let (code, _) = call(
            &app,
            None,
            "POST",
            "/v1/auth/signin",
            Some(json!({ "name": "bob", "password": "not the one at all" })),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }
    let (code, _) = call(
        &app,
        None,
        "POST",
        "/v1/auth/signin",
        Some(json!({ "name": "bob", "password": "not the one at all" })),
    )
    .await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
    // Keys.
    let (_, made) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/keys",
        Some(json!({ "name": "bot-key", "account": "bot" })),
    )
    .await;
    let bot = made["secret"].as_str().unwrap().to_owned();
    let (code, rotated) = call(&app, Some(OWNER), "POST", "/v1/keys/bot-key/rotate", None).await;
    assert_eq!(code, StatusCode::OK, "{rotated}");
    let bot_new = rotated["secret"].as_str().unwrap().to_owned();
    // Refused: no credential, a wrong one, a privilege (by the middleware),
    // a statement (by the handler).
    let (code, _) = call(&app, None, "GET", "/v1/namespaces", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _) = call(&app, Some(&bot), "GET", "/v1/namespaces", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "the rotated-away secret");
    let (code, _) = call(&app, Some(&bot_new), "GET", "/v1/metrics", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(
        &app,
        Some(&bot_new),
        "POST",
        "/v1/default/query",
        Some(json!({ "query": "CREATE ROLE sneaky" })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(&app, Some(OWNER), "DELETE", "/v1/keys/bot-key", None).await;
    assert_eq!(code, StatusCode::NO_CONTENT);

    let all = events(&app).await;
    for (kind, times) in [
        ("account_created", 2),
        ("role_created", 1),
        ("role_changed", 2),
        ("account_changed", 1),
        ("session_created", 1),
        ("signin_failed", 4),
        ("lockout", 1),
        ("key_created", 1),
        ("key_rotated", 1),
        ("key_revoked", 1),
        ("refused", 4),
    ] {
        assert_eq!(count(&all, kind), times, "{kind}: {all:#?}");
    }
    // Numbered without gaps.
    for (i, event) in all.iter().enumerate() {
        assert_eq!(event["seq"], i as u64 + 1);
    }
    // Who, how and from where.
    let created = all.iter().find(|e| e["kind"] == "account_created").unwrap();
    assert_eq!(created["via"], "token");
    assert_eq!(created["address"], "192.0.2.7");
    assert_eq!(created["target"], "ana");
    let locked = all.iter().find(|e| e["kind"] == "lockout").unwrap();
    assert_eq!(
        (locked["target"].clone(), locked["outcome"].clone()),
        (json!("bob"), json!("refused"))
    );
    let refused: Vec<&Value> = all.iter().filter(|e| e["kind"] == "refused").collect();
    assert_eq!(refused[0]["target"], "GET /v1/namespaces");
    assert!(refused[0].get("account").is_none());
    assert_eq!(refused[2]["account"], "bot");
    assert_eq!(refused[2]["via"], "key:bot-key");
    assert_eq!(refused[3]["target"], "POST /v1/default/query");
    let text = serde_json::to_string(&all).unwrap();
    for secret in [
        bot.as_str(),
        bot_new.as_str(),
        ana.as_str(),
        "correct horse battery",
        "not the one at all",
    ] {
        assert!(!text.contains(secret), "a secret in the audit log");
    }

    // Reading needs the privilege; ana (auditor) has it.
    let (code, _) = call(&app, Some(&bot_new), "GET", "/v1/audit", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "revoked by now");
    let (code, page) = call(&app, Some(&ana), "GET", "/v1/audit?since=3&limit=2", None).await;
    assert_eq!(code, StatusCode::OK, "{page}");
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["events"][0]["seq"], 4);
    // JSON Lines.
    let request = Request::builder()
        .uri("/v1/audit?format=jsonl")
        .header(header::AUTHORIZATION, format!("Bearer {ana}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let lines: Vec<Value> = String::from_utf8_lossy(&body)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(lines.len() >= all.len());
    // In GQ.
    let (code, rows) = call(
        &app,
        Some(&ana),
        "POST",
        "/v1/default/query",
        Some(json!({ "query": "CALL db.audit(0, 1000) YIELD seq, kind WHERE kind = 'lockout' RETURN seq" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rows}");
    assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
    // A stream, from an event on.
    let request = Request::builder()
        .uri("/v1/audit")
        .header(header::AUTHORIZATION, format!("Bearer {ana}"))
        .header(header::ACCEPT, "text/event-stream")
        .header("last-event-id", "1")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let mut body = response.into_body();
    let first = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(first.data_ref().unwrap()).into_owned();
    assert!(
        text.contains("id: 2") && text.contains("event: audit"),
        "{text}"
    );
}

#[tokio::test]
async fn managing_accounts_lists_and_ends_their_sessions() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    for query in [
        "CREATE ROLE hr",
        "GRANT manage_accounts ON SERVER TO hr",
        "CREATE USER helen SET PASSWORD 'correct horse battery'",
        "CREATE USER dan SET PASSWORD 'correct horse battery'",
        "CREATE USER root SET PASSWORD 'correct horse battery' SET SUPERUSER true",
        "GRANT ROLE hr TO helen",
    ] {
        let (code, body) = call(
            &app,
            Some(OWNER),
            "POST",
            "/v1/default/query",
            Some(json!({ "query": query })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{query}: {body}");
    }
    let mut tokens = std::collections::HashMap::new();
    for name in ["helen", "dan", "root"] {
        let (_, session) = call(
            &app,
            None,
            "POST",
            "/v1/auth/signin",
            Some(json!({ "name": name, "password": "correct horse battery" })),
        )
        .await;
        tokens.insert(name, session);
    }
    let helen = tokens["helen"]["access_token"].as_str().unwrap();
    let (code, list) = call(
        &app,
        Some(helen),
        "GET",
        "/v1/auth/sessions?account=*",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    let accounts: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["account"].as_str().unwrap())
        .collect();
    assert!(
        accounts.contains(&"dan") && accounts.contains(&"helen"),
        "{list}"
    );
    assert!(
        !accounts.contains(&"root"),
        "a superuser's sessions are not hers to see: {list}"
    );
    let dan_session = tokens["dan"]["session"].as_str().unwrap();
    let (code, _) = call(
        &app,
        Some(helen),
        "DELETE",
        &format!("/v1/auth/sessions/{dan_session}"),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let root_session = tokens["root"]["session"].as_str().unwrap();
    let (code, _) = call(
        &app,
        Some(helen),
        "DELETE",
        &format!("/v1/auth/sessions/{root_session}"),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // Ended by helen: the event says so.
    let all = events(&app).await;
    let ended = all
        .iter()
        .find(|e| e["kind"] == "session_ended" && e["target"] == "dan")
        .unwrap();
    assert_eq!(ended["account"], "helen");
    assert!(ended["via"].as_str().unwrap().starts_with("session:"));
}

/// Found by an outside review: every request with a wrong key wrote an
/// event, so a flood of them was a flood of writes. Ten a minute from one
/// source are written; the rest are counted, and the next one written says
/// how many were left out.
#[tokio::test]
async fn a_flood_of_refusals_is_counted_not_written() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(OWNER),
    ));
    for _ in 0..40 {
        let (code, _) = call(&app, Some("not-a-key"), "GET", "/v1/namespaces", None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }
    let all = events(&app).await;
    assert_eq!(count(&all, "refused"), 10, "{all:?}");
    let (code, metrics) = call(&app, Some(OWNER), "GET", "/v1/metrics", None).await;
    assert_eq!(code, StatusCode::OK);
    let metrics = metrics.as_str().unwrap_or_default().to_owned();
    assert!(
        metrics.contains("grafeno_audit_refusals_left_out_total 30"),
        "{metrics}"
    );
}
