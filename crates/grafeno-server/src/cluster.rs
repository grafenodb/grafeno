//! Running the server as a member of a cluster (phase 13.2): its `[cluster]`
//! settings, the transport between members over HTTP, and the member's own
//! listener.
//!
//! Members talk on a port of their own (`[cluster] addr`), apart from the
//! API: `POST /raft` carries one Raft message, framed as a little-endian
//! `u32` length, the message as JSON, then a snapshot's bytes if it has
//! one. Every request carries the cluster secret in `x-grafeno-cluster`;
//! anything else is refused. The traffic is not encrypted yet: members
//! should talk over a private network (TLS between members comes later).
//!
//! Sending never blocks the member's Raft thread: each peer has a thread
//! with a bounded queue, and a message that does not fit, or does not get
//! through, is dropped (Raft resends what matters).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use grafeno_cluster::{Node, NodeOptions, Storage, Transport};
use grafeno_raft::{Envelope, Message, NodeId};
use serde::{Deserialize, Serialize};

const SECRET_HEADER: &str = "x-grafeno-cluster";
/// What the sending member runs (`{id, version, format, layout}`), with
/// every message (13.6).
const MEMBER_HEADER: &str = "x-grafeno-member";

/// A member of the cluster and where it listens for other members.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub id: NodeId,
    /// Where it listens for other members.
    pub addr: String,
    /// Its API (`host:port`), where followers forward what only the leader
    /// can serve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// `[cluster]` in `grafeno.toml`.
///
/// ```toml
/// [cluster]
/// id = 1
/// addr = "10.0.0.1:7811"      # where this member listens for the others
/// secret = "…"                # the same on every member
/// voters = [1, 2, 3]          # the first members (empty when joining)
/// peers = [
///   { id = 1, addr = "10.0.0.1:7811", api = "10.0.0.1:7810" },
///   { id = 2, addr = "10.0.0.2:7811", api = "10.0.0.2:7810" },
///   { id = 3, addr = "10.0.0.3:7811", api = "10.0.0.3:7810" },
/// ]
/// tick_ms = 50
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    pub id: NodeId,
    pub addr: SocketAddr,
    pub secret: String,
    #[serde(default)]
    pub voters: Vec<NodeId>,
    #[serde(default)]
    pub peers: Vec<Peer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_ms: Option<u64>,
    /// Reads whose estimated cost (as `EXPLAIN` shows it) reaches this are
    /// split across the members (phase 13.4); [`DEFAULT_SPLIT_COST`] by
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_cost: Option<u64>,
    /// More Raft groups (13.5), each with its own voters and database.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<crate::groups::GroupConfig>,
}

/// A message for a peer, with a snapshot's bytes.
type Outgoing = (Envelope, Option<Arc<Vec<u8>>>);

/// Sends messages to peers over HTTP, a thread and a queue per peer.
pub struct HttpTransport {
    peers: HashMap<NodeId, SyncSender<Outgoing>>,
}

impl HttpTransport {
    pub fn new(config: &ClusterConfig) -> Self {
        Self::for_group(config, 0)
    }

    /// The transport of group `group` (`/raft` for group 0, `/raft/{id}`
    /// for the others) to every other member.
    pub fn for_group(config: &ClusterConfig, group: u64) -> Self {
        let mut peers = HashMap::new();
        for peer in config.peers.iter().filter(|peer| peer.id != config.id) {
            let (sender, receiver) = sync_channel::<Outgoing>(1024);
            let url = if group == 0 {
                format!("http://{}/raft", peer.addr)
            } else {
                format!("http://{}/raft/{group}", peer.addr)
            };
            let secret = config.secret.clone();
            let mut me =
                serde_json::to_value(grafeno_cluster::MemberInfo::here()).unwrap_or_default();
            me["id"] = serde_json::json!(config.id);
            let me = me.to_string();
            std::thread::Builder::new()
                .name(format!("grafeno-raft-send-{}", peer.id))
                .spawn(move || {
                    let agent: ureq::Agent = ureq::Agent::config_builder()
                        .timeout_global(Some(Duration::from_secs(5)))
                        .timeout_connect(Some(CONNECT_TIMEOUT))
                        .http_status_as_error(false)
                        .build()
                        .into();
                    while let Ok(first) = receiver.recv() {
                        // Everything waiting by now, the stale appends left out.
                        let mut waiting = vec![first];
                        while let Ok(more) = receiver.try_recv() {
                            waiting.push(more);
                        }
                        for (envelope, snapshot) in coalesce(waiting) {
                            let Ok(json) = serde_json::to_vec(&envelope) else {
                                continue;
                            };
                            let mut body = Vec::with_capacity(4 + json.len());
                            body.extend_from_slice(&(json.len() as u32).to_le_bytes());
                            body.extend_from_slice(&json);
                            if let Some(snapshot) = snapshot {
                                body.extend_from_slice(&snapshot);
                            }
                            let _ = agent
                                .post(&url)
                                .header(SECRET_HEADER, &secret)
                                .header(MEMBER_HEADER, &me)
                                .send(&body[..]);
                        }
                    }
                })
                .expect("a sender thread starts");
            peers.insert(peer.id, sender);
        }
        Self { peers }
    }
}

/// Of the messages waiting for a peer, only the newest append is sent: an
/// append carries the leader's whole view of that peer (the entries from
/// where it stands to the end of the log, the commit index, the read
/// round), so the newest makes the others stale, and Raft makes up for a
/// message that never arrives. The other messages go as they are, in
/// order. Without this, a follower that fell behind under load got a queue
/// of appends each repeating the same entries, and learned of new ones
/// only after sending through all of them: seconds behind, and further
/// behind the more it lagged.
fn coalesce(waiting: Vec<Outgoing>) -> Vec<Outgoing> {
    let is_append = |(envelope, _): &Outgoing| matches!(envelope.message, Message::Append { .. });
    let newest = waiting.iter().rposition(is_append);
    waiting
        .into_iter()
        .enumerate()
        .filter(|(at, message)| !is_append(message) || Some(*at) == newest)
        .map(|(_, message)| message)
        .collect()
}

impl Transport for HttpTransport {
    fn send(&self, envelope: Envelope, snapshot: Option<Arc<Vec<u8>>>) {
        if let Some(sender) = self.peers.get(&envelope.to) {
            match sender.try_send((envelope, snapshot)) {
                Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
}

/// Compares secrets in time that does not depend on where they differ. An
/// empty secret matches nothing: a missing header reads as empty, and a
/// member with no secret would otherwise take any request.
pub(crate) fn same_secret(given: &[u8], expected: &[u8]) -> bool {
    !expected.is_empty()
        && given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[derive(Clone)]
struct Listener {
    node: Arc<Node>,
    nodes: Arc<HashMap<u64, Arc<Node>>>,
    secret: Arc<str>,
}

async fn receive_group(
    State(state): State<Listener>,
    axum::extract::Path(group): axum::extract::Path<u64>,
    request: axum::extract::Request,
) -> StatusCode {
    match state.nodes.get(&group).cloned() {
        Some(node) => receive(State(Listener { node, ..state }), request).await,
        None => StatusCode::NOT_FOUND,
    }
}

/// What a refused request to the members' port may still send before its
/// connection is closed.
const REFUSED_BODY: usize = 64 * 1024;

/// Whether a request carries this cluster's secret.
fn from_member(headers: &HeaderMap, secret: &str) -> bool {
    let given = headers
        .get(SECRET_HEADER)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    same_secret(given, secret.as_bytes())
}

async fn receive(State(state): State<Listener>, request: axum::extract::Request) -> StatusCode {
    // The secret is checked before the body is read: a request without it
    // is refused before its bytes take memory.
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    if !from_member(&headers, &state.secret) {
        // A little of it is read, so that a small request gets its answer
        // instead of a closed connection; a large one is cut there.
        let _ = axum::body::to_bytes(body, REFUSED_BODY).await;
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(body) = axum::body::to_bytes(body, usize::MAX).await else {
        return StatusCode::BAD_REQUEST;
    };
    let Some(length) = body.get(..4) else {
        return StatusCode::BAD_REQUEST;
    };
    let length = u32::from_le_bytes([length[0], length[1], length[2], length[3]]) as usize;
    let Some(json) = body.get(4..4 + length) else {
        return StatusCode::BAD_REQUEST;
    };
    let Ok(envelope) = serde_json::from_slice::<Envelope>(json) else {
        return StatusCode::BAD_REQUEST;
    };
    // A member that says what it runs (members before 13.6 do not).
    if let Some(info) = headers
        .get(MEMBER_HEADER)
        .and_then(|v| serde_json::from_slice::<serde_json::Value>(v.as_bytes()).ok())
        && let Some(id) = info["id"].as_u64()
        && let Ok(member) = serde_json::from_value::<grafeno_cluster::MemberInfo>(info)
    {
        state.node.note_member(id, member.clone());
        for node in state.nodes.values() {
            node.note_member(id, member.clone());
        }
    }
    let rest = &body[4 + length..];
    let snapshot = (!rest.is_empty()).then(|| Arc::new(rest.to_vec()));
    state.node.deliver(envelope, snapshot);
    StatusCode::NO_CONTENT
}

async fn status(
    State(state): State<Listener>,
    headers: HeaderMap,
) -> Result<axum::Json<grafeno_cluster::Status>, StatusCode> {
    // What the member is and whom it votes for is the cluster's business.
    if !from_member(&headers, &state.secret) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(axum::Json(state.node.status()))
}

/// Starts this member: its Raft state next to the database file (`.raft`),
/// snapshots in a folder beside it, the transport to its peers.
pub fn start(
    db: Arc<grafeno::Grafeno>,
    db_path: Option<&std::path::Path>,
    config: &ClusterConfig,
) -> Result<Arc<Node>, Box<dyn std::error::Error>> {
    start_group(db, db_path, config, 0, &config.voters)
}

/// The database file of group `group` beside the main one.
pub fn group_db_path(db_path: &std::path::Path, group: u64) -> PathBuf {
    let mut path = db_path.as_os_str().to_owned();
    path.push(format!(".group-{group}.redb"));
    PathBuf::from(path)
}

/// Starts this member of group `group` on `db` (its Raft state and
/// snapshots beside `db_path`).
pub fn start_group(
    db: Arc<grafeno::Grafeno>,
    db_path: Option<&std::path::Path>,
    config: &ClusterConfig,
    group: u64,
    voters: &[NodeId],
) -> Result<Arc<Node>, Box<dyn std::error::Error>> {
    let (storage, snapshots) = match db_path {
        Some(path) => {
            let mut raft = path.as_os_str().to_owned();
            raft.push(".raft");
            let mut snapshots = path.as_os_str().to_owned();
            snapshots.push(".snapshots");
            (
                Storage::open(&PathBuf::from(raft))?,
                PathBuf::from(snapshots),
            )
        }
        None => (
            Storage::in_memory()?,
            std::env::temp_dir().join(format!("grafeno-snapshots-{}-{group}", config.id)),
        ),
    };
    let mut options = NodeOptions::new(config.id, snapshots);
    if let Some(tick) = config.tick_ms {
        options.tick = Duration::from_millis(tick.max(1));
    }
    let transport = Arc::new(HttpTransport::for_group(config, group));
    Ok(Arc::new(Node::start(
        db, storage, transport, voters, options,
    )?))
}

/// Serves other members' messages on `[cluster] addr` until the process
/// ends.
pub async fn listen(node: Arc<Node>, config: &ClusterConfig) -> std::io::Result<()> {
    listen_groups(node, HashMap::new(), config).await
}

/// [`listen`], for group 0's member and those of more groups.
pub async fn listen_groups(
    node: Arc<Node>,
    nodes: HashMap<u64, Arc<Node>>,
    config: &ClusterConfig,
) -> std::io::Result<()> {
    if config.secret.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "[cluster] secret is empty: every member needs the same secret, and an empty one would let anyone in",
        ));
    }
    let state = Listener {
        node,
        nodes: Arc::new(nodes),
        secret: Arc::from(config.secret.as_str()),
    };
    let app = Router::new()
        .route("/raft", post(receive).layer(DefaultBodyLimit::disable()))
        .route(
            "/raft/{group}",
            post(receive_group).layer(DefaultBodyLimit::disable()),
        )
        .route("/status", get(status))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    eprintln!(
        "grafeno cluster member {} listening on {}",
        config.id,
        listener.local_addr()?
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(())
}

/// A cluster error as an engine error, for the API's handlers: a member
/// that cannot take the write answers "busy", naming the leader.
pub fn engine_error(error: grafeno_cluster::Error) -> grafeno::Error {
    use grafeno_cluster::Error;
    match error {
        Error::NotLeader(Some(leader)) => {
            grafeno::Error::Busy(format!("{NOT_LEADER_MESSAGE}: write to member {leader}"))
        }
        Error::NotLeader(None) => {
            grafeno::Error::Busy(format!("{NOT_LEADER_MESSAGE}, and none is known yet"))
        }
        Error::Lost => {
            grafeno::Error::Busy("the write was lost to a change of leader: try again".into())
        }
        Error::Timeout => {
            grafeno::Error::Busy("the cluster did not commit the write in time".into())
        }
        Error::ChangeInProgress => {
            grafeno::Error::Conflict("a membership change is under way".into())
        }
        Error::Stopped => grafeno::Error::Storage("the cluster member is stopped".into()),
        Error::NotHeld(index) => grafeno::Error::Busy(format!(
            "this member no longer holds a snapshot at position {index}"
        )),
        Error::Storage(message) => grafeno::Error::Storage(message),
        Error::Engine(error) => error,
    }
}

// ------------------------------------------------------------- routing (13.3)

/// Marks a request one member forwarded to the leader (never forwarded
/// again).
pub const FORWARDED: &str = "x-grafeno-forwarded";
/// `linearizable` (through the leader, after confirming it still leads),
/// `bounded` (this member, unless it is far behind), or `local` (the
/// default: whatever this member has applied).
pub const CONSISTENCY: &str = "x-grafeno-consistency";
/// Read your writes: wait until this member has applied this position.
pub const AFTER: &str = "x-grafeno-after";
/// On every response from a member: the position it has applied (after a
/// write, at least the write's).
pub const POSITION: &str = "x-grafeno-position";
/// On a refusal because this member is not the leader.
pub const NOT_LEADER: &str = "x-grafeno-not-leader";
/// The start of the message of that refusal (see [`engine_error`]).
/// How long to wait for a connection to another member. A member that is
/// down should fail fast (Windows takes 2 s to refuse one on its own).
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// How long a follower waits for an election before refusing a write.
const ELECTION_WAIT: Duration = Duration::from_secs(3);
pub const NOT_LEADER_MESSAGE: &str = "this member is not the cluster's leader";
/// A `bounded` read is forwarded when this member is more entries behind.
const MAX_LAG: u64 = 1_000;
/// Bodies a member buffers to forward them.
pub const MAX_FORWARDED_BODY: usize = 64 * 1024 * 1024;

pub fn refuse(
    status: axum::http::StatusCode,
    code: &str,
    message: &str,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        status,
        axum::Json(serde_json::json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// Sends a request on to the leader's API and returns its answer.
fn forward(
    api: &str,
    from: NodeId,
    method: &axum::http::Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    body: Bytes,
    secret: Option<&str>,
) -> axum::response::Response {
    send_on(
        api,
        method,
        uri,
        headers,
        body,
        (FORWARDED, from.to_string()),
        secret,
    )
}

/// Headers only members set, each other with the cluster secret: never
/// passed on from a client.
const MEMBER_HEADERS: [&str; 3] = [SECRET_HEADER, FORWARDED, crate::groups::GROUP_HOP];

/// Whether a request on the API came from another member: it carries the
/// cluster secret. A client's `x-grafeno-forwarded` alone changes nothing.
pub fn member_request(headers: &HeaderMap, secret: Option<&str>) -> bool {
    secret.is_some_and(|secret| from_member(headers, secret))
}

/// Sends a request on to another member's API, marked with `mark`, and
/// returns its answer.
pub fn send_on(
    api: &str,
    method: &axum::http::Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    body: Bytes,
    mark: (&str, String),
    secret: Option<&str>,
) -> axum::response::Response {
    let url = format!(
        "http://{api}{}",
        uri.path_and_query().map_or("/", |p| p.as_str())
    );
    let mut builder = ureq::http::Request::builder()
        .method(method.as_str())
        .uri(&url);
    // Only what the leader needs to serve it as the member would have: the
    // caller's credentials and the request's meaning, never transport
    // headers (lengths, encodings, connection) of the first hop.
    for (name, value) in headers {
        let name = name.as_str();
        let kept = matches!(
            name,
            "authorization" | "content-type" | "accept" | "idempotency-key" | "last-event-id"
        ) || (name.starts_with("x-grafeno-") && !MEMBER_HEADERS.contains(&name));
        if kept {
            builder = builder.header(name, value.as_bytes());
        }
    }
    builder = builder.header(mark.0, mark.1);
    if let Some(secret) = secret {
        builder = builder.header(SECRET_HEADER, secret);
    }
    let Ok(request) = builder.body(body.to_vec()) else {
        return refuse(
            axum::http::StatusCode::BAD_GATEWAY,
            "forward_failed",
            "cannot forward the request",
        );
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .build()
        .into();
    let mut answer = match agent.run(request) {
        Ok(answer) => answer,
        Err(error) => {
            return refuse(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                &format!("the leader did not answer: {error}"),
            );
        }
    };
    let status = axum::http::StatusCode::from_u16(answer.status().as_u16())
        .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
    let mut response = axum::response::Response::builder().status(status);
    for (name, value) in answer.headers() {
        if name == "content-type" || name == "retry-after" || name.as_str().starts_with("x-grafeno")
        {
            response = response.header(name.as_str(), value.as_bytes());
        }
    }
    let body = answer.body_mut().read_to_vec().unwrap_or_default();
    response
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| {
            refuse(
                axum::http::StatusCode::BAD_GATEWAY,
                "forward_failed",
                "bad answer",
            )
        })
}

/// The cluster's part of serving a request on a member: read-your-writes,
/// consistency levels, forwarding to the leader what only it can do, and
/// the member's position on every response.
pub async fn route(
    axum::extract::State(state): axum::extract::State<crate::http::AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    let Some(node) = state.writer.cluster().cloned() else {
        return next.run(request).await;
    };
    let headers = request.headers().clone();
    let secret = state.writer.secret().map(str::to_owned);
    // Forwarded by another member, which says so with the cluster secret;
    // a client cannot mark its own request as one, to skip the leader.
    let forwarded = headers.contains_key(FORWARDED) && member_request(&headers, secret.as_deref());
    let consistency = headers
        .get(CONSISTENCY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("local")
        .to_ascii_lowercase();
    let after: Option<u64> = headers
        .get(AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    if let Some(after) = after {
        let waiting = node.clone();
        let caught_up = tokio::task::spawn_blocking(move || {
            waiting
                .wait_for(Duration::from_secs(5), |s| s.applied >= after)
                .is_some()
        })
        .await
        .unwrap_or(false);
        if !caught_up {
            return refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                &format!("this member has not applied position {after} yet"),
            );
        }
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_FORWARDED_BODY).await else {
        return refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "the body is too large",
        );
    };
    // Inside a nested router the URI lost its prefix (`/v1`): forward the
    // one the client sent.
    let uri = parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .map_or_else(|| parts.uri.clone(), |original| original.0.clone());
    let method = parts.method.clone();
    let leader_api = |node: &Node| node.status().leader.and_then(|id| state.writer.api_of(id));
    let forward_now = |api: String, bytes: Bytes| {
        let (method, uri, headers, id) = (method.clone(), uri.clone(), headers.clone(), node.id());
        let secret = secret.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                forward(&api, id, &method, &uri, &headers, bytes, secret.as_deref())
            })
            .await
            .unwrap_or_else(|_| {
                refuse(
                    StatusCode::BAD_GATEWAY,
                    "forward_failed",
                    "the forward failed",
                )
            })
        }
    };
    let status = node.status();
    let leading = status.role == "leader";
    match consistency.as_str() {
        "linearizable" if leading => {
            let reading = node.clone();
            let confirmed = tokio::task::spawn_blocking(move || reading.read_barrier())
                .await
                .map(|outcome| outcome.is_ok())
                .unwrap_or(false);
            if !confirmed {
                return refuse(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "busy",
                    "the leader could not confirm its leadership: try again",
                );
            }
        }
        "linearizable" if !forwarded => match leader_api(&node) {
            Some(api) => return forward_now(api, bytes).await,
            None => {
                return refuse(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "busy",
                    "the cluster has no leader to read from",
                );
            }
        },
        "bounded" if !leading && !forwarded => {
            let behind =
                status.leader.is_none() || status.commit.saturating_sub(status.applied) > MAX_LAG;
            if behind && let Some(api) = leader_api(&node) {
                return forward_now(api, bytes).await;
            }
        }
        _ => {}
    }
    let request = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes.clone()));
    let mut response = next.run(request).await;
    // A write refused on a follower goes to the leader; during an
    // election, once there is one again.
    if response.status() == StatusCode::MISDIRECTED_REQUEST
        && response.headers().contains_key(NOT_LEADER)
        && !forwarded
    {
        let waiting = node.clone();
        let elected = tokio::task::spawn_blocking(move || {
            waiting.wait_for(ELECTION_WAIT, |s| s.leader.is_some())
        })
        .await
        .ok()
        .flatten();
        if let Some(api) = elected
            .and_then(|s| s.leader)
            .and_then(|id| state.writer.api_of(id))
        {
            return forward_now(api, bytes).await;
        }
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&node.status().applied.to_string()) {
        response.headers_mut().insert(POSITION, value);
    }
    response
}

// ------------------------------------------------------ split reads (13.4)

/// `on` splits a read across the members whenever its shape allows,
/// `off` never; by default (`auto`) when its estimated cost reaches the
/// cluster's `split_cost`.
pub const SPLIT: &str = "x-grafeno-split";
/// The estimated cost from which reads split by default.
pub const DEFAULT_SPLIT_COST: f64 = 200_000.0;
/// How long a member waits to reach the position of a part it is asked
/// to run.
const PART_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitMode {
    Auto,
    On,
    Off,
}

impl SplitMode {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        match headers
            .get(SPLIT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("on") => SplitMode::On,
            Some("off") => SplitMode::Off,
            _ => SplitMode::Auto,
        }
    }
}

/// What the member that splits a read needs from the request.
pub struct SplitRequest {
    pub params: serde_json::Map<String, serde_json::Value>,
    /// The caller's `Authorization`, sent on to the other members.
    pub authorization: Option<String>,
    pub mode: SplitMode,
}

/// One part of a split read, asked of a member (`POST /v1/{ns}/query/part`).
#[derive(Serialize, Deserialize)]
pub struct PartRequest {
    pub query: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    pub plan: grafeno_query::SplitPlan,
    pub part: usize,
    pub parts: usize,
    /// The log position every part reads at.
    pub position: u64,
}

/// A path segment, percent-encoded.
fn encode_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Asks the member at `api` for a part; `None` if it does not answer it.
fn remote_part(
    api: &str,
    ns: &str,
    request: &PartRequest,
    authorization: Option<&str>,
    timeout: Duration,
    secret: Option<&str>,
) -> Option<grafeno_query::Partial> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .build()
        .into();
    let url = format!("http://{api}/v1/{}/query/part", encode_segment(ns));
    let mut call = agent.post(&url).header(FORWARDED, "1");
    if let Some(secret) = secret {
        call = call.header(SECRET_HEADER, secret);
    }
    if let Some(authorization) = authorization {
        call = call.header("authorization", authorization);
    }
    let mut response = call.send_json(request).ok()?;
    if response.status().as_u16() != 200 {
        return None;
    }
    let text = response
        .body_mut()
        .with_config()
        .limit(1 << 30)
        .read_to_string()
        .ok()?;
    serde_json::from_str(&text).ok()
}

/// Runs a read on a cluster member: on its latest snapshot, or, when the
/// query can be split and it pays, in one part per member, every part at
/// this member's position, merged here. A member that does not answer
/// has its part run here instead. A split result says so:
/// `split: {parts, here}`, `here` being the parts run on this member.
#[allow(clippy::too_many_arguments)]
pub fn read(
    writer: &crate::http::Writes,
    node: &Node,
    ns: &str,
    query: &str,
    params: &grafeno_query::Params,
    limits: &grafeno_query::Limits,
    split: &SplitRequest,
) -> Result<serde_json::Value, crate::http::ApiError> {
    let (position, read) = node.snapshot().map_err(engine_error)?;
    let members = writer.members();
    let plan = match split.mode {
        SplitMode::Off => None,
        _ if members.len() < 2 => None,
        mode => grafeno_query::split_plan(&read, ns, query, params)?
            .filter(|plan| mode == SplitMode::On || plan.cost() >= writer.split_cost()),
    };
    let Some(plan) = plan else {
        return Ok(grafeno_query::execute_read_with(&read, ns, query, params, limits)?.to_json());
    };
    let parts = members.len();
    let request = |part: usize| PartRequest {
        query: query.to_owned(),
        params: split.params.clone(),
        plan: plan.clone(),
        part,
        parts,
        position,
    };
    let timeout = limits.timeout.unwrap_or(Duration::from_secs(60)) + PART_WAIT;
    let me = node.id();
    let local = |part: usize| {
        grafeno_query::execute_part(&read, ns, query, params, limits, &plan, part, parts)
    };
    let partials = std::thread::scope(|scope| {
        let remote: Vec<_> = members
            .iter()
            .enumerate()
            .map(|(part, (id, api))| {
                (*id != me).then(|| {
                    let request = request(part);
                    let authorization = split.authorization.as_deref();
                    let secret = writer.secret();
                    scope.spawn(move || {
                        remote_part(api, ns, &request, authorization, timeout, secret)
                    })
                })
            })
            .collect();
        // This member's part, while the others run theirs.
        let mut partials: Vec<Option<Result<grafeno_query::Partial, grafeno_query::QueryError>>> =
            remote.iter().map(|_| None).collect();
        let mut here = 0;
        for (part, handle) in remote.iter().enumerate() {
            if handle.is_none() {
                partials[part] = Some(local(part));
                here += 1;
            }
        }
        for (part, handle) in remote.into_iter().enumerate() {
            if let Some(handle) = handle {
                partials[part] = Some(match handle.join().ok().flatten() {
                    Some(partial) => Ok(partial),
                    None => {
                        here += 1;
                        local(part)
                    }
                });
            }
        }
        partials
            .into_iter()
            .map(|partial| partial.expect("every part ran"))
            .collect::<Result<Vec<_>, _>>()
            .map(|partials| (partials, here))
    });
    let (partials, here) = partials?;
    let result = grafeno_query::merge_parts(&read, ns, query, params, limits, &plan, partials)?;
    let mut json = result.to_json();
    json["split"] = serde_json::json!({ "parts": parts, "here": here });
    Ok(json)
}

/// Runs one part of a split read on this member's snapshot at the
/// coordinator's position.
pub fn read_part(
    node: &Node,
    ns: &str,
    request: &PartRequest,
    limits: &grafeno_query::Limits,
) -> Result<serde_json::Value, crate::http::ApiError> {
    let read = node
        .snapshot_at(request.position, PART_WAIT)
        .map_err(engine_error)?;
    let params = grafeno_query::params_from_json(&request.params);
    let partial = grafeno_query::execute_part(
        &read,
        ns,
        &request.query,
        &params,
        limits,
        &request.plan,
        request.part,
        request.parts,
    )?;
    serde_json::to_value(partial)
        .map_err(|e| crate::http::ApiError::from(grafeno::Error::Serialization(e.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append(commit: u64) -> Outgoing {
        let message = Message::Append {
            term: 1,
            prev_index: 0,
            prev_term: 0,
            entries: Vec::new(),
            commit,
            read: 0,
        };
        (
            Envelope {
                from: 1,
                to: 2,
                message,
            },
            None,
        )
    }

    fn vote_reply() -> Outgoing {
        let message = Message::VoteReply {
            term: 1,
            granted: true,
            pre_vote: false,
        };
        (
            Envelope {
                from: 1,
                to: 2,
                message,
            },
            None,
        )
    }

    fn commits(sent: &[Outgoing]) -> Vec<Option<u64>> {
        sent.iter()
            .map(|(envelope, _)| match envelope.message {
                Message::Append { commit, .. } => Some(commit),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn only_the_newest_append_goes() {
        let sent = coalesce(vec![
            append(1),
            vote_reply(),
            append(2),
            append(3),
            vote_reply(),
        ]);
        // The votes stay, in order; of the appends, the newest.
        assert_eq!(commits(&sent), [None, Some(3), None]);
        assert_eq!(commits(&coalesce(vec![append(7)])), [Some(7)]);
        assert_eq!(commits(&coalesce(vec![vote_reply()])), [None]);
    }
}
