//! The static token is the administrator's credential, with no account.
//! It can be changed while the server runs, by whoever shows the current
//! one, and the change is kept in the settings file it came from.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::{Grafeno, NodeId};
use grafeno_server::http::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const FIRST: &str = "the-first-token-of-this-test";

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

fn settings_file() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("grafeno-token-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("grafeno.toml");
    std::fs::write(
        &path,
        format!(
            "# Kept as it is.\naddr = \"127.0.0.1:7810\"\ntoken = \"{FIRST}\"\n\n[query]\ntimeout_secs = 30.0\n"
        ),
    )
    .unwrap();
    path
}

#[tokio::test]
async fn the_token_changes_itself_given_the_current_one() {
    let path = settings_file();
    let app = router(
        AppState::new(Arc::new(Grafeno::in_memory().unwrap()), Some(FIRST))
            .with_token_file(Some(path.clone())),
    );

    // The token is the administrator: it makes users, with no account of
    // its own.
    let (code, body) = call(
        &app,
        Some(FIRST),
        "POST",
        "/v1/accounts",
        Some(
            json!({ "name": "root", "kind": "user", "password": "a long passphrase here",
                     "superuser": true }),
        ),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    let (code, body) = call(
        &app,
        None,
        "POST",
        "/v1/auth/signin",
        Some(json!({ "name": "root", "password": "a long passphrase here" })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let session = body["access_token"].as_str().unwrap().to_owned();

    // A superuser's account may not change the token, even knowing it.
    let (code, body) = call(
        &app,
        Some(&session),
        "POST",
        "/v1/auth/token",
        Some(json!({ "current_token": FIRST })),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body}");

    // The token itself, with a wrong current one: nothing changes.
    let (code, body) = call(
        &app,
        Some(FIRST),
        "POST",
        "/v1/auth/token",
        Some(json!({ "current_token": "not-the-token" })),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "{body}");
    assert!(std::fs::read_to_string(&path).unwrap().contains(FIRST));

    // With the current one: a new token, at once and in the file.
    let (code, body) = call(
        &app,
        Some(FIRST),
        "POST",
        "/v1/auth/token",
        Some(json!({ "current_token": FIRST })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let new = body["token"].as_str().unwrap().to_owned();
    assert_ne!(new, FIRST);
    let (code, _) = call(&app, Some(FIRST), "GET", "/v1/whoami", None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "the old token is gone");
    let (code, body) = call(&app, Some(&new), "GET", "/v1/whoami", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(&format!("token = \"{new}\"")), "{text}");
    assert!(!text.contains(FIRST), "{text}");
    assert!(text.starts_with("# Kept as it is.\n"), "{text}");
    assert!(text.contains("[query]\ntimeout_secs = 30.0"), "{text}");
    let config = grafeno_server::config::Config::load(&path).unwrap();
    assert_eq!(config.token.as_deref(), Some(new.as_str()));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn a_token_from_the_environment_is_changed_there() {
    let app = router(AppState::new(
        Arc::new(Grafeno::in_memory().unwrap()),
        Some(FIRST),
    ));
    let (code, body) = call(
        &app,
        Some(FIRST),
        "POST",
        "/v1/auth/token",
        Some(json!({ "current_token": FIRST })),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    let (code, _) = call(&app, Some(FIRST), "GET", "/v1/whoami", None).await;
    assert_eq!(code, StatusCode::OK, "the token is unchanged");
}
