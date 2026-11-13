//! The audit log over HTTP (phase 14.5): recording what happens outside the
//! database (refused requests, failed sign-ins, lockouts), reading the log
//! (`GET /v1/audit`: a page, a server-sent events stream, or JSON Lines),
//! and, on a cluster, taking events from the other members to the leader.
//!
//! Changes to accounts, keys, sessions and roles are audited by the engine,
//! in the transaction that makes them; what is recorded here comes from the
//! server alone. On a cluster member that does not lead, an event goes to
//! the leader (with the cluster secret), so it is written once, in the
//! replicated log, wherever it happened.

use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use grafeno::{AuditPage, NewAuditEvent, Outcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth;
use crate::http::{ApiError, ApiResult, AppState};

/// The header that carries the cluster secret between members.
const SECRET_HEADER: &str = "x-grafeno-cluster";

/// An event as it travels to the leader.
#[derive(Serialize, Deserialize)]
pub struct Wire {
    kind: String,
    account: Option<String>,
    via: Option<String>,
    address: Option<String>,
    target: Option<String>,
    outcome: Outcome,
    detail: Value,
}

impl From<&NewAuditEvent> for Wire {
    fn from(event: &NewAuditEvent) -> Self {
        Self {
            kind: event.kind.clone(),
            account: event.account.clone(),
            via: event.via.clone(),
            address: event.address.clone(),
            target: event.target.clone(),
            outcome: event.outcome,
            detail: event.detail.clone(),
        }
    }
}

impl From<Wire> for NewAuditEvent {
    fn from(wire: Wire) -> Self {
        Self {
            kind: wire.kind,
            account: wire.account,
            via: wire.via,
            address: wire.address,
            target: wire.target,
            outcome: wire.outcome,
            detail: wire.detail,
        }
    }
}

/// Writes `event` to the audit log: here, or on a cluster's leader. A
/// failure is reported on standard error; the request it describes goes on.
pub async fn record(state: &AppState, event: NewAuditEvent) {
    let writer = state.writer.clone();
    let local = event.clone();
    let written = tokio::task::spawn_blocking(move || {
        writer.write(move |tx| tx.audit(local.clone()).map(|_| ()))
    })
    .await;
    let failed = match written {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error.to_string(),
        Err(error) => error.to_string(),
    };
    // A member that does not lead hands the event to the leader.
    if let (Some(node), Some(secret)) = (state.writer.cluster(), state.writer.secret())
        && let Some(leader) = node.status().leader
        && let Some(api) = state.writer.api_of(leader)
    {
        let body = serde_json::to_string(&Wire::from(&event)).unwrap_or_default();
        let secret = secret.to_string();
        let sent = tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(5)))
                .http_status_as_error(false)
                .build()
                .into();
            agent
                .post(&format!("http://{api}/v1/cluster/audit"))
                .header(SECRET_HEADER, &secret)
                .header("content-type", "application/json")
                .send(body.as_str())
                .map(|response| response.status().is_success())
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        if sent {
            return;
        }
    }
    eprintln!(
        "grafeno: an audit event ({}) was not recorded: {failed}",
        event.kind
    );
}

/// `POST /v1/cluster/audit`: an event from another member, written here
/// (the leader). Members authenticate with the cluster secret.
pub async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(wire): Json<Wire>,
) -> Response {
    let given = headers
        .get(SECRET_HEADER)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    let Some(secret) = state.writer.secret() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !crate::cluster::same_secret(given, secret.as_bytes()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let event = NewAuditEvent::from(wire);
    let writer = state.writer.clone();
    match tokio::task::spawn_blocking(move || {
        writer.write(move |tx| tx.audit(event.clone()).map(|_| ()))
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// A refused request, as the audit log keeps it.
pub fn refusal(method: &str, path: &str, code: &str, message: &str) -> NewAuditEvent {
    NewAuditEvent::new("refused")
        .target(format!("{method} {path}"))
        .outcome(Outcome::Refused)
        .detail(json!({ "code": code, "message": message }))
}

#[derive(Deserialize)]
pub struct AuditQuery {
    since: Option<u64>,
    limit: Option<usize>,
    /// `jsonl`: every event after `since`, one JSON object a line.
    format: Option<String>,
}

/// `GET /v1/audit` (the `audit` privilege): a page of events after `since`;
/// with `Accept: text/event-stream`, a stream that goes on as events are
/// appended; with `format=jsonl`, JSON Lines.
pub async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Response> {
    let wants_stream = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let since = resume.or(query.since).unwrap_or(0);
    if query.format.as_deref() == Some("jsonl") {
        let db = state.db.clone();
        let lines = tokio::task::spawn_blocking(move || -> grafeno::Result<String> {
            let read = db.begin_read()?;
            let mut out = String::new();
            let mut since = since;
            loop {
                let page = read.audit(since, 1000)?;
                if page.events.is_empty() {
                    return Ok(out);
                }
                for event in &page.events {
                    out.push_str(&serde_json::to_string(event).unwrap_or_default());
                    out.push('\n');
                    since = event.seq;
                }
            }
        })
        .await
        .map_err(|error| ApiError::internal(error.to_string()))??;
        return Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], lines).into_response());
    }
    if !wants_stream {
        let limit = query.limit.unwrap_or(500).min(10_000);
        let db = state.db.clone();
        let page: AuditPage =
            tokio::task::spawn_blocking(move || db.begin_read()?.audit(since, limit))
                .await
                .map_err(|error| ApiError::internal(error.to_string()))??;
        return Ok(Json(page).into_response());
    }
    let db = state.db.clone();
    let stream = futures_util::stream::unfold(
        (
            db,
            since,
            std::collections::VecDeque::<axum::response::sse::Event>::new(),
        ),
        |(db, mut cursor, mut queue)| async move {
            use axum::response::sse::Event;
            loop {
                if let Some(event) = queue.pop_front() {
                    return Some((
                        Ok::<_, std::convert::Infallible>(event),
                        (db, cursor, queue),
                    ));
                }
                let reader = db.clone();
                let page =
                    tokio::task::spawn_blocking(move || reader.begin_read()?.audit(cursor, 500))
                        .await
                        .ok()?
                        .ok()?;
                for event in page.events {
                    cursor = event.seq;
                    queue.push_back(
                        Event::default()
                            .id(event.seq.to_string())
                            .event("audit")
                            .data(serde_json::to_string(&event).unwrap_or_default()),
                    );
                }
                if queue.is_empty() {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        },
    );
    Ok(axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response())
}

/// The origin of the current request's writes (who, with what, from where).
pub fn origin() -> Option<grafeno::Origin> {
    let caller = auth::current();
    let address = auth::address();
    match &caller {
        auth::Caller::Anonymous => address.map(|address| grafeno::Origin {
            address: Some(address),
            ..grafeno::Origin::default()
        }),
        auth::Caller::Owner => Some(grafeno::Origin {
            account: None,
            via: Some("token".into()),
            address,
        }),
        auth::Caller::Account { account, via, .. } => Some(grafeno::Origin {
            account: Some(account.name.clone()),
            via: Some(match via {
                auth::Via::Key(name) => format!("key:{name}"),
                auth::Via::Session(id) => format!("session:{id}"),
            }),
            address,
        }),
    }
}
