//! The HTTP API: JSON over `/v1/{namespace}/...`, with a bearer token: the
//! server's own (the owner) or an API key with grants per namespace (see
//! [`crate::auth`]).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use grafeno::{
    Direction, EdgeFilter, EdgeId, Error, Grafeno, NodeId, Props, Role, Timestamp, WriteTx,
};
use grafeno_memory::{context_for, recall};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::api::{
    ContextInput, EdgeInput, ErrorKind, FilterInput, NodeInput, NodeOutput, NodeRef, RecallInput,
    TimeInput, error_kind, parse_id, resolve_ref,
};
use crate::auth::{self, Caller, Need};

const BODY_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Grafeno>,
    /// Commits concurrent writes in groups, or, on a cluster member,
    /// through the cluster's leader.
    pub writer: Arc<Writes>,
    pub token: Option<Arc<str>>,
    /// Which key each secret is, loaded from the database.
    pub keys: Arc<crate::auth::Credentials>,
    /// Failed sign-ins, for lockouts.
    pub throttle: Arc<crate::auth::Throttle>,
    /// Sessions' lives and lockouts.
    pub auth: crate::accounts::AuthConfig,
    /// Limits for each GQ query.
    pub limits: grafeno_query::Limits,
    /// Requests being served under `/v1`; beyond its permits the server
    /// answers 503 instead of piling up work.
    pub in_flight: Arc<tokio::sync::Semaphore>,
    /// Counters for `GET /v1/metrics`.
    pub metrics: Arc<crate::metrics::ServerMetrics>,
    /// Records per transaction of a bulk import.
    pub import_chunk: usize,
    /// Commit notifications, for change streams and background work.
    pub events: Arc<crate::events::Events>,
    /// Work that runs from the change log.
    pub background: crate::events::BackgroundConfig,
    pub runners: Arc<std::sync::Mutex<Vec<crate::events::RunnerStatus>>>,
    /// The cluster group this state serves (0 on a single server).
    pub group: u64,
    /// Every group of this member, once they are all up (13.5).
    pub groups: crate::groups::Slot,
    /// The settings the server runs with, without secrets
    /// (`GET /v1/settings`).
    pub settings: Arc<Value>,
    /// Explicit transactions open across requests (16.4).
    pub transactions: Arc<crate::transactions::Transactions>,
}

/// Where writes go: the group-commit writer of a single server, or the
/// cluster's leader through this member (writes are then serialized on the
/// leader, and a follower answers "busy" naming it).
pub struct Writes {
    local: Arc<grafeno::Writer>,
    cluster: Option<Arc<grafeno_cluster::Node>>,
    /// The members' API addresses, for forwarding to the leader.
    apis: std::collections::HashMap<u64, String>,
    /// Reads from this estimated cost split across members (13.4).
    split_cost: f64,
    /// The cluster secret, for handing audit events to the leader (14.5).
    secret: Option<Arc<str>>,
}

impl Writes {
    pub fn write<T: Send + 'static>(
        &self,
        f: impl Fn(&mut WriteTx) -> grafeno::Result<T> + Send + 'static,
    ) -> grafeno::Result<T> {
        match &self.cluster {
            Some(node) => node.write(f).map_err(crate::cluster::engine_error),
            None => self.local.write(f),
        }
    }

    pub fn write_bulk<T: Send + 'static>(
        &self,
        f: impl Fn(&mut WriteTx) -> grafeno::Result<T> + Send + 'static,
    ) -> grafeno::Result<T> {
        match &self.cluster {
            Some(node) => node.write(f).map_err(crate::cluster::engine_error),
            None => self.local.write_bulk(f),
        }
    }

    pub fn flush(&self) -> grafeno::Result<()> {
        self.local.flush()
    }

    /// Imports an export in chunks (single servers only for now: a cluster
    /// member refuses, until imports go through the leader's log).
    pub fn import(
        &self,
        input: impl std::io::BufRead,
        chunk: usize,
    ) -> grafeno::Result<grafeno::DumpStats> {
        if self.cluster.is_some() {
            return Err(grafeno::Error::Invalid(
                "imports into a cluster are not supported yet: import into a single server and start the cluster from it".into(),
            ));
        }
        self.local.import(input, chunk)
    }

    pub fn metrics(&self) -> grafeno::WriterSnapshot {
        self.local.metrics()
    }

    /// The cluster member, on a cluster.
    pub fn cluster(&self) -> Option<&Arc<grafeno_cluster::Node>> {
        self.cluster.as_ref()
    }

    /// The cluster secret, on a cluster.
    pub fn secret(&self) -> Option<&str> {
        self.secret.as_deref()
    }

    /// A member's API address, if configured.
    pub fn api_of(&self, id: u64) -> Option<String> {
        self.apis.get(&id).cloned()
    }

    /// Every member with an API address, by id: the parts of a split read.
    pub fn members(&self) -> Vec<(u64, String)> {
        let mut members: Vec<(u64, String)> = self
            .apis
            .iter()
            .map(|(id, api)| (*id, api.clone()))
            .collect();
        members.sort();
        members
    }

    /// The estimated cost from which reads split across members.
    pub fn split_cost(&self) -> f64 {
        self.split_cost
    }
}

/// Requests served at once under `/v1` by default.
pub const MAX_REQUESTS: usize = 512;

/// Records per transaction of a bulk import, by default. Small enough that
/// an agent's write waits for one chunk at most (tens of milliseconds); at
/// 20 000 it waited most of a second (`examples/saturation.rs`).
pub const IMPORT_CHUNK: usize = 2_000;

impl AppState {
    /// The state for `db`, with its group-commit writer, the standard query
    /// limits and room for [`MAX_REQUESTS`] requests at once.
    pub fn new(db: Arc<Grafeno>, token: Option<&str>) -> Self {
        Self::with_writer(db, token, grafeno::WriterOptions::default())
    }

    /// [`AppState::new`] with a writer of these options.
    pub fn with_writer(
        db: Arc<Grafeno>,
        token: Option<&str>,
        options: grafeno::WriterOptions,
    ) -> Self {
        Self {
            events: crate::events::Events::attach(&db),
            writer: Arc::new(Writes {
                local: Arc::new(grafeno::Writer::new(db.clone(), options)),
                cluster: None,
                apis: Default::default(),
                split_cost: crate::cluster::DEFAULT_SPLIT_COST,
                secret: None,
            }),
            keys: Arc::new(crate::auth::Credentials::load(&db).unwrap_or_default()),
            throttle: Arc::default(),
            auth: crate::accounts::AuthConfig::default(),
            db,
            token: token.map(Arc::from),
            limits: grafeno_query::Limits::standard(),
            in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_REQUESTS)),
            metrics: Arc::default(),
            import_chunk: IMPORT_CHUNK,
            background: crate::events::BackgroundConfig::default(),
            runners: Arc::default(),
            group: 0,
            groups: Arc::default(),
            settings: Arc::new(json!({})),
            transactions: Arc::new(crate::transactions::Transactions::new(Default::default())),
        }
    }

    /// The state of another cluster group of this member: its database
    /// and member, the rest shared (keys, limits, metrics, the registry).
    pub fn for_group(
        &self,
        group: u64,
        db: Arc<Grafeno>,
        node: Arc<grafeno_cluster::Node>,
        apis: std::collections::HashMap<u64, String>,
    ) -> Self {
        let mut state = self.clone();
        state.group = group;
        state.events = crate::events::Events::attach(&db);
        state.writer = Arc::new(Writes {
            local: Arc::new(grafeno::Writer::new(
                db.clone(),
                grafeno::WriterOptions::default(),
            )),
            cluster: Some(node),
            apis,
            split_cost: self.writer.split_cost,
            secret: self.writer.secret.clone(),
        });
        state.runners = Arc::default();
        state.db = db;
        state
    }

    /// Sends every write through a cluster member (phase 13.2).
    pub fn with_cluster(
        mut self,
        node: Arc<grafeno_cluster::Node>,
        apis: std::collections::HashMap<u64, String>,
        split_cost: f64,
        secret: Option<&str>,
    ) -> Self {
        self.writer = Arc::new(Writes {
            local: self.writer.local.clone(),
            cluster: Some(node),
            apis,
            split_cost,
            secret: secret.map(Arc::from),
        });
        self
    }

    /// Whether work that must run once (webhooks, background tasks) runs
    /// here: on a single server, or on a cluster's leader.
    pub fn leading(&self) -> bool {
        self.writer
            .cluster()
            .is_none_or(|node| node.status().role == "leader")
    }

    /// How long explicit transactions may live and wait (16.4).
    pub fn with_transactions(mut self, settings: crate::transactions::TransactionSettings) -> Self {
        self.transactions = Arc::new(crate::transactions::Transactions::new(settings));
        self
    }

    pub fn with_background(mut self, background: crate::events::BackgroundConfig) -> Self {
        self.background = background;
        self
    }

    pub fn with_import_chunk(mut self, chunk: usize) -> Self {
        self.import_chunk = chunk.max(1);
        self
    }

    pub fn with_limits(mut self, limits: grafeno_query::Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_max_requests(mut self, max: usize) -> Self {
        self.in_flight = Arc::new(tokio::sync::Semaphore::new(max.max(1)));
        self
    }

    pub fn with_auth(mut self, auth: crate::accounts::AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    /// What `GET /v1/settings` reports.
    pub fn with_settings(mut self, settings: Value) -> Self {
        self.settings = Arc::new(settings);
        self
    }
}

/// What the HTTP server offers besides the API.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Origins allowed to call the API from a browser (CORS), such as
    /// Grafeno Studio served from a web server; `*` allows any origin.
    pub cors_origins: Vec<String>,
}

/// Serves until Ctrl+C (or SIGTERM on Unix, as sent by systemd).
pub async fn serve(state: AppState, addr: SocketAddr, options: Options) -> std::io::Result<()> {
    serve_until(state, addr, options, shutdown_signal()).await
}

/// Serves until `shutdown` completes, then finishes the requests in flight.
pub async fn serve_until(
    state: AppState,
    addr: SocketAddr,
    options: Options,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    eprintln!("grafeno listening on http://{local}");
    crate::events::start(&state);
    // Explicit transactions past their time give the writer back.
    {
        let transactions = state.transactions.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                every.tick().await;
                let transactions = transactions.clone();
                let _ = tokio::task::spawn_blocking(move || transactions.reap()).await;
            }
        });
    }
    // Vector indexes created to be built in the background (16.7): a chunk
    // at a time, on the leader of a cluster (each chunk replicates).
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                every.tick().await;
                if !state.leading() {
                    continue;
                }
                let state = state.clone();
                let _ = tokio::task::spawn_blocking(move || build_indexes(&state)).await;
            }
        });
    }
    axum::serve(
        listener,
        router_with(state, &options).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// One round of background index builds: a chunk of each index pending,
/// each in a write of its own, so other writes go between them.
fn build_indexes(state: &AppState) -> grafeno::Result<()> {
    let read = state.db.begin_read()?;
    let mut pending = Vec::new();
    for ns in read.namespaces()? {
        for name in read.pending_indexes(&ns.name)? {
            pending.push((ns.name.clone(), name));
        }
    }
    drop(read);
    for (ns, name) in pending {
        state
            .writer
            .write(move |tx| tx.build_index_step(&ns, &name, 2_000))?;
    }
    Ok(())
}

/// Ctrl+C, or SIGTERM on Unix (what systemd sends to stop a service).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub fn router(state: AppState) -> Router {
    router_with(state, &Options::default())
}

pub fn router_with(state: AppState, options: &Options) -> Router {
    let v1 = Router::new()
        .route("/namespaces", get(namespaces).post(create_namespace))
        .route(
            "/namespaces/{ns}",
            get(get_namespace)
                .patch(describe_namespace)
                .delete(drop_namespace),
        )
        .route("/export", get(export))
        .route("/backup", get(backup))
        .route("/import", post(import))
        .route("/{ns}/stats", get(stats))
        .route("/{ns}/read-many", post(read_many))
        .route("/{ns}/check", get(check))
        .route("/{ns}/rebuild", post(rebuild))
        .route("/{ns}/compact", post(compact))
        .route("/{ns}/vectors/recall", get(vector_recall))
        .route("/{ns}/vectors/health", get(vector_health))
        .route("/{ns}/nodes", get(list_nodes).post(create_node))
        .route(
            "/{ns}/nodes/{id}",
            get(get_node)
                .put(put_node)
                .patch(patch_node)
                .delete(delete_node),
        )
        .route("/{ns}/nodes/{id}/neighbors", get(neighbors))
        .route("/{ns}/edges", get(list_edges).post(create_edge))
        .route("/{ns}/edges/{id}", get(get_edge).delete(delete_edge))
        .route("/{ns}/edges/{id}/invalidate", post(invalidate_edge))
        .route("/{ns}/edges/{id}/retract", post(retract_edge))
        .route("/{ns}/k-hop", post(k_hop))
        .route("/{ns}/path", post(path))
        .route("/{ns}/subgraph", post(subgraph))
        .route("/{ns}/neighborhoods", post(neighborhoods))
        .route("/{ns}/search", post(search_handler))
        .route("/{ns}/search/text", post(search_text))
        .route("/{ns}/similar", post(similar_handler))
        .route("/{ns}/search/vector", post(search_vector))
        .route("/{ns}/recall", post(recall_handler))
        .route("/{ns}/context", post(context_handler))
        .route("/{ns}/batch", post(batch))
        .route("/{ns}/query", post(query))
        .route("/{ns}/query/part", post(query_part))
        .route("/algorithms", get(algorithms))
        .route("/whoami", get(crate::accounts::whoami))
        .route("/privileges", get(crate::roles::catalog))
        .route("/audit", get(crate::audit::read))
        .route("/cluster/audit", post(crate::audit::receive))
        // 14.3: the same accounts and roles under /admin, and grants.
        .route(
            "/admin/users",
            get(crate::accounts::list_accounts).post(crate::accounts::create_account),
        )
        .route(
            "/admin/users/{name}",
            get(crate::accounts::get_account)
                .patch(crate::accounts::change_account)
                .delete(crate::accounts::delete_account),
        )
        .route("/admin/roles", get(crate::roles::list_roles))
        .route(
            "/admin/roles/{name}",
            get(crate::roles::get_role)
                .put(crate::roles::put_role)
                .delete(crate::roles::delete_role),
        )
        .route(
            "/admin/grants",
            get(crate::admin::privileges_of).post(crate::admin::grant),
        )
        .route("/admin/grants/revoke", post(crate::admin::revoke))
        .route("/admin/grants/roles", post(crate::admin::grant_roles))
        .route(
            "/admin/grants/roles/revoke",
            post(crate::admin::revoke_roles),
        )
        .route("/roles", get(crate::roles::list_roles))
        .route(
            "/roles/{name}",
            get(crate::roles::get_role)
                .put(crate::roles::put_role)
                .delete(crate::roles::delete_role),
        )
        .route(
            "/keys",
            get(crate::accounts::list_keys).post(crate::accounts::create_key),
        )
        .route(
            "/keys/{name}",
            axum::routing::delete(crate::accounts::revoke_key),
        )
        .route("/keys/{name}/rotate", post(crate::accounts::rotate_key))
        // What this engine is and runs with (15.3).
        .route("/about", get(crate::about::about))
        .route("/settings", get(crate::about::settings))
        // Accounts and sign-in (14.1).
        .route("/auth/status", get(crate::accounts::status))
        .route("/auth/setup", post(crate::accounts::setup))
        .route("/auth/signin", post(crate::accounts::signin))
        .route("/auth/refresh", post(crate::accounts::refresh))
        .route("/auth/signout", post(crate::accounts::signout))
        .route("/auth/password", post(crate::accounts::change_password))
        .route("/auth/sessions", get(crate::accounts::sessions))
        .route(
            "/auth/sessions/{id}",
            axum::routing::delete(crate::accounts::end_session),
        )
        .route(
            "/accounts",
            get(crate::accounts::list_accounts).post(crate::accounts::create_account),
        )
        .route(
            "/accounts/{name}",
            get(crate::accounts::get_account)
                .patch(crate::accounts::change_account)
                .delete(crate::accounts::delete_account),
        )
        .route("/{ns}/algo/{name}", post(algorithm))
        .route("/{ns}/resolve", get(resolve))
        .route("/{ns}/duplicates", get(duplicates))
        .route("/{ns}/fuzzy", get(fuzzy))
        .route("/{ns}/match", post(find_matches))
        .route("/{ns}/matches", get(match_pairs))
        .route("/{ns}/constraints", get(constraints))
        .route("/{ns}/contradictions", get(contradictions))
        .route("/{ns}/nodes/{id}/history/{key}", get(property_history))
        .route("/{ns}/export.graphml", get(export_graphml))
        .route("/{ns}/export/nodes.csv", get(export_nodes_csv))
        .route("/{ns}/export/edges.csv", get(export_edges_csv))
        .route("/{ns}/import/graphml", post(import_graphml))
        .route("/{ns}/import/nodes.csv", post(import_nodes_csv))
        .route("/{ns}/import/edges.csv", post(import_edges_csv))
        .route("/{ns}/transactions", post(begin_transaction))
        .route("/{ns}/transactions/{id}", get(get_transaction))
        .route("/{ns}/transactions/{id}/query", post(transaction_query))
        .route("/{ns}/transactions/{id}/commit", post(commit_transaction))
        .route(
            "/{ns}/transactions/{id}/rollback",
            post(rollback_transaction),
        )
        .route("/{ns}/changes", get(changes))
        .route("/{ns}/events/{name}", get(subscription_events))
        .route("/{ns}/ingest", post(ingest))
        .route("/{ns}/webhooks", get(list_webhooks).post(create_webhook))
        .route(
            "/{ns}/webhooks/{id}",
            get(get_webhook).delete(delete_webhook),
        )
        .route("/background", get(background))
        .route("/{ns}/communities", get(communities))
        .route("/{ns}/communities/refresh", post(refresh_communities))
        .route("/{ns}/communities/{id}", get(community))
        .route("/{ns}/communities/{id}/text", put(community_text))
        .route("/{ns}/extract", post(extract))
        .route("/{ns}/review", get(review_queue))
        .route("/{ns}/review/{key}", post(resolve_review))
        .route("/{ns}/nodes/{id}/mentions", post(add_mentions))
        .route("/{ns}/match-rules", get(match_rules))
        .route(
            "/{ns}/match-rules/{label}",
            axum::routing::put(set_match_rule).delete(delete_match_rule),
        )
        .route("/{ns}/merge", post(merge))
        .route("/{ns}/merges", get(merges))
        .route("/{ns}/merges/{id}/undo", post(undo_merge))
        .route("/{ns}/touch", post(touch))
        .route("/{ns}/weak", get(weak))
        .route("/{ns}/archive", post(archive_nodes))
        .route("/{ns}/consolidation", get(consolidation))
        .route("/{ns}/summaries", post(summaries))
        .route("/metrics", get(metrics))
        .route("/cluster", get(cluster_status))
        .route("/cluster/voters", post(cluster_voters))
        .route("/cluster/compact", post(cluster_compact))
        .route("/cluster/groups", get(crate::groups::groups))
        .route(
            "/cluster/placement",
            get(crate::groups::placement).post(crate::groups::place),
        )
        .route(
            "/cluster/groups/{group}/import",
            post(crate::groups::import),
        )
        .route("/cluster/groups/{group}/fence", post(crate::groups::fence))
        // Each group's own status, voters and compaction (13.6).
        .route("/cluster/groups/{group}/status", get(cluster_status))
        .route("/cluster/groups/{group}/voters", post(cluster_voters))
        .route("/cluster/groups/{group}/compact", post(cluster_compact))
        .route(
            "/cluster/groups/{group}/directory",
            post(crate::groups::set_directory),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::cluster::route,
        ))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth))
        .route_layer(middleware::from_fn_with_state(state.clone(), admit))
        .route_layer(middleware::from_fn_with_state(state.clone(), observe))
        .layer(middleware::from_fn(crate::about::api_header));
    // No interface here (15.1): `/` and the Studio's old place say where
    // it went.
    let app = Router::new()
        .route("/health", get(health))
        .nest("/v1", v1)
        .route("/", get(crate::studio::note))
        .route("/studio", get(crate::studio::note))
        .route("/studio/", get(crate::studio::note))
        .route("/studio/{*path}", get(crate::studio::note));
    // A panic while serving a request (a damaged page, a bug) answers 500
    // instead of dropping the connection.
    let mut app = app
        // On a cluster of several groups, to the group a request is for.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::groups::dispatch,
        ))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ));
    if !options.cors_origins.is_empty() {
        app = app.layer(cors(&options.cors_origins));
    }
    app.with_state(state)
}

fn panic_response(panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let detail = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    ApiError::internal(format!(
        "the server failed while serving this request ({detail}); if the database file may be damaged, stop the server and run `grafeno check`"
    ))
    .into_response()
}

fn cors(origins: &[String]) -> tower_http::cors::CorsLayer {
    use axum::http::{HeaderValue, Method};
    use tower_http::cors::{AllowOrigin, CorsLayer};

    let origin = if origins.iter().any(|origin| origin == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(
            origins
                .iter()
                .filter_map(|origin| HeaderValue::from_str(origin.trim_end_matches('/')).ok()),
        )
    };
    CorsLayer::new()
        .allow_origin(origin)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
}

// ---------------------------------------------------------------- plumbing

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    /// More fields of the error object (the conflicts of a refused merge).
    details: Option<serde_json::Map<String, Value>>,
}

impl ApiError {
    pub(crate) fn with_status(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    pub(crate) fn not_found_message(message: impl Into<String>) -> Self {
        Self::not_found(message)
    }

    pub(crate) fn forbidden_message(message: impl Into<String>) -> Self {
        Self::forbidden(message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
            details: None,
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
            message: message.into(),
            details: None,
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: message.into(),
            details: None,
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: message.into(),
            details: None,
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let (status, code) = match error_kind(&error) {
            ErrorKind::BadRequest => (StatusCode::BAD_REQUEST, "invalid"),
            ErrorKind::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ErrorKind::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
            ErrorKind::Busy
                if error
                    .to_string()
                    .contains(crate::cluster::NOT_LEADER_MESSAGE) =>
            {
                (StatusCode::MISDIRECTED_REQUEST, "not_leader")
            }
            ErrorKind::Busy => (StatusCode::SERVICE_UNAVAILABLE, "busy"),
            ErrorKind::Conflict => (StatusCode::CONFLICT, "conflict"),
            ErrorKind::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
        };
        Self {
            status,
            code,
            message: error.to_string(),
            details: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = serde_json::Map::new();
        error.insert("code".into(), Value::from(self.code));
        error.insert("message".into(), Value::from(self.message));
        error.extend(self.details.unwrap_or_default());
        let body = json!({ "error": error });
        let mut response = (self.status, Json(body)).into_response();
        if self.code == "not_leader" {
            response.headers_mut().insert(
                crate::cluster::NOT_LEADER,
                header::HeaderValue::from_static("1"),
            );
        }
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
        }
        response
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

/// Runs a write through the group-commit writer. `work` may run more than
/// once (when another write of its group fails), so it takes its inputs by
/// reference or clones them.
pub(crate) async fn write<T: Send + 'static>(
    state: &AppState,
    work: impl Fn(&mut WriteTx) -> grafeno::Result<T> + Send + 'static,
) -> ApiResult<T> {
    let writer = state.writer.clone();
    // Writes of a group share a transaction: each one acts as its caller,
    // from where it came (14.5).
    let actor = auth::current().actor();
    let origin = crate::audit::origin();
    let work = move |tx: &mut WriteTx| {
        tx.act_as(actor.clone());
        tx.set_origin(origin.clone());
        let result = work(tx);
        tx.act_as(None);
        tx.set_origin(None);
        result
    };
    tokio::task::spawn_blocking(move || writer.write(work))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(ApiError::from)
}

/// Runs a long write (a rebuild, a compaction) in the writer's bulk lane:
/// in a transaction of its own, after the interactive writes waiting.
async fn write_bulk<T: Send + 'static>(
    state: &AppState,
    work: impl Fn(&mut WriteTx) -> grafeno::Result<T> + Send + 'static,
) -> ApiResult<T> {
    let writer = state.writer.clone();
    let actor = auth::current().actor();
    let origin = crate::audit::origin();
    let work = move |tx: &mut WriteTx| {
        tx.act_as(actor.clone());
        tx.set_origin(origin.clone());
        let result = work(tx);
        tx.act_as(None);
        tx.set_origin(None);
        result
    };
    tokio::task::spawn_blocking(move || {
        let value = writer.write_bulk(work)?;
        writer.flush()?;
        Ok::<T, grafeno::Error>(value)
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(ApiError::from)
}

/// The database as the caller of a request sees it (14.4): its snapshots
/// leave out what the caller may not read. Everything else is the
/// database's.
pub(crate) struct Db<'a> {
    db: &'a Grafeno,
    view: Option<Arc<grafeno::View>>,
}

impl std::ops::Deref for Db<'_> {
    type Target = Grafeno;

    fn deref(&self) -> &Grafeno {
        self.db
    }
}

impl Db<'_> {
    /// A snapshot that reads as the caller may.
    pub(crate) fn begin_read(&self) -> grafeno::Result<grafeno::ReadTx> {
        Ok(self.db.begin_read()?.with_view(self.view.clone()))
    }
}

/// Runs engine work on the blocking pool: redb calls are synchronous. Its
/// snapshots read as the request's caller may (taken here, in the request).
pub(crate) async fn blocking<T: Send + 'static>(
    state: &AppState,
    work: impl FnOnce(&Db) -> grafeno::Result<T> + Send + 'static,
) -> ApiResult<T> {
    let db = state.db.clone();
    let view = auth::current().view();
    tokio::task::spawn_blocking(move || work(&Db { db: &db, view }))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(ApiError::from)
}

/// A JSON body whose errors are reported as JSON.
pub struct Body<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Body(value))
            .map_err(|rejection| ApiError::bad_request(rejection.body_text()))
    }
}

/// Query parameters whose errors are reported as JSON.
pub struct Params<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Params(value))
            .map_err(|rejection| ApiError::bad_request(rejection.body_text()))
    }
}

/// Finds the caller (the owner token or an API key), checks what the route
/// needs, and serves the request as that caller.
async fn auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    let need = auth::need(request.method().as_str(), request.uri().path());
    let public = matches!(need, Need::Public);
    let address = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip().to_string());
    // Inside the nested router the path lost `/v1`: the audit log keeps
    // the one the client sent.
    let (method, path) = (
        request.method().to_string(),
        request
            .extensions()
            .get::<axum::extract::OriginalUri>()
            .map_or_else(
                || request.uri().path().to_owned(),
                |o| o.0.path().to_owned(),
            ),
    );
    let open = state.token.is_none() && !state.keys.has_accounts();
    let caller = match given {
        _ if open => Ok(Caller::Owner),
        Some(given)
            if state
                .token
                .as_ref()
                .is_some_and(|token| constant_time_eq(given.as_bytes(), token.as_bytes())) =>
        {
            Ok(Caller::Owner)
        }
        Some(given) => {
            let state = state.clone();
            tokio::task::spawn_blocking(move || crate::accounts::authenticate(&state, &given))
                .await
                .unwrap_or_else(|e| Err(ApiError::internal(e.to_string())))
        }
        None => Err(ApiError::with_status(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or wrong bearer token or API key",
        )),
    };
    // Signing in needs nobody: a bad or missing credential is no reason to
    // refuse it.
    let caller = match (caller, &need) {
        (Ok(caller), _) => caller,
        (Err(_), Need::Public) => Caller::Anonymous,
        (Err(error), _) => {
            let _ = axum::body::to_bytes(request.into_body(), 1024 * 1024).await;
            // Refused for its credential: audited (14.5).
            let mut event = crate::audit::refusal(&method, &path, error.code, &error.message);
            event.address = address;
            crate::audit::record(&state, event).await;
            return error.into_response();
        }
    };
    let allowed = match need {
        Need::Public | Need::Caller => Ok(()),
        Need::Owner if caller.is_owner() => Ok(()),
        Need::Owner => Err(format!(
            "{:?} is not a superuser (or its key is scoped)",
            caller.name()
        )),
        Need::Server(action) if caller.can_server(action) => Ok(()),
        Need::Server(action) => Err(caller.server_refusal(action)),
        Need::Namespace(ns, actions) if caller.can_all(actions, &ns) => Ok(()),
        Need::Namespace(ns, actions) => Err(caller.refusal(&ns, actions)),
    };
    if let Err(message) = allowed {
        // Refused for a privilege: audited, as the caller (14.5).
        let event = crate::audit::refusal(&method, &path, "forbidden", &message);
        let refused = refuse(request, StatusCode::FORBIDDEN, "forbidden", message);
        auth::ADDRESS
            .scope(
                address.clone(),
                auth::CALLER.scope(caller.clone(), async {
                    let mut event = event;
                    if let Some(origin) = crate::audit::origin() {
                        event.account = origin.account;
                        event.via = origin.via;
                        event.address = origin.address;
                    }
                    crate::audit::record(&state, event).await;
                }),
            )
            .await;
        return refused.await;
    }
    let response = auth::ADDRESS
        .scope(
            address.clone(),
            auth::CALLER.scope(caller.clone(), next.run(request)),
        )
        .await;
    // Refused by the handler itself (a statement, a record): audited too.
    // Signing in audits itself.
    let status = response.status();
    if !public && (status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED) {
        auth::ADDRESS
            .scope(
                address,
                auth::CALLER.scope(caller, async {
                    let mut event = crate::audit::refusal(
                        &method,
                        &path,
                        if status == StatusCode::FORBIDDEN {
                            "forbidden"
                        } else {
                            "unauthorized"
                        },
                        "",
                    );
                    if let Some(origin) = crate::audit::origin() {
                        event.account = origin.account;
                        event.via = origin.via;
                        event.address = origin.address;
                    }
                    crate::audit::record(&state, event).await;
                }),
            )
            .await;
    }
    response
}

async fn refuse(
    request: Request,
    status: StatusCode,
    code: &'static str,
    message: String,
) -> Response {
    // Read (a bounded part of) the body first: closing the connection while
    // the client still sends it makes some clients see a reset instead of
    // this response.
    let _ = axum::body::to_bytes(request.into_body(), 1024 * 1024).await;
    ApiError {
        status,
        code,
        message,
        details: None,
    }
    .into_response()
}

/// 403 when the caller sees `ns` record by record (14.4): `what` reads every
/// record of the namespace, and its result would tell what the caller may
/// not read (entity resolution, community summaries, maintenance).
fn whole_namespace(ns: &str, what: &str) -> ApiResult<()> {
    match auth::current().view() {
        Some(view) if view.per_record(ns) => Err(ApiError::forbidden(format!(
            "{what} reads every record of {ns:?}, and some are hidden from {:?}: it needs read on all of them",
            auth::current().name()
        ))),
        _ => Ok(()),
    }
}

/// Why the caller may not do what `role` stands for in `ns`, if it may not.
fn missing(caller: &Caller, ns: &str, role: Role) -> Option<String> {
    let actions = auth::role_actions(role);
    (!caller.can_all(actions, ns)).then(|| caller.refusal(ns, actions))
}

/// 403 unless the caller of this request has `role` on `ns`.
fn require(ns: &str, role: Role) -> ApiResult<()> {
    require_all(ns, auth::role_actions(role))
}

/// 403 unless the caller of this request may do every one of `actions` in
/// `ns`.
fn require_all(ns: &str, actions: &[grafeno::Act]) -> ApiResult<()> {
    let caller = auth::current();
    if caller.can_all(actions, ns) {
        Ok(())
    } else {
        Err(ApiError::forbidden(caller.refusal(ns, actions)))
    }
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Absent → `None`, `null` → `Some(None)`, a value → `Some(Some(value))`.
pub(crate) fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A time in a query string: microseconds or ISO 8601.
fn parse_time_param(text: &str) -> grafeno::Result<Timestamp> {
    match text.parse::<i64>() {
        Ok(micros) => Ok(Timestamp::from_micros(micros)),
        Err(_) => Timestamp::parse(text),
    }
}

fn parse_edge_id(text: &str) -> grafeno::Result<EdgeId> {
    parse_id(text).map(|id| EdgeId::from_u128(id.as_u128()))
}

// ---------------------------------------------------------------- handlers

/// Turns requests away with 503 (and `Retry-After`) when too many are in
/// flight, instead of queueing without bound.
/// Counts every request under `/v1` and how long it took.
async fn observe(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    state
        .metrics
        .request(response.status().as_u16(), started.elapsed().as_secs_f64());
    response
}

/// The server's and the writer's counters, in the Prometheus text format.
async fn metrics(State(state): State<AppState>) -> Response {
    let mut text = state
        .metrics
        .render(&state.writer.metrics(), state.db.projection_cache_bytes());
    // Each Raft group this member holds (13.6).
    let mut statuses: Vec<(u64, grafeno_cluster::Status)> = Vec::new();
    match state.groups.get() {
        Some(registry) => {
            for (group, group_state) in &registry.states {
                if let Some(node) = group_state.writer.cluster() {
                    statuses.push((*group, node.status()));
                }
            }
        }
        None => {
            if let Some(node) = state.writer.cluster() {
                statuses.push((0, node.status()));
            }
        }
    }
    text.push_str(&crate::metrics::render_cluster(&statuses));
    let (reading, writing) = state.transactions.open();
    text.push_str(&format!(
        "# HELP grafeno_transactions_open Explicit transactions open (16.4)\n\
         # TYPE grafeno_transactions_open gauge\n\
         grafeno_transactions_open{{kind=\"read\"}} {reading}\n\
         grafeno_transactions_open{{kind=\"write\"}} {writing}\n"
    ));
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
        .into_response()
}

async fn admit(State(state): State<AppState>, request: Request, next: Next) -> Response {
    match state.in_flight.clone().try_acquire_owned() {
        Ok(_permit) => next.run(request).await,
        Err(_) => {
            state.metrics.busy();
            busy_response()
        }
    }
}

fn busy_response() -> Response {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "busy",
        message: "the server is busy: retry in a moment".into(),
        details: None,
    }
    .into_response()
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "layout": grafeno::SCHEMA_VERSION,
    }))
}

#[derive(Serialize)]
struct Upserted {
    id: NodeId,
    created: bool,
}

async fn create_node(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<NodeInput>,
) -> ApiResult<(StatusCode, Json<Upserted>)> {
    let result = write(&state, move |tx| match input.id {
        Some(id) => {
            let created = tx.upsert_node(&ns, id, input.clone().into_new())?;
            Ok(Upserted { id, created })
        }
        None => Ok(Upserted {
            id: tx.create_node(&ns, input.clone().into_new())?,
            created: true,
        }),
    })
    .await?;
    let status = if result.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(result)))
}

async fn put_node(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
    Body(input): Body<NodeInput>,
) -> ApiResult<(StatusCode, Json<Upserted>)> {
    let id = parse_id(&id)?;
    if input.id.is_some_and(|body_id| body_id != id) {
        return Err(ApiError::bad_request(
            "the id in the body does not match the path",
        ));
    }
    let created = write(&state, move |tx| {
        tx.upsert_node(&ns, id, input.clone().into_new())
    })
    .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(Upserted { id, created })))
}

const DEFAULT_PAGE: usize = 100;
const MAX_PAGE: usize = 1_000;

#[derive(Deserialize)]
struct ListParams {
    label: Option<String>,
    /// The `next` value of the previous page.
    after: Option<String>,
    limit: Option<usize>,
}

impl ListParams {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
    }
}

/// A page of a listing. `next` is set when there may be more: pass it as
/// `after` to get them.
#[derive(Serialize)]
struct Page<T> {
    items: Vec<T>,
    next: Option<String>,
}

impl<T> Page<T> {
    fn new(items: Vec<T>, limit: usize, id: impl Fn(&T) -> String) -> Self {
        let next = (items.len() == limit)
            .then(|| items.last().map(id))
            .flatten();
        Self { items, next }
    }
}

async fn list_nodes(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<ListParams>,
) -> ApiResult<Json<Page<grafeno::Node>>> {
    let limit = params.limit();
    let after = params.after.as_deref().map(parse_id).transpose()?;
    let nodes = blocking(&state, move |db| {
        db.begin_read()?
            .nodes_page(&ns, params.label.as_deref(), after, limit)
    })
    .await?;
    Ok(Json(Page::new(nodes, limit, |node| node.id.to_string())))
}

async fn list_edges(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<ListParams>,
) -> ApiResult<Json<Page<grafeno::Edge>>> {
    let limit = params.limit();
    let after = params.after.as_deref().map(parse_edge_id).transpose()?;
    let edges = blocking(&state, move |db| {
        db.begin_read()?
            .edges_page(&ns, params.label.as_deref(), after, limit)
    })
    .await?;
    Ok(Json(Page::new(edges, limit, |edge| edge.id.to_string())))
}

/// The limit counts positions, repeated IDs included, across both lists.
const MAX_READ_MANY: usize = 512;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadManyInput {
    #[serde(default)]
    nodes: Vec<NodeId>,
    #[serde(default)]
    edges: Vec<EdgeId>,
}

#[derive(Serialize)]
struct ReadManyOutput {
    nodes: Vec<Option<NodeOutput>>,
    edges: Vec<Option<grafeno::Edge>>,
}

async fn read_many(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<ReadManyInput>,
) -> ApiResult<Json<ReadManyOutput>> {
    if input.nodes.len().saturating_add(input.edges.len()) > MAX_READ_MANY {
        return Err(ApiError::bad_request(
            "read-many accepts at most 512 node and edge IDs combined",
        ));
    }
    let result = blocking(&state, move |db| {
        let read = db.begin_read()?;
        let nodes = read
            .nodes_by_id(&ns, &input.nodes)?
            .into_iter()
            .map(|node| {
                node.map(|node| NodeOutput::load(&read, &ns, node))
                    .transpose()
            })
            .collect::<grafeno::Result<Vec<_>>>()?;
        let edges = read.edges_by_id(&ns, &input.edges)?;
        Ok(ReadManyOutput { nodes, edges })
    })
    .await?;
    Ok(Json(result))
}

async fn get_node(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
) -> ApiResult<Json<NodeOutput>> {
    let id = parse_id(&id)?;
    let node = blocking(&state, move |db| {
        let read = db.begin_read()?;
        let node = read.node(&ns, id)?.ok_or(Error::NodeNotFound(id))?;
        NodeOutput::load(&read, &ns, node)
    })
    .await?;
    Ok(Json(node))
}

/// A partial update. `text` and `embedding` set to `null` remove them.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchNode {
    /// Merged into the current properties; a `null` is stored as a value.
    #[serde(default)]
    props: Option<Props>,
    /// Properties to remove (after `props` is merged).
    #[serde(default)]
    remove_props: Vec<String>,
    #[serde(default)]
    add_labels: Vec<String>,
    #[serde(default)]
    remove_labels: Vec<String>,
    #[serde(default, deserialize_with = "double_option")]
    text: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    embedding: Option<Option<Vec<f32>>>,
    /// From 0 to 1.
    #[serde(default)]
    importance: Option<f32>,
}

fn apply_patch(tx: &mut WriteTx, ns: &str, id: NodeId, patch: PatchNode) -> grafeno::Result<()> {
    tx.node(ns, id)?.ok_or(Error::NodeNotFound(id))?;
    if let Some(importance) = patch.importance {
        tx.set_importance(ns, id, importance)?;
    }
    if let Some(props) = patch.props {
        tx.set_node_props(ns, id, props)?;
    }
    if !patch.remove_props.is_empty() {
        let keys: Vec<&str> = patch.remove_props.iter().map(String::as_str).collect();
        tx.remove_node_props(ns, id, &keys)?;
    }
    for label in &patch.add_labels {
        tx.add_label(ns, id, label)?;
    }
    for label in &patch.remove_labels {
        tx.remove_label(ns, id, label)?;
    }
    match patch.text {
        Some(Some(text)) => tx.set_text(ns, id, &text)?,
        Some(None) => {
            tx.remove_text(ns, id)?;
        }
        None => {}
    }
    match patch.embedding {
        Some(Some(embedding)) => tx.set_embedding(ns, id, &embedding)?,
        Some(None) => {
            tx.remove_embedding(ns, id)?;
        }
        None => {}
    }
    Ok(())
}

async fn patch_node(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
    Body(patch): Body<PatchNode>,
) -> ApiResult<Json<NodeOutput>> {
    let id = parse_id(&id)?;
    let patched = ns.clone();
    write(&state, move |tx| {
        apply_patch(tx, &patched, id, patch.clone())
    })
    .await?;
    let node = blocking(&state, move |db| {
        let read = db.begin_read()?;
        let node = read.node(&ns, id)?.ok_or(Error::NodeNotFound(id))?;
        NodeOutput::load(&read, &ns, node)
    })
    .await?;
    Ok(Json(node))
}

async fn delete_node(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let id = parse_id(&id)?;
    write(&state, move |tx| tx.delete_node(&ns, id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NeighborParams {
    direction: Option<Direction>,
    /// Comma-separated labels.
    label: Option<String>,
    valid_at: Option<String>,
    known_at: Option<String>,
}

async fn neighbors(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
    Params(params): Params<NeighborParams>,
) -> ApiResult<Json<Vec<grafeno::Neighbor>>> {
    let id = parse_id(&id)?;
    let mut filter = EdgeFilter::any();
    for label in params.label.iter().flat_map(|labels| labels.split(',')) {
        if !label.is_empty() {
            filter = filter.label(label);
        }
    }
    if let Some(at) = &params.valid_at {
        filter = filter.valid_at(parse_time_param(at)?);
    }
    if let Some(at) = &params.known_at {
        filter = filter.known_at(parse_time_param(at)?);
    }
    let direction = params.direction.unwrap_or_default();
    let neighbors = blocking(&state, move |db| {
        db.begin_read()?.neighbors(&ns, id, direction, &filter)
    })
    .await?;
    Ok(Json(neighbors))
}

async fn create_edge(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<EdgeInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let new = input.into_new(&HashMap::new())?;
    let id = write(&state, move |tx| tx.create_edge(&ns, new.clone())).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

async fn get_edge(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
) -> ApiResult<Json<grafeno::Edge>> {
    let id = parse_edge_id(&id)?;
    let edge = blocking(&state, move |db| {
        db.begin_read()?
            .edge(&ns, id)?
            .ok_or(Error::EdgeNotFound(id))
    })
    .await?;
    Ok(Json(edge))
}

async fn delete_edge(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let id = parse_edge_id(&id)?;
    write(&state, move |tx| tx.delete_edge(&ns, id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvalidateInput {
    at: TimeInput,
}

async fn invalidate_edge(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
    Body(input): Body<InvalidateInput>,
) -> ApiResult<Json<grafeno::Edge>> {
    let id = parse_edge_id(&id)?;
    let at = input.at.resolve()?;
    let edge = write(&state, move |tx| {
        tx.invalidate_edge(&ns, id, at)?;
        tx.edge(&ns, id)?.ok_or(Error::EdgeNotFound(id))
    })
    .await?;
    Ok(Json(edge))
}

async fn retract_edge(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
) -> ApiResult<Json<grafeno::Edge>> {
    let id = parse_edge_id(&id)?;
    let edge = write(&state, move |tx| {
        tx.retract_edge(&ns, id)?;
        tx.edge(&ns, id)?.ok_or(Error::EdgeNotFound(id))
    })
    .await?;
    Ok(Json(edge))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KHopInput {
    start: NodeId,
    #[serde(default = "one")]
    depth: usize,
    #[serde(default)]
    direction: Direction,
    #[serde(default)]
    filter: FilterInput,
}

fn one() -> usize {
    1
}

async fn k_hop(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<KHopInput>,
) -> ApiResult<Json<Vec<Value>>> {
    let filter = input.filter.resolve()?;
    let found = blocking(&state, move |db| {
        db.begin_read()?
            .k_hop(&ns, input.start, input.depth, input.direction, &filter)
    })
    .await?;
    Ok(Json(
        found
            .into_iter()
            .map(|(node, hops)| json!({ "node": node, "hops": hops }))
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathInput {
    from: NodeId,
    to: NodeId,
    #[serde(default = "default_max_depth")]
    max_depth: usize,
    #[serde(default)]
    direction: Direction,
    #[serde(default)]
    filter: FilterInput,
}

fn default_max_depth() -> usize {
    6
}

async fn path(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<PathInput>,
) -> ApiResult<Json<Value>> {
    let filter = input.filter.resolve()?;
    let path = blocking(&state, move |db| {
        db.begin_read()?.shortest_path(
            &ns,
            input.from,
            input.to,
            input.max_depth,
            input.direction,
            &filter,
        )
    })
    .await?;
    Ok(Json(json!({ "path": path })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubgraphInput {
    seeds: Vec<NodeId>,
    #[serde(default = "one")]
    depth: usize,
    #[serde(default)]
    direction: Direction,
    #[serde(default)]
    filter: FilterInput,
}

async fn subgraph(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<SubgraphInput>,
) -> ApiResult<Json<grafeno::Subgraph>> {
    let filter = input.filter.resolve()?;
    let subgraph = blocking(&state, move |db| {
        db.begin_read()?
            .subgraph(&ns, &input.seeds, input.depth, input.direction, &filter)
    })
    .await?;
    Ok(Json(subgraph))
}

/// Separate depth-one neighborhoods, in the order of the roots.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NeighborhoodsInput {
    seeds: Vec<NodeId>,
    #[serde(default)]
    direction: Direction,
    #[serde(default)]
    filter: FilterInput,
}

async fn neighborhoods(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<NeighborhoodsInput>,
) -> ApiResult<Json<Vec<grafeno::Subgraph>>> {
    if input.seeds.len() > 8 {
        return Err(ApiError::bad_request(
            "neighborhoods accepts at most 8 roots",
        ));
    }
    let filter = input.filter.resolve()?;
    let result = blocking(&state, move |db| {
        let read = db.begin_read()?;
        input
            .seeds
            .iter()
            .map(|&seed| read.subgraph(&ns, &[seed], 1, input.direction, &filter))
            .collect::<grafeno::Result<Vec<_>>>()
    })
    .await?;
    Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextSearchInput {
    query: String,
    #[serde(default = "ten")]
    limit: usize,
    /// A full-text index (16.6): its query language, and fragments.
    #[serde(default)]
    index: Option<String>,
    #[serde(default)]
    fragments: bool,
}

fn ten() -> usize {
    10
}

async fn search_text(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<TextSearchInput>,
) -> ApiResult<Json<Vec<grafeno::FulltextHit>>> {
    let hits = blocking(&state, move |db| {
        let read = db.begin_read()?;
        match &input.index {
            Some(index) => {
                read.fulltext_search(&ns, index, &input.query, input.limit, input.fragments)
            }
            None => Ok(read
                .search_text(&ns, &input.query, input.limit)?
                .into_iter()
                .map(|hit| grafeno::FulltextHit {
                    node: hit.node,
                    score: hit.score,
                    fragments: Vec::new(),
                })
                .collect()),
        }
    })
    .await?;
    Ok(Json(hits))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    index: Option<String>,
    #[serde(default)]
    embedding: Option<Vec<f32>>,
    #[serde(default)]
    vector_index: Option<String>,
    #[serde(default)]
    mode: grafeno::SearchMode,
    #[serde(default)]
    fusion: Option<String>,
    #[serde(default)]
    k: Option<f32>,
    #[serde(default)]
    text_weight: Option<f32>,
    #[serde(default = "ten")]
    limit: usize,
    #[serde(default = "fifty")]
    candidates: usize,
    #[serde(default)]
    fragments: bool,
}

fn fifty() -> usize {
    50
}

/// Text, vector or hybrid search, as the request says (16.6).
async fn search_handler(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<SearchInput>,
) -> ApiResult<Json<Vec<grafeno::SearchHit>>> {
    let request = grafeno::SearchRequest {
        fusion: crate::api::fusion_of(input.fusion.as_deref(), input.k, input.text_weight)?,
        text: input.text,
        index: input.index,
        embedding: input.embedding,
        vector_index: input.vector_index,
        mode: input.mode,
        limit: input.limit,
        candidates: input.candidates,
        fragments: input.fragments,
    };
    let hits = blocking(&state, move |db| db.begin_read()?.search(&ns, &request)).await?;
    Ok(Json(hits))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VectorSearchInput {
    #[serde(default)]
    embedding: Option<Vec<f32>>,
    /// Several at once (16.7): the answer is a list per embedding.
    #[serde(default)]
    embeddings: Option<Vec<Vec<f32>>>,
    #[serde(default = "ten")]
    limit: usize,
    #[serde(default)]
    ef: Option<usize>,
    /// A vector index (16.7); without one, the node embeddings.
    #[serde(default)]
    index: Option<String>,
    /// Kept during the walk: `{labels, props}`.
    #[serde(default)]
    filter: Option<grafeno::VectorFilter>,
}

async fn search_vector(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<VectorSearchInput>,
) -> ApiResult<Json<Value>> {
    let found = blocking(&state, move |db| {
        let read = db.begin_read()?;
        let search = grafeno::VectorSearch {
            index: input.index.clone(),
            limit: input.limit,
            ef: input.ef,
            filter: input.filter.clone(),
            exclude: None,
        };
        let plain = input.index.is_none() && input.filter.is_none();
        let one = |embedding: &[f32]| {
            if plain {
                // As before 16.7: hits a view hides are replaced.
                match input.ef {
                    Some(ef) => read.search_vector_with(&ns, embedding, input.limit, ef),
                    None => read.search_vector(&ns, embedding, input.limit),
                }
            } else {
                read.vector_search(&ns, embedding, &search)
            }
        };
        match (&input.embedding, &input.embeddings) {
            (Some(embedding), None) => Ok(json!(one(embedding)?)),
            (None, Some(embeddings)) => {
                let mut lists = Vec::new();
                for embedding in embeddings {
                    lists.push(one(embedding)?);
                }
                Ok(json!(lists))
            }
            _ => Err(grafeno::Error::Invalid(
                "a vector search takes `embedding` or `embeddings`".into(),
            )),
        }
    })
    .await?;
    Ok(Json(found))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SimilarInput {
    node: NodeId,
    #[serde(default = "ten")]
    limit: usize,
    #[serde(default)]
    ef: Option<usize>,
    #[serde(default)]
    index: Option<String>,
    #[serde(default)]
    filter: Option<grafeno::VectorFilter>,
}

/// The nodes nearest to a node's own vector (16.7).
async fn similar_handler(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<SimilarInput>,
) -> ApiResult<Json<Vec<grafeno::Hit>>> {
    let hits = blocking(&state, move |db| {
        db.begin_read()?.similar(
            &ns,
            input.node,
            &grafeno::VectorSearch {
                index: input.index.clone(),
                limit: input.limit,
                ef: input.ef,
                filter: input.filter.clone(),
                exclude: None,
            },
        )
    })
    .await?;
    Ok(Json(hits))
}

async fn recall_handler(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<RecallInput>,
) -> ApiResult<Json<Vec<grafeno_memory::Recalled>>> {
    let query = input.resolve()?;
    let touch = input.touch;
    let read_ns = ns.clone();
    let results = blocking(&state, move |db| {
        recall(&db.begin_read()?, &read_ns, &query)
    })
    .await?;
    if touch {
        let ids: Vec<NodeId> = results.iter().map(|r| r.node.id).collect();
        write(&state, move |tx| tx.touch(&ns, &ids, Timestamp::now())).await?;
    }
    Ok(Json(results))
}

async fn context_handler(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<ContextInput>,
) -> ApiResult<Json<grafeno_memory::Context>> {
    let (query, options) = input.resolve()?;
    let touch = input.query.touch;
    let read_ns = ns.clone();
    let context = blocking(&state, move |db| {
        context_for(&db.begin_read()?, &read_ns, &query, &options)
    })
    .await?;
    if touch {
        let ids: Vec<NodeId> = context.items.iter().map(|item| item.node).collect();
        write(&state, move |tx| tx.touch(&ns, &ids, Timestamp::now())).await?;
    }
    Ok(Json(context))
}

/// One operation of an atomic batch.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BatchOp {
    /// Creates a node; `ref` (`$name`) lets later operations point to it.
    CreateNode {
        #[serde(default, rename = "ref")]
        reference: Option<String>,
        node: NodeInput,
    },
    /// Creates or replaces the node with `node.id`.
    UpsertNode {
        #[serde(default, rename = "ref")]
        reference: Option<String>,
        node: NodeInput,
    },
    UpdateNode {
        id: NodeRef,
        patch: PatchNode,
    },
    DeleteNode {
        id: NodeRef,
    },
    CreateEdge {
        edge: EdgeInput,
    },
    DeleteEdge {
        id: EdgeId,
    },
    DeleteEdgesFrom {
        from: NodeRef,
        #[serde(default)]
        label: Option<String>,
    },
    InvalidateEdge {
        id: EdgeId,
        at: TimeInput,
    },
    RetractEdge {
        id: EdgeId,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchInput {
    ops: Vec<BatchOp>,
}

#[derive(Serialize)]
struct BatchOutput {
    results: Vec<Value>,
    refs: HashMap<String, NodeId>,
}

fn remember_ref(
    refs: &mut HashMap<String, NodeId>,
    reference: Option<String>,
    id: NodeId,
) -> grafeno::Result<()> {
    if let Some(reference) = reference {
        if !reference.starts_with('$') || reference.len() < 2 {
            return Err(Error::Invalid(format!(
                "reference {reference:?} must look like \"$name\""
            )));
        }
        refs.insert(reference, id);
    }
    Ok(())
}

fn apply_op(
    tx: &mut WriteTx,
    ns: &str,
    op: BatchOp,
    refs: &mut HashMap<String, NodeId>,
) -> grafeno::Result<Value> {
    Ok(match op {
        BatchOp::CreateNode { reference, node } => {
            if node.id.is_some() {
                return Err(Error::Invalid(
                    "create_node does not take an id; use upsert_node".into(),
                ));
            }
            let id = tx.create_node(ns, node.into_new())?;
            remember_ref(refs, reference, id)?;
            json!({ "id": id, "created": true })
        }
        BatchOp::UpsertNode { reference, node } => {
            let id = node
                .id
                .ok_or_else(|| Error::Invalid("upsert_node needs node.id".into()))?;
            let created = tx.upsert_node(ns, id, node.into_new())?;
            remember_ref(refs, reference, id)?;
            json!({ "id": id, "created": created })
        }
        BatchOp::UpdateNode { id, patch } => {
            let id = resolve_ref(&id, refs)?;
            apply_patch(tx, ns, id, patch)?;
            json!({ "id": id })
        }
        BatchOp::DeleteNode { id } => {
            tx.delete_node(ns, resolve_ref(&id, refs)?)?;
            json!({})
        }
        BatchOp::CreateEdge { edge } => {
            let id = tx.create_edge(ns, edge.into_new(refs)?)?;
            json!({ "id": id })
        }
        BatchOp::DeleteEdge { id } => {
            tx.delete_edge(ns, id)?;
            json!({})
        }
        BatchOp::DeleteEdgesFrom { from, label } => {
            let deleted = tx.delete_edges_from(ns, resolve_ref(&from, refs)?, label.as_deref())?;
            json!({ "deleted": deleted })
        }
        BatchOp::InvalidateEdge { id, at } => {
            tx.invalidate_edge(ns, id, at.resolve()?)?;
            json!({})
        }
        BatchOp::RetractEdge { id } => {
            tx.retract_edge(ns, id)?;
            json!({})
        }
    })
}

/// Applies every operation in one transaction: all of them or none.
async fn batch(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<BatchInput>,
) -> ApiResult<Json<BatchOutput>> {
    let writer = state.writer.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let failed_at = Arc::new(std::sync::Mutex::new(None));
        let failed = failed_at.clone();
        let result = writer.write(move |tx| {
            let mut refs = HashMap::new();
            let mut results = Vec::with_capacity(input.ops.len());
            for (index, op) in input.ops.iter().cloned().enumerate() {
                match apply_op(tx, &ns, op, &mut refs) {
                    Ok(value) => results.push(value),
                    Err(error) => {
                        *failed.lock().expect("not poisoned") = Some(index);
                        return Err(error);
                    }
                }
            }
            Ok(BatchOutput { results, refs })
        });
        let failed_at = *failed_at.lock().expect("not poisoned");
        result.map_err(|error| (failed_at, error))
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?;

    match outcome {
        Ok(output) => Ok(Json(output)),
        Err((failed_at, error)) => {
            let mut api_error = ApiError::from(error);
            if let Some(index) = failed_at {
                api_error.message = format!("operation {index}: {}", api_error.message);
            }
            Err(api_error)
        }
    }
}

// ---------------------------------------------------------------- administration

const IMPORT_LIMIT: usize = 1024 * 1024 * 1024;

async fn namespaces(State(state): State<AppState>) -> ApiResult<Json<Vec<grafeno::NamespaceInfo>>> {
    let caller = auth::current();
    let registry = state.groups.get().cloned();
    let namespaces = blocking(&state, move |db| {
        let mut list = match &registry {
            // Every group's namespaces, each from where it lives (13.5).
            Some(registry) => crate::groups::namespaces(registry)?,
            None => db.begin_read()?.namespaces()?,
        };
        // A namespace seen record by record counts what the caller sees
        // (14.4); on several groups, its counts are left out.
        let read = db.begin_read()?;
        for info in &mut list {
            if read.view().is_some_and(|view| view.per_record(&info.name)) {
                let (nodes, edges) = match registry {
                    None => {
                        let stats = read.stats(&info.name)?;
                        (stats.nodes, stats.edges)
                    }
                    Some(_) => (0, 0),
                };
                info.nodes = nodes;
                info.edges = edges;
            }
        }
        Ok(list)
    })
    .await?;
    Ok(Json(
        namespaces
            .into_iter()
            .filter(|info| caller.may(&info.name, Role::Read))
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewNamespace {
    name: String,
    description: Option<String>,
}

/// Creates an empty namespace: 201, or 409 if it exists.
async fn create_namespace(
    State(state): State<AppState>,
    Body(input): Body<NewNamespace>,
) -> ApiResult<(StatusCode, Json<grafeno::NamespaceInfo>)> {
    require_all(&input.name, &[grafeno::Act::CreateNamespace])?;
    let info = write(&state, move |tx| {
        tx.create_namespace(&input.name, input.description.as_deref())?;
        tx.namespace(&input.name)?
            .ok_or_else(|| Error::NamespaceNotFound(input.name.clone()))
    })
    .await?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn get_namespace(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Json<grafeno::NamespaceInfo>> {
    let info = blocking(&state, move |db| {
        db.begin_read()?
            .namespace(&ns)?
            .ok_or(Error::NamespaceNotFound(ns))
    })
    .await?;
    Ok(Json(info))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamespacePatch {
    description: Option<String>,
}

/// Sets the description (`null` removes it).
async fn describe_namespace(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<NamespacePatch>,
) -> ApiResult<Json<grafeno::NamespaceInfo>> {
    let info = write(&state, move |tx| {
        tx.describe_namespace(&ns, input.description.as_deref())?;
        tx.namespace(&ns)?
            .ok_or_else(|| Error::NamespaceNotFound(ns.clone()))
    })
    .await?;
    Ok(Json(info))
}

/// Deletes a namespace and everything in it.
async fn drop_namespace(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Json<grafeno::DropStats>> {
    let dropped = write(&state, move |tx| tx.drop_namespace(&ns)).await?;
    Ok(Json(dropped))
}

async fn stats(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Json<grafeno::NamespaceStats>> {
    let stats = blocking(&state, move |db| db.begin_read()?.stats(&ns)).await?;
    Ok(Json(stats))
}

async fn check(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Json<grafeno::CheckReport>> {
    whole_namespace(&ns, "checking integrity")?;
    let report = blocking(&state, move |db| db.begin_read()?.check(&ns)).await?;
    Ok(Json(report))
}

async fn rebuild(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Json<grafeno::RebuildReport>> {
    let report = write_bulk(&state, move |tx| tx.rebuild_indexes(&ns)).await?;
    Ok(Json(report))
}

/// Builds the namespace's vector graph again (admin).
#[derive(Deserialize)]
struct CompactParams {
    /// A vector index; the namespace's own vectors without it.
    index: Option<String>,
}

async fn compact(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<CompactParams>,
) -> ApiResult<Json<grafeno::CompactReport>> {
    let report = write_bulk(&state, move |tx| match &params.index {
        Some(index) => tx.compact_vector_index(&ns, index),
        None => tx.compact_vectors(&ns),
    })
    .await?;
    Ok(Json(report))
}

/// How worn each vector graph of the namespace is (removals since it was
/// built); reads only their metadata.
async fn vector_health(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Json<Vec<grafeno::VectorHealth>>> {
    whole_namespace(&ns, "vector health")?;
    let health = blocking(&state, move |db| db.begin_read()?.vector_health(&ns)).await?;
    Ok(Json(health))
}

#[derive(Deserialize)]
struct RecallParams {
    #[serde(default = "default_recall_sample")]
    sample: usize,
    #[serde(default = "default_recall_ef")]
    ef: usize,
}

fn default_recall_sample() -> usize {
    100
}

fn default_recall_ef() -> usize {
    64
}

/// An estimate of the recall@10 of vector searches, from a sample of the
/// namespace's own vectors (reads every vector once).
async fn vector_recall(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<RecallParams>,
) -> ApiResult<Json<Value>> {
    whole_namespace(&ns, "measuring vector recall")?;
    let sample = params.sample.clamp(1, 1_000);
    let ef = params.ef.clamp(1, 1_000);
    let recall = blocking(&state, move |db| {
        db.begin_read()?.vector_recall(&ns, sample, ef)
    })
    .await?;
    Ok(Json(
        json!({ "sample": sample, "ef": ef, "recall": recall }),
    ))
}

#[derive(Deserialize)]
struct ExportParams {
    /// Comma-separated namespaces; all of them when absent.
    ns: Option<String>,
}

/// The whole export as JSON Lines, from one consistent snapshot.
async fn export(
    State(state): State<AppState>,
    Params(params): Params<ExportParams>,
) -> ApiResult<Response> {
    let namespaces: Vec<String> = params
        .ns
        .iter()
        .flat_map(|names| names.split(','))
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    let caller = auth::current();
    for ns in &namespaces {
        require_all(ns, &[grafeno::Act::Export])?;
    }
    let body = blocking(&state, move |db| {
        let read = db.begin_read()?;
        // "Everything" is what the caller may read.
        let namespaces = if namespaces.is_empty() && !caller.is_owner() {
            let readable: Vec<String> = read
                .namespaces()?
                .into_iter()
                .map(|info| info.name)
                .filter(|ns| caller.may(ns, Role::Read))
                .collect();
            if readable.is_empty() {
                return Err(Error::Forbidden(format!(
                    "the key {:?} may read no namespace",
                    caller.name()
                )));
            }
            readable
        } else {
            namespaces
        };
        let mut body = Vec::new();
        read.export(&namespaces, &mut body)?;
        Ok(body)
    })
    .await?;
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response())
}

/// A copy of the whole database file, consistent, taken while the server
/// keeps serving (owners only): open it with `grafeno serve --db`.
async fn backup(State(state): State<AppState>) -> ApiResult<Response> {
    let bytes = blocking(&state, |db| {
        let path =
            std::env::temp_dir().join(format!("grafeno-backup-{}.redb", grafeno::NodeId::new()));
        let outcome = db.begin_read()?.backup(&path).and_then(|_| {
            std::fs::read(&path)
                .map_err(|error| Error::Storage(format!("cannot read the backup: {error}")))
        });
        let _ = std::fs::remove_file(&path);
        outcome
    })
    .await?;
    let name = format!(
        "attachment; filename=\"grafeno-{}.redb\"",
        Timestamp::now().date()
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_DISPOSITION, name),
        ],
        bytes,
    )
        .into_response())
}

/// Imports a JSON Lines export (up to 1 GiB) in one transaction.
#[derive(Deserialize)]
struct ImportParams {
    /// Load in chunks (faster; an error keeps the chunks before it).
    #[serde(default)]
    bulk: bool,
}

async fn import(
    State(state): State<AppState>,
    Params(params): Params<ImportParams>,
    request: Request,
) -> ApiResult<Json<grafeno::DumpStats>> {
    let body = axum::body::to_bytes(request.into_body(), IMPORT_LIMIT)
        .await
        .map_err(|error| ApiError::bad_request(format!("cannot read the export: {error}")))?;
    // Through the bulk lane, so agents keep writing during a load: in
    // chunks with `bulk`, else in one transaction (all of it or nothing).
    let chunk = if params.bulk {
        state.import_chunk
    } else {
        usize::MAX
    };
    let writer = state.writer.clone();
    let stats = tokio::task::spawn_blocking(move || writer.import(body.as_ref(), chunk))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))??;
    Ok(Json(stats))
}

// ---------------------------------------------------------------- queries

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryInput {
    query: String,
    #[serde(default)]
    params: serde_json::Map<String, Value>,
    /// A writing query with a key runs once: the same key again returns
    /// the first result without writing (also as the `Idempotency-Key`
    /// header).
    #[serde(default)]
    idempotency_key: Option<String>,
    /// Records how the statement walks the graph (16.9): `trace` in the
    /// result, as with `TRACE` before the statement.
    #[serde(default)]
    trace: bool,
}

impl From<grafeno_query::QueryError> for ApiError {
    fn from(error: grafeno_query::QueryError) -> Self {
        Self {
            status: if error.internal {
                StatusCode::INTERNAL_SERVER_ERROR
            } else if error.forbidden {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_REQUEST
            },
            code: if error.internal {
                "internal"
            } else if error.forbidden {
                "forbidden"
            } else if error.limit {
                "query_limit"
            } else {
                "invalid_query"
            },
            message: error.to_string(),
            details: None,
        }
    }
}

/// Runs one GQ statement; statements with writes run in one transaction.
/// Sets the flag when dropped.
struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

async fn query(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    headers: axum::http::HeaderMap,
    Body(input): Body<QueryInput>,
) -> ApiResult<Json<Value>> {
    let params = grafeno_query::params_from_json(&input.params);
    let key = input.idempotency_key.or_else(|| {
        headers
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    });
    let split = crate::cluster::SplitRequest {
        params: input.params,
        authorization: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        mode: crate::cluster::SplitMode::from_headers(&headers),
    };
    let query = if input.trace && !input.query.trim_start().to_uppercase().starts_with("TRACE") {
        format!("TRACE {}", input.query)
    } else {
        input.query
    };
    let started = std::time::Instant::now();
    let outcome = run_query_counted(&state, ns, query, params, key, Some(split)).await;
    let limit = match &outcome {
        Err(error) if error.code == "query_limit" => Some(error.message.as_str()),
        _ => None,
    };
    state.metrics.query(started.elapsed().as_secs_f64(), limit);
    outcome.map(Json)
}

/// One part of a read split across cluster members (13.4), asked by the
/// member that splits it.
async fn query_part(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<crate::cluster::PartRequest>,
) -> ApiResult<Json<Value>> {
    require(&ns, Role::Read)?;
    let Some(node) = state.writer.cluster().cloned() else {
        return Err(ApiError::bad_request("this server is not a cluster member"));
    };
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    let caller = auth::current();
    let view = caller.view();
    let limits = grafeno_query::Limits {
        cancel: Some(cancel),
        visible: (!caller.is_owner())
            .then(|| grafeno_query::NamespaceFilter::new(move |ns| caller.may(ns, Role::Read))),
        view,
        ..state.limits.clone()
    };
    tokio::task::spawn_blocking(move || crate::cluster::read_part(&node, &ns, &input, &limits))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map(Json)
}

/// State kind of the results of keyed writing queries.
const IDEMPOTENT: &str = "idempotency";

/// Runs a GQ query, counted in the metrics.
async fn run_query(
    state: &AppState,
    ns: String,
    query: String,
    params: grafeno_query::Params,
) -> ApiResult<Value> {
    run_query_keyed(state, ns, query, params, None).await
}

async fn run_query_keyed(
    state: &AppState,
    ns: String,
    query: String,
    params: grafeno_query::Params,
    key: Option<String>,
) -> ApiResult<Value> {
    let started = std::time::Instant::now();
    let outcome = run_query_counted(state, ns, query, params, key, None).await;
    let limit = match &outcome {
        Err(error) if error.code == "query_limit" => Some(error.message.as_str()),
        _ => None,
    };
    state.metrics.query(started.elapsed().as_secs_f64(), limit);
    outcome
}

/// `POST /v1/{ns}/transactions`: begins an explicit transaction (16.4).
async fn begin_transaction(
    State(state): State<AppState>,
    Path(_ns): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let input: crate::transactions::BeginInput = if body.iter().all(u8::is_ascii_whitespace) {
        Default::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let begun = crate::transactions::begin(&state, input).await?;
    Ok((StatusCode::CREATED, Json(begun)))
}

async fn get_transaction(
    State(state): State<AppState>,
    Path((_ns, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    Ok(Json(crate::transactions::describe(&state, &id)?))
}

/// Runs one GQ statement in the transaction.
async fn transaction_query(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
    Body(input): Body<QueryInput>,
) -> ApiResult<Json<Value>> {
    let params = grafeno_query::params_from_json(&input.params);
    let result = crate::transactions::query(&state, &id, ns, input.query, params).await?;
    Ok(Json(result))
}

async fn commit_transaction(
    State(state): State<AppState>,
    Path((_ns, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    Ok(Json(crate::transactions::commit(&state, &id).await?))
}

async fn rollback_transaction(
    State(state): State<AppState>,
    Path((_ns, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        crate::transactions::rollback_route(&state, &id).await?,
    ))
}

/// A statement checked against its caller: the limits it runs under, what
/// an `EXPLAIN` of it would be denied, and its kind.
pub(crate) struct Checked {
    pub(crate) limits: grafeno_query::Limits,
    pub(crate) denied: Option<String>,
    pub(crate) kind: grafeno_query::StatementKind,
}

/// The rights a GQ statement needs in `ns`, checked for the current caller
/// (refused with 403 unless it is an `EXPLAIN`), and the limits it runs
/// under: for `/query` and for statements in a transaction (16.4).
pub(crate) fn check_statement(
    state: &AppState,
    ns: &str,
    query: &str,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> ApiResult<Checked> {
    let ns = ns.to_owned();
    let caller = auth::current();
    let view = caller.view();
    let context = auth::access_context(&caller);
    let needs = grafeno_query::requirements(query, Some(&context))?;
    // What the statement would lack if it ran: refused, or, under EXPLAIN,
    // said in the plan (14.3).
    let missing = match needs.kind {
        grafeno_query::StatementKind::Read => missing(&caller, &ns, Role::Read),
        // Each change is checked as it is made (14.4): a statement needs some
        // way to write here, and the write transaction checks the rest.
        grafeno_query::StatementKind::Write => {
            let writes = [
                grafeno::Act::Create,
                grafeno::Act::Update,
                grafeno::Act::Delete,
                grafeno::Act::Merge,
                grafeno::Act::Retract,
            ];
            (!writes.iter().any(|act| caller.can(*act, &ns)))
                .then(|| caller.refusal(&ns, &writes[..3]))
        }
        grafeno_query::StatementKind::Schema => missing(&caller, &ns, Role::Admin),
        // Subscriptions and their events show values as they change (16.10).
        grafeno_query::StatementKind::Events => (!caller.can(grafeno::Act::Events, &ns))
            .then(|| caller.refusal(&ns, &[grafeno::Act::Events])),
        grafeno_query::StatementKind::Access => needs.lacking.map(|privilege| {
            format!(
                "{:?} has no `{privilege}` privilege on the server",
                caller.name()
            )
        }),
        // On the namespace it names, as the namespace routes check (16.3).
        grafeno_query::StatementKind::Namespace => needs
            .namespace
            .as_ref()
            .filter(|(target, act)| !caller.can(*act, target))
            .map(|(target, act)| caller.refusal(target, &[*act])),
    };
    if let (Some(message), false) = (&missing, needs.explain) {
        return Err(ApiError::forbidden(message.clone()));
    }
    // Even under EXPLAIN, a plan of a namespace is for those who read it.
    if !matches!(
        needs.kind,
        grafeno_query::StatementKind::Access | grafeno_query::StatementKind::Namespace
    ) {
        require(&ns, Role::Read)?;
    }
    // With several groups, a namespace lives in the group the directory
    // says (group 0 when it says nothing): a statement on it must run there.
    if let (Some((target, _)), Some(registry)) = (&needs.namespace, state.groups.get()) {
        let home = registry.placement(target).group;
        if home != state.group {
            return Err(ApiError::bad_request(format!(
                "namespace {target:?} belongs to group {home}: run this statement against a namespace of that group, or use /v1/namespaces"
            )));
        }
    }
    let denied = missing.filter(|_| needs.explain);
    let limits = grafeno_query::Limits {
        cancel: Some(cancel),
        visible: (!caller.is_owner())
            .then(|| grafeno_query::NamespaceFilter::new(move |ns| caller.may(ns, Role::Read))),
        access: Some(context),
        view: view.clone(),
        ..state.limits.clone()
    };
    Ok(Checked {
        limits,
        denied,
        kind: needs.kind,
    })
}

/// Runs a GQ query: reads on a snapshot, writes in the next group commit.
async fn run_query_counted(
    state: &AppState,
    ns: String,
    query: String,
    params: grafeno_query::Params,
    key: Option<String>,
    split: Option<crate::cluster::SplitRequest>,
) -> ApiResult<Value> {
    let db = state.db.clone();
    // When the client goes away, this handler is dropped: the guard then
    // tells the query to stop.
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    let Checked {
        limits,
        denied,
        kind,
    } = check_statement(state, &ns, &query, cancel)?;
    let access = kind == grafeno_query::StatementKind::Access;
    if !grafeno_query::is_write(&query)? {
        let writer = state.writer.clone();
        return tokio::task::spawn_blocking(move || -> ApiResult<Value> {
            // On a cluster: the member's latest snapshot, split across the
            // members when it pays (13.4).
            if let (Some(node), Some(split)) = (writer.cluster(), &split) {
                return crate::cluster::read(&writer, node, &ns, &query, &params, &limits, split);
            }
            let mut result =
                grafeno_query::execute_read_with(&db.begin_read()?, &ns, &query, &params, &limits)?
                    .to_json();
            if let (Some(message), Some(rows)) = (denied, result["rows"].as_array_mut()) {
                let step = rows.len() + 1;
                rows.push(json!([step, "DENIED", message]));
            }
            Ok(result)
        })
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    }
    // Writes join the next group commit. A failing query must make the
    // writer discard the group's transaction, so its error travels as an
    // engine error and the query error itself is kept aside.
    let failure: Arc<std::sync::Mutex<Option<grafeno_query::QueryError>>> = Arc::default();
    let kept = failure.clone();
    let (token, had) = (state.token.is_some(), state.keys.has_accounts());
    let outcome = write(state, move |tx| {
        // Written already under this key: the first result, nothing more.
        if let Some(key) = &key
            && let Some(bytes) = tx.state(&ns, IDEMPOTENT, key)?
        {
            return serde_json::from_slice::<Value>(&bytes)
                .map_err(|error| Error::Serialization(error.to_string()));
        }
        let read = db.begin_read()?;
        let result = grafeno_query::execute_in(tx, &read, &ns, &query, &params, &limits)
            .map_err(|error| {
                let message = error.to_string();
                *kept.lock().expect("not poisoned") = Some(error);
                Error::Invalid(message)
            })?
            .to_json();
        if access {
            crate::accounts::keep_a_superuser(tx, token, had)?;
        }
        if let Some(key) = &key {
            let bytes = serde_json::to_vec(&result)
                .map_err(|error| Error::Serialization(error.to_string()))?;
            tx.put_state(&ns, IDEMPOTENT, key, &bytes)?;
        }
        Ok(result)
    })
    .await;
    match outcome {
        Ok(result) => {
            // Accounts and keys changed: dropped accounts' keys stop working.
            if access {
                state.keys.reload(&state.db)?;
            }
            Ok(result)
        }
        Err(error) => match failure.lock().expect("not poisoned").take() {
            Some(query_error) => Err(query_error.into()),
            None => Err(error),
        },
    }
}

// ---------------------------------------------------------------- keys

// ---------------------------------------------------------------- algorithms

/// The graph algorithms, as `CALL dbms.procedures()` lists them.
async fn algorithms() -> Json<Value> {
    Json(Value::Array(
        grafeno_query::algorithms()
            .map(|algorithm| {
                json!({
                    "name": algorithm.name,
                    "arguments": algorithm.args,
                    "columns": algorithm.columns,
                    "description": algorithm.description,
                })
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AlgorithmInput {
    /// The arguments before the settings: nodes by id, `k`.
    #[serde(default)]
    args: Vec<Value>,
    #[serde(default)]
    settings: serde_json::Map<String, Value>,
    /// Stores the result in this node property (or `@embedding`) instead
    /// of returning rows.
    write: Option<String>,
    /// With `write`, the column to store; the one after `node` by default.
    column: Option<String>,
    /// Statistics of the rows instead of the rows (16.8): per numeric
    /// column, how many, the least, the most, the mean and the sum.
    #[serde(default)]
    stats: bool,
}

/// `CALL algo.<name>` over HTTP: the rows it yields (with nodes as
/// `{@id, @labels, name}`), or, with `write`, how many nodes got the result.
async fn algorithm(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    Body(input): Body<AlgorithmInput>,
) -> ApiResult<Json<Value>> {
    let wanted = format!("algo.{name}");
    let algorithm = grafeno_query::algorithms()
        .find(|algorithm| {
            algorithm.name.eq_ignore_ascii_case(&wanted)
                || algorithm.name.eq_ignore_ascii_case(&name)
        })
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "no algorithm {name} (GET /v1/algorithms lists them)"
            ))
        })?;
    let mut params = grafeno_query::Params::new();
    let mut call = Vec::new();
    for (at, arg) in input.args.iter().enumerate() {
        params.insert(format!("arg{at}"), grafeno_query::Value::from_json(arg));
        call.push(format!("$arg{at}"));
    }
    if input.stats && input.write.is_some() {
        return Err(ApiError::bad_request(
            "an algorithm writes or gives statistics, not both",
        ));
    }
    if input.settings.contains_key("mode") || input.settings.contains_key("write") {
        return Err(ApiError::bad_request(
            "the mode goes in `stats` and the property in `write`, beside `settings`",
        ));
    }
    if input.stats {
        // The settings as a map written in the query (each value still a
        // parameter), so that GQ sees the mode before it runs.
        let mut entries = vec!["mode: 'stats'".to_owned()];
        for (at, (key, value)) in input.settings.iter().enumerate() {
            let plain = key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !plain {
                return Err(ApiError::bad_request(format!(
                    "no setting {key:?}: settings are names of letters, digits and _"
                )));
            }
            params.insert(
                format!("setting{at}"),
                grafeno_query::Value::from_json(value),
            );
            entries.push(format!("{key}: $setting{at}"));
        }
        call.push(format!("{{{}}}", entries.join(", ")));
    } else {
        params.insert(
            "settings".into(),
            grafeno_query::Value::from_json(&Value::Object(input.settings)),
        );
        call.push("$settings".into());
    }
    let call = format!("CALL {}({})", algorithm.name, call.join(", "));
    let query = match input.write {
        None if input.stats => format!(
            "{call} YIELD column, count, min, max, mean, sum RETURN column, count, min, max, mean, sum"
        ),
        None => {
            let columns: Vec<String> = algorithm
                .columns
                .iter()
                .map(|column| match *column {
                    "node" | "other" | "agent" | "task" | "left" | "right" | "start"
                    | "nearest" => {
                        format!("{{id: {column}.@id, labels: {column}.@labels, name: {column}.name}} AS {column}")
                    }
                    "nodes" | "walk" => format!(
                        "[n IN {column} | {{id: n.@id, labels: n.@labels, name: n.name}}] AS {column}"
                    ),
                    "edges" => "[e IN edges | e.@id] AS edges".to_owned(),
                    "edge" => "edge.@id AS edge".to_owned(),
                    other => other.to_owned(),
                })
                .collect();
            format!(
                "{call} YIELD {} RETURN {}",
                algorithm.columns.join(", "),
                columns.join(", ")
            )
        }
        Some(property) => {
            let valid = property == "@embedding"
                || (!property.is_empty()
                    && property
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !property.starts_with(|c: char| c.is_ascii_digit()));
            if !valid {
                return Err(ApiError::bad_request(format!(
                    "write needs a property name (letters, digits, _) or @embedding, not {property:?}"
                )));
            }
            if algorithm.columns.first() != Some(&"node") || algorithm.columns.len() < 2 {
                return Err(ApiError::bad_request(format!(
                    "{} yields no column per node to write",
                    algorithm.name
                )));
            }
            let column = input
                .column
                .unwrap_or_else(|| algorithm.columns[1].to_owned());
            if !algorithm.columns.contains(&column.as_str()) || column == "node" {
                return Err(ApiError::bad_request(format!(
                    "{} yields {}: no column {column}",
                    algorithm.name,
                    algorithm.columns.join(", ")
                )));
            }
            let target = if property == "@embedding" {
                "node.@embedding".to_owned()
            } else {
                format!("node.{property}")
            };
            format!(
                "{call} YIELD node, {column} SET {target} = {column} RETURN count(node) AS written"
            )
        }
    };
    run_query(&state, ns, query, params).await.map(Json)
}

// ---------------------------------------------------------------- memory

#[derive(Deserialize)]
struct ResolveParams {
    name: String,
    label: Option<String>,
}

/// Nodes named `name` (or with it as an alias), compared normalized.
async fn resolve(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<ResolveParams>,
) -> ApiResult<Json<Vec<grafeno::Node>>> {
    whole_namespace(&ns, "resolving names")?;
    let nodes = blocking(&state, move |db| {
        let read = db.begin_read()?;
        let mut nodes = Vec::new();
        for id in read.find_by_name(&ns, &params.name)? {
            if let Some(node) = read.node(&ns, id)?
                && params
                    .label
                    .as_deref()
                    .is_none_or(|label| node.has_label(label))
            {
                nodes.push(node);
            }
        }
        Ok(nodes)
    })
    .await?;
    Ok(Json(nodes))
}

#[derive(Deserialize)]
struct FuzzyParams {
    label: String,
    key: String,
    q: String,
    limit: Option<usize>,
    /// A measure name (`dice` by default, `jaro_winkler`, `edit`, …).
    measure: Option<String>,
}

fn measure(name: Option<&str>) -> ApiResult<grafeno::textsim::Measure> {
    match name {
        None => Ok(grafeno::textsim::Measure::Dice { n: 3 }),
        Some(name) => grafeno::textsim::Measure::by_name(name)
            .ok_or_else(|| ApiError::bad_request(format!("unknown measure {name:?}"))),
    }
}

/// Nodes whose property looks like `q`, through the fuzzy index on it.
async fn fuzzy(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<FuzzyParams>,
) -> ApiResult<Json<Vec<grafeno::FuzzyHit>>> {
    whole_namespace(&ns, "fuzzy matching")?;
    let measure = measure(params.measure.as_deref())?;
    let limit = params.limit.unwrap_or(10).min(MAX_PAGE);
    let hits = blocking(&state, move |db| {
        db.begin_read()?
            .fuzzy_search(&ns, &params.label, &params.key, &params.q, measure, limit)
    })
    .await?;
    Ok(Json(hits))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchInput {
    label: String,
    #[serde(default)]
    props: grafeno::Props,
    /// Nodes the record is linked to, for rules that weigh neighbors.
    #[serde(default)]
    neighbors: Vec<NodeId>,
    #[serde(default)]
    limit: Option<usize>,
    /// A rule to try instead of the stored one.
    #[serde(default)]
    rule: Option<grafeno::MatchRule>,
}

/// Nodes that may be this record, with scores, decisions and fields.
async fn find_matches(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<MatchInput>,
) -> ApiResult<Json<Vec<grafeno::Match>>> {
    whole_namespace(&ns, "entity resolution")?;
    let limit = input.limit.unwrap_or(10).min(MAX_PAGE);
    let found = blocking(&state, move |db| {
        let probe = grafeno::Probe {
            label: input.label,
            props: input.props,
            neighbors: input.neighbors,
        };
        db.begin_read()?
            .find_matches(&ns, &probe, input.rule.as_ref(), limit, None)
    })
    .await?;
    Ok(Json(found))
}

/// The constraints on facts and properties (`CREATE CONSTRAINT` declares
/// them).
async fn constraints(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Json<Vec<grafeno::Constraint>>> {
    let found = blocking(&state, move |db| db.begin_read()?.constraints(&ns)).await?;
    Ok(Json(found))
}

#[derive(Deserialize)]
struct ContradictionsParams {
    limit: Option<usize>,
}

/// What constraints did: facts ended, shortened, replaced or flagged,
/// newest first.
async fn contradictions(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<ContradictionsParams>,
) -> ApiResult<Json<Vec<grafeno::ConstraintEvent>>> {
    whole_namespace(&ns, "listing contradictions")?;
    let limit = params.limit.unwrap_or(50).min(MAX_PAGE);
    let events = blocking(&state, move |db| {
        db.begin_read()?.constraint_events(&ns, limit)
    })
    .await?;
    Ok(Json(events))
}

/// The earlier values of a functional property of a node.
async fn property_history(
    State(state): State<AppState>,
    Path((ns, id, key)): Path<(String, NodeId, String)>,
) -> ApiResult<Json<Vec<grafeno::HistoryEntry>>> {
    let entries = blocking(&state, move |db| {
        db.begin_read()?.property_history(&ns, id, &key)
    })
    .await?;
    Ok(Json(entries))
}

/// A namespace as GraphML, for graph drawing and analysis tools.
async fn export_graphml(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Response> {
    let body = blocking(&state, move |db| {
        let mut out = Vec::new();
        db.begin_read()?.export_graphml(&ns, &mut out)?;
        Ok(out)
    })
    .await?;
    Ok(([(header::CONTENT_TYPE, "application/graphml+xml")], body).into_response())
}

/// A namespace's nodes as CSV, with typed headers.
async fn export_nodes_csv(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Response> {
    let body = blocking(&state, move |db| {
        let mut out = Vec::new();
        db.begin_read()?.export_nodes_csv(&ns, &mut out)?;
        Ok(out)
    })
    .await?;
    Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], body).into_response())
}

/// A namespace's current facts as CSV.
async fn export_edges_csv(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Response> {
    let body = blocking(&state, move |db| {
        let mut out = Vec::new();
        db.begin_read()?.export_edges_csv(&ns, &mut out)?;
        Ok(out)
    })
    .await?;
    Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], body).into_response())
}

async fn interchange_body(request: Request) -> ApiResult<axum::body::Bytes> {
    axum::body::to_bytes(request.into_body(), IMPORT_LIMIT)
        .await
        .map_err(|error| ApiError::bad_request(format!("cannot read the file: {error}")))
}

async fn import_graphml(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    request: Request,
) -> ApiResult<Json<grafeno::InterchangeStats>> {
    let body = interchange_body(request).await?;
    let stats = blocking(&state, move |db| {
        db.import_graphml(&ns, body.as_ref(), &grafeno::InterchangeOptions::default())
    })
    .await?;
    Ok(Json(stats))
}

async fn import_nodes_csv(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    request: Request,
) -> ApiResult<Json<grafeno::InterchangeStats>> {
    let body = interchange_body(request).await?;
    let stats = blocking(&state, move |db| {
        db.import_nodes_csv(&ns, body.as_ref(), &grafeno::InterchangeOptions::default())
    })
    .await?;
    Ok(Json(stats))
}

async fn import_edges_csv(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    request: Request,
) -> ApiResult<Json<grafeno::InterchangeStats>> {
    let body = interchange_body(request).await?;
    let stats = blocking(&state, move |db| {
        db.import_edges_csv(&ns, body.as_ref(), &grafeno::InterchangeOptions::default())
    })
    .await?;
    Ok(Json(stats))
}

#[derive(Deserialize)]
struct ChangesParams {
    since: Option<u64>,
    limit: Option<usize>,
    /// Keep the response open and send changes as they are committed
    /// (server-sent events); also with `Accept: text/event-stream`.
    stream: Option<bool>,
}

/// The namespace's change log after `since`: a page of JSON, or a stream
/// of server-sent events (`id` is the sequence number, so a reconnecting
/// client resumes with `Last-Event-ID`; a `gap` event says changes were
/// pruned before it read them).
async fn changes(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<ChangesParams>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Response> {
    let wants_stream = params.stream.unwrap_or(false)
        || headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let since = resume.or(params.since).unwrap_or(0);
    if !wants_stream {
        let limit = params.limit.unwrap_or(500).min(MAX_PAGE);
        let page = blocking(&state, move |db| {
            db.begin_read()?.changes(&ns, since, limit)
        })
        .await?;
        return Ok(Json(page).into_response());
    }
    let feed = crate::events::Feed {
        db: state.db.clone(),
        ns,
        cursor: since,
        commits: state.events.subscribe(),
        queue: Default::default(),
        gap_told: false,
        view: auth::current().view(),
        subscription: None,
    };
    let stream = futures_util::stream::unfold(feed, |feed| async move {
        feed.next()
            .await
            .map(|(event, feed)| (Ok::<_, std::convert::Infallible>(event), feed))
    });
    Ok(axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response())
}

/// The events of a subscription (16.10) after `since`, with their values:
/// a page of JSON (`changes` holds the events; continue after `through`),
/// or server-sent events named `event` (as `/changes` streams).
async fn subscription_events(
    State(state): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    Params(params): Params<ChangesParams>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Response> {
    let wants_stream = params.stream.unwrap_or(false)
        || headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let since = resume.or(params.since).unwrap_or(0);
    let view = auth::current().view();
    // Refused or unknown before a stream opens.
    let limit = if wants_stream {
        1
    } else {
        params.limit.unwrap_or(500).min(MAX_PAGE)
    };
    let (read_ns, read_name, read_view) = (ns.clone(), name.clone(), view.clone());
    let page = blocking(&state, move |db| {
        db.begin_read()?
            .with_view(read_view)
            .events(&read_ns, &read_name, since, limit)
    })
    .await?;
    if !wants_stream {
        return Ok(Json(page).into_response());
    }
    let feed = crate::events::Feed {
        db: state.db.clone(),
        ns,
        cursor: since,
        commits: state.events.subscribe(),
        queue: Default::default(),
        gap_told: false,
        view,
        subscription: Some(name),
    };
    let stream = futures_util::stream::unfold(feed, |feed| async move {
        feed.next()
            .await
            .map(|(event, feed)| (Ok::<_, std::convert::Infallible>(event), feed))
    });
    Ok(axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response())
}

/// Lines of NDJSON per transaction while ingesting.
const INGEST_BATCH: usize = 500;

/// Streaming ingestion: NDJSON, one node or edge per line, read as it
/// arrives and written in batches in the writer's bulk lane. A line's
/// `key` makes it idempotent: sent again (a retry), it writes nothing.
async fn ingest(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    request: Request,
) -> ApiResult<Json<crate::events::IngestReport>> {
    use futures_util::StreamExt;
    let mut body = request.into_body().into_data_stream();
    let mut report = crate::events::IngestReport::default();
    let mut pending: Vec<u8> = Vec::new();
    let mut lines: Vec<Result<crate::events::IngestLine, String>> = Vec::new();
    let mut number = 1usize;
    let actor = auth::current().actor();
    let flush = |lines: Vec<Result<crate::events::IngestLine, String>>, first: usize| {
        let (writer, ns, actor) = (state.writer.clone(), ns.clone(), actor.clone());
        let lines = Arc::new(lines);
        async move {
            tokio::task::spawn_blocking(move || {
                writer.write_bulk(move |tx| {
                    tx.act_as(actor.clone());
                    let result = crate::events::ingest_batch(tx, &ns, first, &lines);
                    tx.act_as(None);
                    result
                })
            })
            .await
            .map_err(|error| ApiError::internal(error.to_string()))?
            .map_err(ApiError::from)
        }
    };
    let mut done = false;
    while !done {
        match body.next().await {
            Some(Ok(chunk)) => pending.extend_from_slice(&chunk),
            Some(Err(error)) => return Err(ApiError::bad_request(error.to_string())),
            None => {
                done = true;
                pending.push(b'\n');
            }
        }
        // Whole lines of what has arrived; the rest waits for more.
        let mut start = 0;
        while let Some(length) = pending[start..].iter().position(|b| *b == b'\n') {
            let parsed = {
                let text = String::from_utf8_lossy(&pending[start..start + length]);
                let text = text.trim();
                (!text.is_empty()).then(|| serde_json::from_str(text).map_err(|e| e.to_string()))
            };
            start += length + 1;
            let Some(parsed) = parsed else {
                continue;
            };
            lines.push(parsed);
            if lines.len() >= INGEST_BATCH {
                let first = number;
                number += lines.len();
                report.add(flush(std::mem::take(&mut lines), first).await?);
            }
        }
        pending.drain(..start);
    }
    if !lines.is_empty() {
        report.add(flush(lines, number).await?);
    }
    Ok(Json(report))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookInput {
    url: String,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    /// Deliver from after this change; from now when not given.
    #[serde(default)]
    since: Option<u64>,
}

async fn list_webhooks(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Json<Vec<Value>>> {
    let hooks = blocking(&state, move |db| {
        crate::events::webhooks(&db.begin_read()?, &ns)
    })
    .await?;
    Ok(Json(
        hooks.iter().map(crate::events::Webhook::public).collect(),
    ))
}

/// Registers a URL that receives the namespace's changes (POSTed as
/// `{namespace, webhook, changes, last, gap}`), signed when a secret is
/// given.
async fn create_webhook(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<WebhookInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !(input.url.starts_with("http://") || input.url.starts_with("https://")) {
        return Err(ApiError::bad_request("the url must be http:// or https://"));
    }
    let since = match input.since {
        Some(since) => since,
        None => {
            let ns = ns.clone();
            blocking(&state, move |db| {
                Ok(db.begin_read()?.changes(&ns, 0, 0)?.last)
            })
            .await?
        }
    };
    let hook = crate::events::Webhook {
        id: grafeno::NodeId::new().to_string(),
        url: input.url,
        secret: input.secret.filter(|s| !s.is_empty()),
        labels: input.labels,
        cursor: since,
        created_at: Timestamp::now(),
        delivered: 0,
        failures: 0,
        last_error: None,
        last_delivery: None,
    };
    let public = hook.public();
    write(&state, move |tx| {
        crate::events::save_webhook(tx, &ns, &hook)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(public)))
}

async fn get_webhook(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let hook = blocking(&state, move |db| {
        crate::events::webhooks(&db.begin_read()?, &ns)?
            .into_iter()
            .find(|hook| hook.id == id)
            .ok_or_else(|| Error::Invalid(format!("no webhook {id:?}")))
    })
    .await?;
    Ok(Json(hook.public()))
}

async fn delete_webhook(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let found = write(&state, move |tx| {
        tx.delete_state(&ns, crate::events::WEBHOOK, &id)
    })
    .await?;
    Ok(if found {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

/// What the background tasks have done.
fn member(state: &AppState) -> ApiResult<Arc<grafeno_cluster::Node>> {
    state.writer.cluster().cloned().ok_or_else(|| {
        ApiError::from(grafeno::Error::Invalid(
            "this server is not a cluster member".into(),
        ))
    })
}

/// This member's view of the cluster.
async fn cluster_status(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let node = member(&state)?;
    Ok(Json(json!(node.status())))
}

#[derive(serde::Deserialize)]
struct VotersBody {
    voters: Vec<u64>,
}

/// Changes the voting members (on the leader).
async fn cluster_voters(
    State(state): State<AppState>,
    Json(body): Json<VotersBody>,
) -> ApiResult<Json<Value>> {
    let node = member(&state)?;
    let index = tokio::task::spawn_blocking(move || node.change_membership(&body.voters))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(|error| ApiError::from(crate::cluster::engine_error(error)))?;
    Ok(Json(json!({ "index": index })))
}

/// Snapshots this member's database and drops its log up to it.
async fn cluster_compact(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let node = member(&state)?;
    let index = tokio::task::spawn_blocking(move || node.compact())
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(|error| ApiError::from(crate::cluster::engine_error(error)))?;
    Ok(Json(json!({ "snapshot_index": index })))
}

async fn background(State(state): State<AppState>) -> Json<Value> {
    let runners = state
        .runners
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Json(json!({ "config": state.background, "tasks": runners }))
}

/// Recomputes the community hierarchy and brings its summaries up to date
/// (the options of `CommunityOptions`, all optional, as the body). The
/// slow part runs on a read transaction; the writes go in one.
async fn refresh_communities(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<grafeno_memory::CommunityReport>> {
    whole_namespace(&ns, "summarizing communities")?;
    let options: grafeno_memory::CommunityOptions = if body.iter().all(u8::is_ascii_whitespace) {
        grafeno_memory::CommunityOptions::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let plan = {
        let ns = ns.clone();
        blocking(&state, move |db| {
            grafeno_memory::plan_communities(&db.begin_read()?, &ns, &options)
        })
        .await?
    };
    let plan = Arc::new(plan);
    let report = write(&state, move |tx| {
        grafeno_memory::apply_communities(tx, &ns, &plan)
    })
    .await?;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct CommunitiesParams {
    q: Option<String>,
    level: Option<i64>,
    limit: Option<usize>,
}

/// Global search over community summaries (`?q=`), or the top of the
/// hierarchy without one.
async fn communities(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<CommunitiesParams>,
) -> ApiResult<Json<Vec<grafeno_memory::CommunityHit>>> {
    whole_namespace(&ns, "community summaries")?;
    let options = grafeno_memory::OverviewOptions {
        limit: params.limit.unwrap_or(10).min(MAX_PAGE),
        level: params.level,
    };
    let hits = blocking(&state, move |db| {
        grafeno_memory::overview(&db.begin_read()?, &ns, params.q.as_deref(), &options)
    })
    .await?;
    Ok(Json(hits))
}

#[derive(Deserialize)]
struct CommunityParams {
    limit: Option<usize>,
}

/// One community: its summary, the summaries it is made of, and its
/// direct members, central ones first.
async fn community(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, NodeId)>,
    Params(params): Params<CommunityParams>,
) -> ApiResult<Json<grafeno_memory::CommunityMembers>> {
    whole_namespace(&ns, "community summaries")?;
    let limit = params.limit.unwrap_or(50).min(MAX_PAGE);
    let opened = blocking(&state, move |db| {
        grafeno_memory::community_members(&db.begin_read()?, &ns, id, limit)
    })
    .await?;
    Ok(Json(opened))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommunityText {
    /// The agent's own text; `null` goes back to the generated one.
    text: Option<String>,
}

/// Replaces a summary's text with an agent's own (`{"text": null}` undoes
/// it on the next refresh).
async fn community_text(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, NodeId)>,
    Body(input): Body<CommunityText>,
) -> ApiResult<Json<Value>> {
    whole_namespace(&ns, "community summaries")?;
    write(&state, move |tx| {
        grafeno_memory::set_community_text(tx, &ns, id, input.text.as_deref())
    })
    .await?;
    Ok(Json(json!({ "id": id })))
}

/// Reads the namespace's new and changed texts and reconciles what they
/// mention, a batch of texts per transaction in the writer's bulk lane.
async fn extract(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<grafeno_memory::ExtractReport>> {
    whole_namespace(&ns, "extraction")?;
    let options: grafeno_memory::ExtractOptions = if body.iter().all(u8::is_ascii_whitespace) {
        grafeno_memory::ExtractOptions::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let (db, writer) = (state.db.clone(), state.writer.clone());
    let actor = auth::current().actor();
    let report = tokio::task::spawn_blocking(move || -> grafeno::Result<_> {
        let options = Arc::new(options);
        let plan = Arc::new(grafeno_memory::plan(&db.begin_read()?, &ns, &options)?);
        let mut report = grafeno_memory::ExtractReport {
            unchanged: plan.unchanged,
            ..Default::default()
        };
        for batch in plan.sources.chunks(options.batch.max(1)) {
            let (db, plan, options, ns, actor) = (
                db.clone(),
                plan.clone(),
                options.clone(),
                ns.clone(),
                actor.clone(),
            );
            let batch = batch.to_vec();
            let done = writer.write_bulk(move |tx| {
                tx.act_as(actor.clone());
                let read = db.begin_read()?;
                let result = grafeno_memory::extract_batch(tx, &read, &ns, &plan, &batch, &options);
                tx.act_as(None);
                result
            })?;
            report.read += done.read;
            report.mentions += done.mentions;
            report.linked += done.linked;
            report.created += done.created;
            report.queued += done.queued;
            for (kind, count) in done.found {
                *report.found.entry(kind).or_default() += count;
            }
        }
        writer.flush()?;
        Ok(report)
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))??;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct ReviewParams {
    limit: Option<usize>,
}

/// What extraction could not decide: ambiguous names and new phrases.
async fn review_queue(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<ReviewParams>,
) -> ApiResult<Json<Vec<grafeno_memory::ReviewItem>>> {
    whole_namespace(&ns, "the review queue")?;
    let limit = params.limit.unwrap_or(50).min(MAX_PAGE);
    let items = blocking(&state, move |db| {
        grafeno_memory::review_queue(&db.begin_read()?, &ns, limit)
    })
    .await?;
    Ok(Json(items))
}

/// Settles a review item: `{"action": "link", "node": …}`, `{"action":
/// "create", "label": …}` or `{"action": "dismiss"}`.
async fn resolve_review(
    State(state): State<AppState>,
    Path((ns, key)): Path<(String, String)>,
    Body(resolution): Body<grafeno_memory::Resolution>,
) -> ApiResult<Json<Value>> {
    whole_namespace(&ns, "the review queue")?;
    let (node, report) = write(&state, move |tx| {
        grafeno_memory::resolve(tx, &ns, &key, resolution.clone())
    })
    .await?;
    Ok(Json(json!({ "node": node, "report": report })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MentionsInput {
    mentions: Vec<grafeno_memory::AgentMention>,
    /// The label of entities made for mentions without one.
    #[serde(default)]
    default_label: Option<String>,
}

/// An agent's mentions of a node's text, reconciled like extraction's.
async fn add_mentions(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, NodeId)>,
    Body(input): Body<MentionsInput>,
) -> ApiResult<Json<grafeno_memory::ExtractReport>> {
    whole_namespace(&ns, "extraction")?;
    let db = state.db.clone();
    let label = input.default_label.unwrap_or_else(|| "Entity".into());
    let report = write(&state, move |tx| {
        let read = db.begin_read()?;
        grafeno_memory::ingest(tx, &read, &ns, id, &input.mentions, &label)
    })
    .await?;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct PairsParams {
    label: String,
    limit: Option<usize>,
}

/// Pairs of nodes of a label that its rule calls the same or to review.
async fn match_pairs(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<PairsParams>,
) -> ApiResult<Json<Value>> {
    whole_namespace(&ns, "entity resolution")?;
    let limit = params.limit.unwrap_or(50).min(MAX_PAGE);
    let pairs = blocking(&state, move |db| {
        db.begin_read()?.match_pairs(&ns, &params.label, limit)
    })
    .await?;
    Ok(Json(json!(
        pairs
            .into_iter()
            .map(|(node, found)| json!({ "a": node, "b": found.node, "match": found }))
            .collect::<Vec<_>>()
    )))
}

async fn match_rules(
    State(state): State<AppState>,
    Path(ns): Path<String>,
) -> ApiResult<Json<Vec<grafeno::MatchRule>>> {
    let rules = blocking(&state, move |db| db.begin_read()?.match_rules(&ns)).await?;
    Ok(Json(rules))
}

/// Declares the match rule of a label (its `label` is the path's).
async fn set_match_rule(
    State(state): State<AppState>,
    Path((ns, label)): Path<(String, String)>,
    axum::Json(mut rule): axum::Json<Value>,
) -> ApiResult<Json<grafeno::MatchRule>> {
    if let Some(object) = rule.as_object_mut() {
        object.insert("label".into(), Value::from(label));
    }
    let rule: grafeno::MatchRule =
        serde_json::from_value(rule).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let stored = rule.clone();
    write(&state, move |tx| tx.set_match_rule(&ns, &rule)).await?;
    Ok(Json(stored))
}

async fn delete_match_rule(
    State(state): State<AppState>,
    Path((ns, label)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let existed = write(&state, move |tx| tx.delete_match_rule(&ns, &label)).await?;
    if existed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no match rule for that label"))
    }
}

#[derive(Deserialize)]
struct DuplicateParams {
    label: Option<String>,
    min_score: Option<f32>,
    /// Compare embeddings too (default true).
    vectors: Option<bool>,
    limit: Option<usize>,
}

async fn duplicates(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<DuplicateParams>,
) -> ApiResult<Json<Vec<grafeno_memory::Duplicate>>> {
    whole_namespace(&ns, "finding duplicates")?;
    let base = grafeno_memory::DuplicateOptions::default();
    let options = grafeno_memory::DuplicateOptions {
        label: params.label,
        min_score: params.min_score.unwrap_or(base.min_score),
        vector_threshold: if params.vectors.unwrap_or(true) {
            base.vector_threshold
        } else {
            None
        },
        limit: params.limit.unwrap_or(base.limit).min(MAX_PAGE),
        ..base
    };
    let found = blocking(&state, move |db| {
        grafeno_memory::find_duplicates(&db.begin_read()?, &ns, &options)
    })
    .await?;
    Ok(Json(found))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeInput {
    /// The node that stays.
    keep: NodeId,
    /// The node merged into it, then deleted.
    merge: NodeId,
    /// Merge in spite of conflicts (the kept node's values win).
    #[serde(default)]
    force: bool,
    /// Only report the conflicts; merge nothing.
    #[serde(default)]
    dry_run: bool,
    /// Edge labels a node has one of at a time (`born_in`).
    #[serde(default)]
    functional: Vec<String>,
    /// Identifying properties; `email` and `external_id` when missing.
    #[serde(default)]
    strong_keys: Option<Vec<String>>,
}

/// Merges two nodes. Conflicts (different emails or labels, two birth
/// dates) refuse the merge with 409 and the list of `conflicts`, unless
/// `force`; `dry_run` only lists them.
async fn merge(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<MergeInput>,
) -> ApiResult<Json<Value>> {
    whole_namespace(&ns, "merging")?;
    let base = grafeno::MergeOptions::default();
    let options = grafeno::MergeOptions {
        force: input.force,
        functional: input.functional,
        strong_keys: input.strong_keys.unwrap_or(base.strong_keys.clone()),
        ..base
    };
    let (keep, merged, dry_run) = (input.keep, input.merge, input.dry_run);
    let outcome = write(&state, move |tx| {
        let conflicts = tx.merge_conflicts(&ns, keep, merged, &options)?;
        if dry_run || (!conflicts.is_empty() && !options.force) {
            return Ok(Err(conflicts));
        }
        tx.merge_nodes_with(&ns, keep, merged, &options).map(Ok)
    })
    .await?;
    match outcome {
        Ok(report) => Ok(Json(
            serde_json::to_value(report).map_err(|e| ApiError::internal(e.to_string()))?,
        )),
        Err(conflicts) if dry_run => Ok(Json(json!({ "conflicts": conflicts }))),
        Err(conflicts) => {
            let listed: Vec<String> = conflicts.iter().map(grafeno::Conflict::describe).collect();
            let mut details = serde_json::Map::new();
            details.insert("conflicts".into(), json!(conflicts));
            Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "conflict",
                message: format!(
                    "the nodes disagree, so they may not be the same: {}; send force: true to merge anyway",
                    listed.join("; ")
                ),
                details: Some(details),
            })
        }
    }
}

#[derive(Deserialize)]
struct MergesParams {
    node: Option<NodeId>,
    limit: Option<usize>,
}

/// Merges recorded in the namespace, newest first (`?node=` for one node's).
async fn merges(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<MergesParams>,
) -> ApiResult<Json<Vec<grafeno::MergeRecord>>> {
    whole_namespace(&ns, "the merge log")?;
    let limit = params.limit.unwrap_or(50).min(MAX_PAGE);
    let mut records = blocking(&state, move |db| db.begin_read()?.merges(&ns, params.node)).await?;
    records.truncate(limit);
    Ok(Json(records))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct UndoInput {
    /// Undo even if the kept node changed since (its changes are lost).
    #[serde(default)]
    force: bool,
}

/// Undoes a merge: the merged node and its facts come back.
async fn undo_merge(
    State(state): State<AppState>,
    Path((ns, id)): Path<(String, grafeno::MergeId)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<grafeno::MergeRecord>> {
    whole_namespace(&ns, "the merge log")?;
    // The body is optional: none is `{}`.
    let input: UndoInput = if body.iter().all(u8::is_ascii_whitespace) {
        UndoInput::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let force = input.force;
    let record = write(&state, move |tx| tx.undo_merge(&ns, id, force)).await?;
    Ok(Json(record))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdsInput {
    ids: Vec<NodeId>,
}

/// Records an access to each node now (as recall with `touch` does).
async fn touch(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<IdsInput>,
) -> ApiResult<StatusCode> {
    write(&state, move |tx| {
        tx.touch(&ns, &input.ids, Timestamp::now())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct WeakParams {
    half_life_days: Option<f64>,
    threshold: Option<f32>,
    label: Option<String>,
    limit: Option<usize>,
}

async fn weak(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<WeakParams>,
) -> ApiResult<Json<Vec<grafeno_memory::WeakMemory>>> {
    whole_namespace(&ns, "memory decay")?;
    let base = grafeno_memory::DecayOptions::default();
    let options = grafeno_memory::DecayOptions {
        half_life_secs: params
            .half_life_days
            .map_or(base.half_life_secs, |days| days * 86_400.0),
        threshold: params.threshold.unwrap_or(base.threshold),
        labels: params.label.into_iter().collect(),
        limit: params.limit.unwrap_or(base.limit).min(MAX_PAGE),
        now: Timestamp::now(),
    };
    let found = blocking(&state, move |db| {
        grafeno_memory::weak_memories(&db.begin_read()?, &ns, &options)
    })
    .await?;
    Ok(Json(found))
}

/// Adds the `Archived` label: recall skips these nodes, nothing is deleted.
async fn archive_nodes(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<IdsInput>,
) -> ApiResult<Json<Value>> {
    let archived = write(&state, move |tx| {
        grafeno_memory::archive(tx, &ns, &input.ids)
    })
    .await?;
    Ok(Json(json!({ "archived": archived })))
}

#[derive(Deserialize)]
struct ConsolidationParams {
    older_than_days: Option<i64>,
    min_size: Option<usize>,
    limit: Option<usize>,
    /// Comma-separated labels of the memories to group.
    labels: Option<String>,
}

async fn consolidation(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Params(params): Params<ConsolidationParams>,
) -> ApiResult<Json<Vec<grafeno_memory::ConsolidationGroup>>> {
    whole_namespace(&ns, "consolidation")?;
    let base = grafeno_memory::ConsolidationOptions::default();
    let options = grafeno_memory::ConsolidationOptions {
        memory_labels: params
            .labels
            .map(|labels| {
                labels
                    .split(',')
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or(base.memory_labels),
        older_than_secs: params
            .older_than_days
            .map_or(base.older_than_secs, |days| days * 86_400),
        min_size: params.min_size.unwrap_or(base.min_size),
        limit: params.limit.unwrap_or(base.limit).min(MAX_PAGE),
        now: Timestamp::now(),
    };
    let groups = blocking(&state, move |db| {
        grafeno_memory::consolidation_groups(&db.begin_read()?, &ns, &options)
    })
    .await?;
    Ok(Json(groups))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryInput {
    text: String,
    sources: Vec<NodeId>,
    #[serde(default)]
    about: Option<NodeId>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    embedding: Option<Vec<f32>>,
    /// Archive the summarized memories (default true).
    #[serde(default)]
    archive_sources: Option<bool>,
}

/// Stores a summary of several memories (written by an agent), linked to
/// them with `summarizes`, and archives them.
async fn summaries(
    State(state): State<AppState>,
    Path(ns): Path<String>,
    Body(input): Body<SummaryInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    whole_namespace(&ns, "summaries")?;
    let summary = grafeno_memory::Summary {
        text: input.text,
        name: input.name,
        labels: input.labels,
        sources: input.sources,
        about: input.about,
        embedding: input.embedding,
        archive_sources: input.archive_sources.unwrap_or(true),
    };
    let id = write(&state, move |tx| {
        grafeno_memory::write_summary(tx, &ns, summary.clone())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}
