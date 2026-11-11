//! Following the writes over HTTP: the change log as JSON and as
//! server-sent events, NDJSON ingestion with idempotency keys, signed
//! webhooks, and background work that runs from the log.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use grafeno::{Grafeno, NewNode};
use grafeno_server::events::{self, BackgroundConfig};
use grafeno_server::http::{AppState, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: Body,
    kind: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, kind)
        .body(body)
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)))
    };
    (status, value)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    send(app, method, uri, body, "application/json").await
}

#[tokio::test]
async fn changes_as_a_page_and_as_a_stream() {
    let db = Arc::new(Grafeno::in_memory().unwrap());
    let app = router(AppState::new(db.clone(), None));
    for name in ["Ana", "Bruno"] {
        let (status, _) = call(
            &app,
            "POST",
            "/v1/k/nodes",
            Some(json!({ "labels": ["Person"], "props": { "name": name } })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, page) = call(&app, "GET", "/v1/k/changes?since=0", None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["last"], 2);
    assert_eq!(page["changes"][0]["op"], "created");
    assert_eq!(page["changes"][1]["labels"][0], "Person");
    let (_, page) = call(&app, "GET", "/v1/k/changes?since=1", None).await;
    assert_eq!(page["changes"].as_array().unwrap().len(), 1);

    // The stream sends what is there, then what is committed later; it
    // resumes after `Last-Event-ID`.
    let request = Request::builder()
        .uri("/v1/k/changes")
        .header(header::ACCEPT, "text/event-stream")
        .header("last-event-id", "1")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut text = String::new();
    let mut read_until = async |needle: &str, text: &mut String| {
        while !text.contains(needle) {
            let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
                .await
                .expect("an event in time")
                .unwrap()
                .unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(&String::from_utf8_lossy(&data));
            }
        }
    };
    read_until("id: 2", &mut text).await;
    assert!(!text.contains("id: 1\n"), "{text}");
    db.write(|tx| {
        tx.create_node("k", NewNode::new("Person").prop("name", "Carla"))
            .map(|_| ())
    })
    .unwrap();
    read_until("id: 3", &mut text).await;
    assert!(text.contains("event: change"), "{text}");
}

#[tokio::test]
async fn ndjson_ingestion_is_idempotent() {
    let db = Arc::new(Grafeno::in_memory().unwrap());
    let app = router(AppState::new(db.clone(), None));
    let lines = [
        json!({ "key": "ana", "node": { "labels": ["Person"], "props": { "name": "Ana" } } }),
        json!({ "key": "acme", "node": { "labels": ["Company"], "props": { "name": "Acme" } } }),
        json!({ "key": "job", "edge": { "from": "$ana", "label": "works_at", "to": "$acme" } }),
        json!({ "edge": { "from": "$nobody", "label": "knows", "to": "$ana" } }),
    ];
    let mut body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    body.push_str("not json\n");
    let (status, report) = send(
        &app,
        "POST",
        "/v1/k/ingest",
        Body::from(body.clone()),
        "application/x-ndjson",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(
        (
            report["lines"].as_u64(),
            report["nodes"].as_u64(),
            report["edges"].as_u64()
        ),
        (Some(5), Some(2), Some(1)),
        "{report}"
    );
    assert_eq!(report["failed"], 2);
    assert_eq!(report["errors"][0]["line"], 4);
    assert_eq!(report["errors"][1]["line"], 5);

    // The same request again (a retry): nothing is written twice.
    let (_, again) = send(
        &app,
        "POST",
        "/v1/k/ingest",
        Body::from(body),
        "application/x-ndjson",
    )
    .await;
    assert_eq!(again["duplicates"], 3, "{again}");
    assert_eq!(again["nodes"], 0);
    // A later request can point at a key ingested before.
    let later =
        json!({ "key": "bob", "node": { "labels": ["Person"], "props": { "name": "Bob" } } })
            .to_string()
            + "\n"
            + &json!({ "edge": { "from": "$bob", "label": "knows", "to": "$ana" } }).to_string();
    let (_, later) = send(
        &app,
        "POST",
        "/v1/k/ingest",
        Body::from(later),
        "application/x-ndjson",
    )
    .await;
    assert_eq!(
        (
            later["nodes"].as_u64(),
            later["edges"].as_u64(),
            later["failed"].as_u64()
        ),
        (Some(1), Some(1), Some(0)),
        "{later}"
    );
    let stats = db.begin_read().unwrap().stats("k").unwrap();
    assert_eq!((stats.nodes, stats.edges), (3, 2));

    // Many lines, in several transactions.
    let many: String = (0..1_234)
        .map(|i| json!({ "key": format!("n{i}"), "node": { "labels": ["Note"], "text": format!("note {i}") } }).to_string() + "\n")
        .collect();
    let (_, many) = send(
        &app,
        "POST",
        "/v1/k/ingest",
        Body::from(many),
        "application/x-ndjson",
    )
    .await;
    assert_eq!(many["nodes"], 1_234, "{many}");
}

#[tokio::test]
async fn webhooks_receive_signed_batches() {
    // A receiver on a local port.
    let received: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let sink = received.clone();
    let receiver = Router::new().route(
        "/hook",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let sink = sink.clone();
                async move {
                    let signature = headers
                        .get("x-grafeno-signature")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    assert_eq!(
                        signature,
                        format!("sha256={}", events::sign(b"s3cret", &body)),
                        "signed body"
                    );
                    sink.lock()
                        .unwrap()
                        .push((signature, serde_json::from_slice(&body).unwrap()));
                    StatusCode::OK
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, receiver).await.unwrap() });

    let db = Arc::new(Grafeno::in_memory().unwrap());
    let state = AppState::new(db.clone(), None);
    let app = router(state.clone());
    db.write(|tx| tx.create_node("k", NewNode::new("Person")).map(|_| ()))
        .unwrap();
    let (status, hook) = call(
        &app,
        "POST",
        "/v1/k/webhooks",
        Some(json!({ "url": format!("http://127.0.0.1:{port}/hook"), "secret": "s3cret", "labels": ["Person"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{hook}");
    assert_eq!(hook["cursor"], 1, "starts after what is already there");
    assert!(hook.get("secret").is_none() && hook["signed"] == true);
    db.write(|tx| {
        tx.create_node("k", NewNode::new("Person"))?;
        tx.create_node("k", NewNode::new("Note"))?;
        tx.create_node("k", NewNode::new("Person")).map(|_| ())
    })
    .unwrap();

    let deliver = |state: AppState| async move {
        tokio::task::spawn_blocking(move || {
            events::deliver(&state, &mut Default::default()).unwrap()
        })
        .await
        .unwrap()
    };
    assert_eq!(
        deliver(state.clone()).await,
        2,
        "the two people, not the note"
    );
    assert_eq!(deliver(state.clone()).await, 0, "nothing new");
    let got = received.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1["changes"].as_array().unwrap().len(), 2);
    assert_eq!(got[0].1["namespace"], "k");
    let id = hook["id"].as_str().unwrap();
    let (_, hook) = call(&app, "GET", &format!("/v1/k/webhooks/{id}"), None).await;
    assert_eq!(
        (hook["cursor"].as_u64(), hook["delivered"].as_u64()),
        (Some(4), Some(2)),
        "{hook}"
    );

    // A failing URL keeps its place and records why.
    let (_, broken) = call(
        &app,
        "POST",
        "/v1/k/webhooks",
        Some(json!({ "url": "http://127.0.0.1:9/nothing", "since": 0 })),
    )
    .await;
    deliver(state.clone()).await;
    let bid = broken["id"].as_str().unwrap();
    let (_, broken) = call(&app, "GET", &format!("/v1/k/webhooks/{bid}"), None).await;
    assert_eq!(broken["cursor"], 0);
    assert_eq!(broken["failures"], 1);
    assert!(broken["last_error"].is_string());
    let (status, _) = call(&app, "DELETE", &format!("/v1/k/webhooks/{bid}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn background_work_runs_from_the_log() {
    let db = Arc::new(Grafeno::in_memory().unwrap());
    let state = AppState::new(db.clone(), None).with_background(BackgroundConfig {
        extract: vec!["k".into()],
        communities: vec!["k".into()],
        communities_every_secs: Some(0),
        communities_min_changes: Some(5),
    });
    db.write(|tx| {
        let ana = tx.create_node("k", NewNode::new("Person").prop("name", "Ana Torres"))?;
        let people: Vec<_> = ["Bruno", "Carla", "Dario"]
            .iter()
            .map(|n| tx.create_node("k", NewNode::new("Person").prop("name", *n)))
            .collect::<grafeno::Result<_>>()?;
        for p in &people {
            tx.create_edge("k", grafeno::NewEdge::new(*p, "works_with", ana))?;
        }
        tx.create_edge(
            "k",
            grafeno::NewEdge::new(people[0], "works_with", people[1]),
        )?;
        tx.create_node(
            "k",
            NewNode::new("Memory").text("Lunch with Ana Torres about the plan"),
        )
        .map(|_| ())
    })
    .unwrap();
    let step = |state: AppState, task: &'static str| async move {
        tokio::task::spawn_blocking(move || events::step(&state, "k", task, &mut None).unwrap())
            .await
            .unwrap()
    };
    let extracted = step(state.clone(), "extract")
        .await
        .expect("a text changed");
    assert_eq!(extracted["linked"], 1, "{extracted}");
    assert!(
        step(state.clone(), "extract").await.is_none(),
        "nothing new to read"
    );
    let summarized = step(state.clone(), "communities")
        .await
        .expect("enough changes");
    assert!(summarized["created"].as_u64().unwrap() >= 1, "{summarized}");
    // Its own summaries do not call for another refresh.
    assert!(step(state.clone(), "communities").await.is_none());
}

#[tokio::test]
async fn subscriptions_carry_values_over_http() {
    let db = Arc::new(Grafeno::in_memory().unwrap());
    let state = AppState::new(db.clone(), None);
    let app = router(state.clone());
    let query = |text: &str| json!({ "query": text });
    // Off by default.
    let (status, refused) = call(
        &app,
        "POST",
        "/v1/k/query",
        Some(query("CREATE EVENT SUBSCRIPTION moves ON PROPERTIES")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused.to_string().contains("[events] enabled"),
        "{refused}"
    );

    db.enable_events(true);
    // A receiver for the webhook subscription.
    let received: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = received.clone();
    let receiver = Router::new().route(
        "/hook",
        axum::routing::post(move |body: axum::body::Bytes| {
            let sink = sink.clone();
            async move {
                sink.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, receiver).await.unwrap() });

    for text in [
        "CREATE EVENT SUBSCRIPTION moves ON PROPERTIES FOR :Person KEYS city".to_owned(),
        format!(
            "CREATE EVENT SUBSCRIPTION lima ON NODES WHERE p.city = 'Lima' DELIVER TO WEBHOOK 'http://127.0.0.1:{port}/hook'"
        ),
    ] {
        let (status, body) = call(&app, "POST", "/v1/k/query", Some(query(&text))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (_, shown) = call(
        &app,
        "POST",
        "/v1/k/query",
        Some(query("SHOW EVENT SUBSCRIPTIONS")),
    )
    .await;
    assert_eq!(shown["rows"].as_array().unwrap().len(), 2, "{shown}");
    let (status, _) = call(
        &app,
        "POST",
        "/v1/k/query",
        Some(query("CREATE EVENT SUBSCRIPTION moves ON NODES")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "already there");

    for text in [
        "CREATE (:Person {name: 'Ana', city: 'Lima'})",
        "MATCH (p:Person {name: 'Ana'}) SET p.city = 'Cusco'",
        "CREATE (:Person {name: 'Eva', city: 'Quito'})",
    ] {
        let (status, body) = call(&app, "POST", "/v1/k/query", Some(query(text))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, page) = call(&app, "GET", "/v1/k/events/moves?since=0", None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let events = page["changes"].as_array().unwrap();
    assert_eq!(events.len(), 3, "{page}");
    assert_eq!(events[1]["values"]["before"]["city"], "Lima");
    assert_eq!(events[1]["values"]["after"]["city"], "Cusco");
    assert_eq!(page["through"], 3);
    let (status, _) = call(&app, "GET", "/v1/k/events/nothing", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // The change log shows no values.
    let (_, log) = call(&app, "GET", "/v1/k/changes", None).await;
    assert!(log["changes"][1].get("values").is_none(), "{log}");

    // The stream.
    let request = Request::builder()
        .uri("/v1/k/events/moves?stream=true&since=1")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut text = String::new();
    while !text.contains("id: 3") {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("an event in time")
            .unwrap()
            .unwrap();
        if let Ok(data) = frame.into_data() {
            text.push_str(&String::from_utf8_lossy(&data));
        }
    }
    assert!(
        text.contains("event: event") && text.contains("Cusco"),
        "{text}"
    );

    // The webhook gets Ana while she was in Lima: created, then moved
    // (before Lima), and not Eva.
    let delivered = tokio::task::spawn_blocking({
        let state = state.clone();
        move || events::deliver(&state, &mut Default::default()).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(delivered, 2);
    let got = received.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["subscription"], "lima");
    assert_eq!(got[0]["events"][1]["values"]["after"]["city"], "Cusco");
    let again = tokio::task::spawn_blocking({
        let state = state.clone();
        move || events::deliver(&state, &mut Default::default()).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(again, 0, "its place is kept");

    let (status, body) = call(
        &app,
        "POST",
        "/v1/k/query",
        Some(query("DROP EVENT SUBSCRIPTION moves")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(
        &app,
        "POST",
        "/v1/k/query",
        Some(query("DROP EVENT SUBSCRIPTION moves IF EXISTS")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
