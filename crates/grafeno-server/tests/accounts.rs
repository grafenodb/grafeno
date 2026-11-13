//! Phase 14.1: accounts and sign-in over HTTP. The first account of an
//! open server; signing in, refreshing (tokens replaced, a refresh token
//! used twice ends the session), expiry, signing out and ending sessions;
//! lockouts per account and per address; the password policy and changing
//! one's password; disabled accounts; and keys that belong to accounts:
//! scoped to namespaces or to reading, expired, rotated, made by their own
//! account. Users, service accounts and scoped keys see only what they may.

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

async fn call_from(
    app: &Router,
    key: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
    from: Option<&str>,
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
    if let Some(from) = from {
        let address: SocketAddr = format!("{from}:5000").parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(address));
    }
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
}

async fn call(
    app: &Router,
    key: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call_from(app, key, method, uri, body, None).await
}

async fn signin(app: &Router, name: &str, password: &str) -> (StatusCode, Value) {
    call(
        app,
        None,
        "POST",
        "/v1/auth/signin",
        Some(json!({ "name": name, "password": password })),
    )
    .await
}

fn app_with(token: Option<&str>, session: Duration) -> Router {
    let mut state = AppState::new(Arc::new(Grafeno::in_memory().unwrap()), token);
    state.auth.session = session;
    router(state)
}

#[tokio::test]
async fn the_first_account_of_an_open_server() {
    let app = app_with(None, Duration::from_secs(900));
    let (_, status) = call(&app, None, "GET", "/v1/auth/status", None).await;
    assert_eq!(
        (status["open"].clone(), status["setup"].clone()),
        (json!(true), json!(true))
    );
    // A weak password is refused; a good one makes a superuser, signed in.
    let (code, body) = call(
        &app,
        None,
        "POST",
        "/v1/auth/setup",
        Some(json!({ "name": "root", "password": "password123" })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    let (code, body) = call(
        &app,
        None,
        "POST",
        "/v1/auth/setup",
        Some(json!({ "name": "root", "password": "correct horse battery" })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    let token = body["access_token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("gfs_"));
    assert_eq!(body["account"]["superuser"], true);
    // The server is closed now, and setup is over.
    let (code, _) = call(&app, None, "GET", "/v1/namespaces", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _) = call(
        &app,
        None,
        "POST",
        "/v1/auth/setup",
        Some(json!({ "name": "other", "password": "correct horse battery" })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (_, status) = call(&app, None, "GET", "/v1/auth/status", None).await;
    assert_eq!(status["open"], false);
    let (code, me) = call(&app, Some(&token), "GET", "/v1/whoami", None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        (me["name"].as_str(), me["via"]["kind"].as_str()),
        (Some("root"), Some("session"))
    );
    let (code, _) = call(
        &app,
        Some(&token),
        "POST",
        "/v1/demo/nodes",
        Some(json!({ "labels": ["Note"] })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
}

/// A server with its token, a user `ana` (read on `a`) and an admin user.
async fn world(session: Duration) -> Router {
    let app = app_with(Some(OWNER), session);
    for ns in ["a", "b"] {
        let (code, _) = call(
            &app,
            Some(OWNER),
            "POST",
            &format!("/v1/{ns}/nodes"),
            Some(json!({ "labels": ["Note"], "props": { "name": ns } })),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
    }
    let (code, body) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/accounts",
        Some(
            json!({ "name": "ana", "kind": "user", "password": "her long passphrase",
                     "grants": [{ "namespace": "a", "role": "write" }] }),
        ),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    assert_eq!(body["has_password"], true);
    assert!(body.get("password_hash").is_none());
    app
}

#[tokio::test]
async fn sign_in_refresh_expire_and_end_sessions() {
    let app = world(Duration::from_secs(2)).await;
    let (code, body) = signin(&app, "ana", "wrong password here").await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "{body}");
    let (code, body) = signin(&app, "nobody", "her long passphrase").await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "{body}");
    let (code, first) = signin(&app, "ana", "her long passphrase").await;
    assert_eq!(code, StatusCode::OK, "{first}");
    let access = first["access_token"].as_str().unwrap().to_owned();
    let refresh = first["refresh_token"].as_str().unwrap().to_owned();
    // The session reads and writes what ana may, and nothing else.
    let (code, _) = call(&app, Some(&access), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = call(&app, Some(&access), "GET", "/v1/b/stats", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(
        &app,
        Some(&access),
        "POST",
        "/v1/a/nodes",
        Some(json!({ "labels": ["Mine"] })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let (_, who) = call(
        &app,
        Some(&access),
        "POST",
        "/v1/a/query",
        Some(json!({ "query": "MATCH (n:Mine) RETURN n.@recorded_by" })),
    )
    .await;
    assert_eq!(who["rows"][0][0], "ana", "writes name the account: {who}");
    // Refreshing replaces both tokens.
    let (code, second) = call(
        &app,
        None,
        "POST",
        "/v1/auth/refresh",
        Some(json!({ "refresh_token": refresh })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{second}");
    let access2 = second["access_token"].as_str().unwrap().to_owned();
    let refresh2 = second["refresh_token"].as_str().unwrap().to_owned();
    assert_eq!(second["session"], first["session"]);
    let (code, _) = call(&app, Some(&access), "GET", "/v1/a/stats", None).await;
    assert_eq!(
        code,
        StatusCode::UNAUTHORIZED,
        "the old access token is replaced"
    );
    let (code, _) = call(&app, Some(&access2), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::OK);
    // The access token expires; refreshing gives a new one.
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    let (code, body) = call(&app, Some(&access2), "GET", "/v1/a/stats", None).await;
    assert_eq!(
        (code, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("session_expired"))
    );
    let (code, third) = call(
        &app,
        None,
        "POST",
        "/v1/auth/refresh",
        Some(json!({ "refresh_token": refresh2 })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{third}");
    let access3 = third["access_token"].as_str().unwrap().to_owned();
    // A refresh token used twice ends the session (it was stolen).
    let (code, _) = call(
        &app,
        None,
        "POST",
        "/v1/auth/refresh",
        Some(json!({ "refresh_token": refresh2 })),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _) = call(&app, Some(&access3), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "the session ended");
    // Two sessions: listed; one ends another; signing out ends the last.
    let (_, one) = signin(&app, "ana", "her long passphrase").await;
    let (_, two) = signin(&app, "ana", "her long passphrase").await;
    let one_token = one["access_token"].as_str().unwrap().to_owned();
    let two_token = two["access_token"].as_str().unwrap().to_owned();
    let (code, list) = call(&app, Some(&one_token), "GET", "/v1/auth/sessions", None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2, "{list}");
    assert_eq!(
        list.as_array()
            .unwrap()
            .iter()
            .filter(|s| s["current"] == true)
            .count(),
        1
    );
    let (code, _) = call(
        &app,
        Some(&one_token),
        "DELETE",
        &format!("/v1/auth/sessions/{}", two["session"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let (code, _) = call(&app, Some(&two_token), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _) = call(&app, Some(&one_token), "POST", "/v1/auth/signout", None).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let (code, _) = call(&app, Some(&one_token), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    // Superusers list and end anyone's sessions; others cannot.
    let (_, three) = signin(&app, "ana", "her long passphrase").await;
    let (code, all) = call(
        &app,
        Some(OWNER),
        "GET",
        "/v1/auth/sessions?account=ana",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(all.as_array().unwrap().len(), 1);
    let (code, _) = call(
        &app,
        Some(three["access_token"].as_str().unwrap()),
        "GET",
        "/v1/auth/sessions?account=root",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn lockouts_passwords_and_disabled_accounts() {
    let app = world(Duration::from_secs(900)).await;
    // Five failures lock the account, even for the right password.
    for _ in 0..5 {
        let (code, _) = signin(&app, "ana", "not the password").await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }
    let (code, body) = signin(&app, "ana", "her long passphrase").await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("locked")
    );
    // An address that tries many names is refused too.
    let app = world(Duration::from_secs(900)).await;
    for i in 0..20 {
        let (code, _) = call_from(
            &app,
            None,
            "POST",
            "/v1/auth/signin",
            Some(json!({ "name": format!("guess{i}"), "password": "whatever it is" })),
            Some("10.0.0.9"),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }
    let (code, _) = call_from(
        &app,
        None,
        "POST",
        "/v1/auth/signin",
        Some(json!({ "name": "ana", "password": "her long passphrase" })),
        Some("10.0.0.9"),
    )
    .await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
    let (code, _) = call_from(
        &app,
        None,
        "POST",
        "/v1/auth/signin",
        Some(json!({ "name": "ana", "password": "her long passphrase" })),
        Some("10.0.0.10"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "another address signs in");

    // Changing one's password checks the old one and the policy, and ends
    // the other sessions.
    let (_, keep) = signin(&app, "ana", "her long passphrase").await;
    let (_, other) = signin(&app, "ana", "her long passphrase").await;
    let keep = keep["access_token"].as_str().unwrap().to_owned();
    let other = other["access_token"].as_str().unwrap().to_owned();
    let change =
        |current: &str, new: &str| json!({ "current_password": current, "new_password": new });
    let (code, _) = call(
        &app,
        Some(&keep),
        "POST",
        "/v1/auth/password",
        Some(change("wrong one here", "a brand new passphrase")),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _) = call(
        &app,
        Some(&keep),
        "POST",
        "/v1/auth/password",
        Some(change("her long passphrase", "short")),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(
        &app,
        Some(&keep),
        "POST",
        "/v1/auth/password",
        Some(change("her long passphrase", "a brand new passphrase")),
    )
    .await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let (code, _) = call(&app, Some(&keep), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::OK, "this session goes on");
    let (code, _) = call(&app, Some(&other), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "the other one ended");
    let (code, _) = signin(&app, "ana", "a brand new passphrase").await;
    assert_eq!(code, StatusCode::OK);

    // A disabled account can neither sign in nor use its sessions or keys.
    let (code, key) = call(
        &app,
        Some(&keep),
        "POST",
        "/v1/keys",
        Some(json!({ "name": "ana-laptop" })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{key}");
    let key = key["secret"].as_str().unwrap().to_owned();
    let (code, _) = call(
        &app,
        Some(OWNER),
        "PATCH",
        "/v1/accounts/ana",
        Some(json!({ "disabled": true })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    for credential in [&keep, &key] {
        let (code, _) = call(&app, Some(credential), "GET", "/v1/a/stats", None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }
    let (code, _) = signin(&app, "ana", "a brand new passphrase").await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    // Service accounts have no password and cannot sign in.
    let (code, _) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/accounts",
        Some(json!({ "name": "bot", "kind": "service", "password": "a long passphrase" })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn keys_belong_to_accounts_and_are_scoped() {
    let app = world(Duration::from_secs(900)).await;
    // A service account with write on every `team-*` namespace and on `a`.
    let (code, _) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/accounts",
        Some(json!({ "name": "indexer", "kind": "service",
                     "grants": [{ "namespace": "team-*", "role": "write" }, { "namespace": "a", "role": "write" }] })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let make = |body: Value| {
        let app = app.clone();
        async move {
            let (code, made) = call(&app, Some(OWNER), "POST", "/v1/keys", Some(body)).await;
            assert_eq!(code, StatusCode::CREATED, "{made}");
            made["secret"].as_str().unwrap().to_owned()
        }
    };
    let full = make(json!({ "name": "indexer-full", "account": "indexer" })).await;
    let team_reader = make(json!({ "name": "indexer-team-read", "account": "indexer",
                                    "scope": { "namespaces": ["team-*"], "read_only": true } }))
    .await;
    // The full key does what the account may.
    let (code, _) = call(
        &app,
        Some(&full),
        "POST",
        "/v1/team-x/nodes",
        Some(json!({ "labels": ["Doc"] })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let (code, _) = call(
        &app,
        Some(&full),
        "POST",
        "/v1/a/nodes",
        Some(json!({ "labels": ["Doc"] })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    // The scoped key reads `team-*` only.
    let (code, _) = call(&app, Some(&team_reader), "GET", "/v1/team-x/stats", None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, body) = call(
        &app,
        Some(&team_reader),
        "POST",
        "/v1/team-x/nodes",
        Some(json!({ "labels": ["Doc"] })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("scope"));
    let (code, _) = call(&app, Some(&team_reader), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "outside its scope");
    let (code, _) = call(&app, Some(&team_reader), "GET", "/v1/b/stats", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // A scope cannot widen the account's rights.
    let wide = make(
        json!({ "name": "indexer-b", "account": "indexer", "scope": { "namespaces": ["b"] } }),
    )
    .await;
    let (code, _) = call(&app, Some(&wide), "GET", "/v1/b/stats", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (_, me) = call(&app, Some(&team_reader), "GET", "/v1/whoami", None).await;
    assert_eq!(
        (me["name"].as_str(), me["via"]["name"].as_str()),
        (Some("indexer"), Some("indexer-team-read"))
    );
    assert_eq!(me["scope"]["read_only"], true);

    // Rotating replaces the secret at once.
    let (code, rotated) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/keys/indexer-full/rotate",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rotated}");
    let renewed = rotated["secret"].as_str().unwrap().to_owned();
    let (code, _) = call(&app, Some(&full), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _) = call(&app, Some(&renewed), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::OK);

    // Users make, list, rotate and revoke their own keys, scoped or not;
    // not other accounts' keys.
    let (_, session) = signin(&app, "ana", "her long passphrase").await;
    let ana = session["access_token"].as_str().unwrap().to_owned();
    let (code, mine) = call(
        &app,
        Some(&ana),
        "POST",
        "/v1/keys",
        Some(json!({ "name": "ana-ro", "scope": { "read_only": true } })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{mine}");
    let ana_ro = mine["secret"].as_str().unwrap().to_owned();
    let (code, _) = call(&app, Some(&ana_ro), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = call(
        &app,
        Some(&ana_ro),
        "POST",
        "/v1/a/nodes",
        Some(json!({ "labels": ["Doc"] })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(
        &app,
        Some(&ana),
        "POST",
        "/v1/keys",
        Some(json!({ "name": "steal", "account": "indexer" })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(
        &app,
        Some(&ana),
        "POST",
        "/v1/keys/indexer-full/rotate",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(&app, Some(&ana), "DELETE", "/v1/keys/indexer-full", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(&app, Some(&ana), "DELETE", "/v1/keys/ana-ro", None).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let (code, _) = call(&app, Some(&ana_ro), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "revoked at once");
    let (code, _) = call(&app, Some(&ana), "GET", "/v1/accounts", None).await;
    assert_eq!(
        code,
        StatusCode::FORBIDDEN,
        "managing accounts is for superusers"
    );

    // A key past its expiry stops working.
    let state_app = {
        let mut state = AppState::new(Arc::new(Grafeno::in_memory().unwrap()), Some(OWNER));
        state.auth.session = Duration::from_secs(900);
        router(state)
    };
    let (code, _) = call(&state_app, Some(OWNER), "POST", "/v1/accounts", Some(json!({ "name": "temp", "kind": "service", "grants": [{ "namespace": "*", "role": "read" }] }))).await;
    assert_eq!(code, StatusCode::CREATED);
    let (code, made) = call(
        &state_app,
        Some(OWNER),
        "POST",
        "/v1/keys",
        Some(json!({ "name": "temp-key", "account": "temp", "expires_in_days": 0 })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{made}");
    let (code, body) = call(
        &state_app,
        Some(made["secret"].as_str().unwrap()),
        "GET",
        "/v1/namespaces",
        None,
    )
    .await;
    assert_eq!(
        (code, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("key_expired"))
    );
}

/// Found by an outside review: a key scoped to reading one namespace, of a
/// superuser's account, made a new key with no scope, and with it every
/// right of the account.
#[tokio::test]
async fn a_key_made_with_a_scoped_key_reaches_no_further() {
    let app = world(Duration::from_secs(900)).await;
    let (code, _) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/accounts",
        Some(json!({ "name": "boss", "kind": "service", "superuser": true })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let (code, made) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/keys",
        Some(json!({ "name": "narrow", "account": "boss",
                     "scope": { "namespaces": ["a"], "read_only": true } })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{made}");
    let narrow = made["secret"].as_str().unwrap().to_owned();
    let (code, _) = call(&app, Some(&narrow), "GET", "/v1/accounts", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);

    // No scope asked: the new key takes the caller's, not the account's.
    let (code, made) = call(
        &app,
        Some(&narrow),
        "POST",
        "/v1/keys",
        Some(json!({ "name": "escaped", "account": "boss" })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{made}");
    assert_eq!(made["key"]["scope"]["namespaces"], json!(["a"]), "{made}");
    assert_eq!(made["key"]["scope"]["read_only"], true);
    let escaped = made["secret"].as_str().unwrap().to_owned();
    let (code, _) = call(&app, Some(&escaped), "GET", "/v1/accounts", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(&app, Some(&escaped), "GET", "/v1/b/stats", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = call(&app, Some(&escaped), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::OK);

    // A wider scope is refused, and so is writing where it only reads.
    for scope in [
        json!({ "namespaces": ["b"], "read_only": true }),
        json!({ "namespaces": ["*"], "read_only": true }),
        json!({ "namespaces": ["a"], "read_only": false }),
    ] {
        let (code, body) = call(
            &app,
            Some(&narrow),
            "POST",
            "/v1/keys",
            Some(json!({ "name": "wider", "account": "boss", "scope": scope })),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{scope} {body}");
    }
    // Rotating keeps the scope, as it did.
    let (code, rotated) = call(&app, Some(&narrow), "POST", "/v1/keys/narrow/rotate", None).await;
    assert_eq!(code, StatusCode::OK, "{rotated}");
    assert_eq!(rotated["key"]["scope"]["namespaces"], json!(["a"]));
}

/// Found by an outside review: forty wrong passwords in parallel all got
/// past a limit of five, since each was counted only after its check.
#[tokio::test]
async fn attempts_in_parallel_count_before_their_passwords_are_checked() {
    let app = world(Duration::from_secs(900)).await;
    let mut tries = Vec::new();
    for _ in 0..40 {
        let app = app.clone();
        tries.push(tokio::spawn(async move {
            call_from(
                &app,
                None,
                "POST",
                "/v1/auth/signin",
                Some(json!({ "name": "ana", "password": "not the password" })),
                Some("10.1.1.1"),
            )
            .await
            .0
        }));
    }
    let mut unauthorized = 0;
    let mut refused = 0;
    for attempt in tries {
        match attempt.await.unwrap() {
            StatusCode::UNAUTHORIZED => unauthorized += 1,
            StatusCode::TOO_MANY_REQUESTS => refused += 1,
            other => panic!("{other}"),
        }
    }
    assert_eq!(unauthorized, 5, "only the account's five tries are checked");
    assert_eq!(refused, 35);

    // A right password after a few wrong ones still signs in, and clears
    // the account's count.
    let app = world(Duration::from_secs(900)).await;
    for _ in 0..3 {
        signin(&app, "ana", "not the password").await;
    }
    let (code, _) = signin(&app, "ana", "her long passphrase").await;
    assert_eq!(code, StatusCode::OK);
    for _ in 0..4 {
        let (code, _) = signin(&app, "ana", "not the password").await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }

    // Names, passwords and bodies past their size are refused before any
    // work is done on them.
    let (code, _) = signin(&app, &"n".repeat(300), "whatever it is").await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _) = signin(&app, "ana", &"p".repeat(2_000)).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, body) = call(
        &app,
        None,
        "POST",
        "/v1/auth/signin",
        Some(json!({ "name": "ana", "password": "x".repeat(40_000) })),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("length limit exceeded"), "{body}");
}

/// Found by an outside review: an account that may only read began a
/// writing transaction, which holds the one writer until it ends.
#[tokio::test]
async fn a_reader_begins_only_a_reading_transaction() {
    let app = world(Duration::from_secs(900)).await;
    let (code, _) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/accounts",
        Some(
            json!({ "name": "reader", "kind": "user", "password": "quiet orchard lamp",
                     "grants": [{ "namespace": "a", "role": "read" }] }),
        ),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let (_, session) = signin(&app, "reader", "quiet orchard lamp").await;
    let reader = session["access_token"].as_str().unwrap().to_owned();

    let (code, body) = call(&app, Some(&reader), "POST", "/v1/a/transactions", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("read_only")
    );
    let (code, body) = call(
        &app,
        Some(&reader),
        "POST",
        "/v1/a/transactions",
        Some(json!({ "read_only": true })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{body}");

    // The writer is free: the owner writes at once.
    let (code, _) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/a/nodes",
        Some(json!({ "labels": ["Doc"] })),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    // Who may write still begins a writing one.
    let (_, session) = signin(&app, "ana", "her long passphrase").await;
    let ana = session["access_token"].as_str().unwrap().to_owned();
    let (code, body) = call(&app, Some(&ana), "POST", "/v1/a/transactions", None).await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
}

/// Found by an outside review: exporting every namespace needed only the
/// right to read them, though exporting one by name needs the right to
/// export it.
#[tokio::test]
async fn exporting_everything_needs_the_right_to_export() {
    let app = world(Duration::from_secs(900)).await;
    let (code, _) = call(
        &app,
        Some(OWNER),
        "POST",
        "/v1/accounts",
        Some(
            json!({ "name": "reader", "kind": "user", "password": "quiet orchard lamp",
                     "grants": [{ "namespace": "a", "role": "read" }] }),
        ),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED);
    let (_, session) = signin(&app, "reader", "quiet orchard lamp").await;
    let reader = session["access_token"].as_str().unwrap().to_owned();
    let (code, _) = call(&app, Some(&reader), "GET", "/v1/a/stats", None).await;
    assert_eq!(code, StatusCode::OK);
    // A namespace by name and everything agree.
    let (named, _) = call(&app, Some(&reader), "GET", "/v1/export?ns=a", None).await;
    let (all, body) = call(&app, Some(&reader), "GET", "/v1/export", None).await;
    assert_eq!(all, named, "{body}");
    // An owner exports everything.
    let (code, _) = call(&app, Some(OWNER), "GET", "/v1/export", None).await;
    assert_eq!(code, StatusCode::OK);
}
