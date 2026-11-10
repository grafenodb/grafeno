//! What this engine is and what it can do, for clients that must work with
//! more than one version of it (phase 15.3): `GET /v1/about` (public) gives
//! the version, the API version and the capabilities; `GET /v1/settings`
//! (superusers) gives the settings it runs with, without secrets; and every
//! response carries the API version in `x-grafeno-api`.
//!
//! The API version changes only when a change would break a client written
//! for the previous one (a route removed, a field renamed or retyped);
//! additions keep it and appear as capabilities instead, so that a client
//! can turn off what an older engine lacks.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{Value, json};

use crate::http::AppState;

/// The version of the HTTP API this engine speaks.
pub const API_VERSION: u32 = 1;

/// Every API version this engine answers to (a client may speak any).
pub const API_VERSIONS: &[u32] = &[1];

/// The header every response carries with [`API_VERSION`].
pub const API_HEADER: &str = "x-grafeno-api";

/// What this engine can do, by name: the features a client may need to
/// check before offering them. A name never changes meaning; new features
/// add names.
pub const CAPABILITIES: &[&str] = &[
    // Graph, text and vectors (phases 1 to 3).
    "namespaces",
    "nodes",
    "edges",
    "facts",
    // Null stored as a value, and properties removed by name (16.2).
    "null_values",
    "search.text",
    "search.vector",
    // Memory (phase 5) and recall.
    "recall",
    "context",
    "memory",
    // Queries (phases 7 and 12): GQ, EXPLAIN, batches.
    "query",
    "query.explain",
    // BEGIN, COMMIT and ROLLBACK across requests (16.4).
    "transactions",
    "batches",
    "traversal",
    "algorithms",
    // Knowledge upkeep (phase 11).
    "resolution",
    "match-rules",
    "extraction",
    "constraints",
    "communities",
    "summaries",
    "indexes",
    // Composite, edge and multi-label indexes; not-null, type and key
    // constraints (16.5).
    "indexes.advanced",
    // Changes and ingestion (11.4).
    "changes",
    "changes.stream",
    "webhooks",
    "ingest",
    // Moving data.
    "export",
    "import",
    "import.csv",
    "import.graphml",
    "backup",
    // Operations.
    "metrics",
    "settings",
    // Access (phase 14).
    "accounts",
    "sessions",
    "keys",
    "roles",
    "grants",
    "policies",
    "audit",
    // Clusters (phase 13).
    "cluster",
    "cluster.groups",
    "query.split",
];

/// `GET /v1/about` (public).
pub async fn about(State(state): State<AppState>) -> Json<Value> {
    let accounts = state.keys.has_accounts();
    let cluster = state.writer.cluster().map(|node| {
        let status = node.status();
        json!({ "id": status.id, "group": state.group })
    });
    Json(json!({
        "product": "grafeno",
        "version": env!("CARGO_PKG_VERSION"),
        "api": { "version": API_VERSION, "supported": API_VERSIONS },
        "layout": grafeno::SCHEMA_VERSION,
        "capabilities": CAPABILITIES,
        "auth": {
            "token": state.token.is_some(),
            "accounts": accounts,
            "open": state.token.is_none() && !accounts,
            "password_sign_in": true,
        },
        "cluster": cluster,
    }))
}

/// `GET /v1/settings` (superusers): what the server runs with, from its
/// settings file, flags and defaults, without the token or the cluster
/// secret.
pub async fn settings(State(state): State<AppState>) -> Json<Value> {
    Json((*state.settings).clone())
}

/// Adds [`API_HEADER`] to every response under `/v1`.
pub async fn api_header(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(API_HEADER, HeaderValue::from(API_VERSION));
    response
}
