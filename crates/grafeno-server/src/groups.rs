//! Several Raft groups on one cluster (phase 13.5).
//!
//! Group 0 is the cluster of 13.2: every member holds it, with the
//! database file `db`. More groups (`groups` in `[cluster]`) each have
//! their own voters, database file (`<db>.group-<id>.redb`) and Raft state,
//! so each has its own leader and its own writes: write throughput grows
//! with the groups. Every namespace belongs to one group; group 0 keeps
//! the directory (state `cluster`/`placement`, replicated as any write), and
//! a namespace not in it belongs to group 0.
//!
//! Any member takes any request: [`dispatch`] looks the namespace up and
//! hands the request to the group's own router (every route, layer and
//! handler as on a single group), or, when this member does not hold that
//! group, to a member that does.
//!
//! Moving a namespace ([`move_namespace`], `POST /v1/cluster/placement`)
//! does not stop it: its records and change log are copied from a
//! snapshot of the source group into the target (as the target's own
//! writes), then the source's log entries after that snapshot, filtered to
//! the namespace, until little is left; then a fence written in the
//! source's own log makes every later write to the namespace there fail
//! as busy (writes are serialized on a leader, so every write before the
//! fence is in the entries before it), the last entries are copied, the
//! directory switches, and the source drops what it held (keeping the
//! fence, so a late write is refused rather than lost). Clients retry the
//! busy writes of the pause, and the next try goes to the target.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use axum::{Json, Router};
use grafeno::{Grafeno, Op};
use grafeno_cluster::Node;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::{ApiError, ApiResult, AppState, Body};

/// The namespace and state kind of the directory, in group 0.
pub const DIRECTORY: (&str, &str) = ("cluster", "placement");
/// Marks a request one member handed to another that holds its group.
pub const GROUP_HOP: &str = "x-grafeno-group-hop";
/// Effects per write when copying a namespace.
const CHUNK: usize = 2_000;
/// Catch-up rounds stop when one copies fewer entries than this.
const SETTLED: usize = 32;
/// How long the source keeps a moved namespace before dropping it, so that
/// members still routing by the old directory find it there.
const DROP_AFTER: Duration = Duration::from_secs(5);

/// One more group: its id (1 and up) and voters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupConfig {
    pub id: u64,
    pub voters: Vec<u64>,
    /// This member joins the group (a member restored from a backup): it
    /// starts with no voters and waits for the leader to add it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub join: bool,
}

/// Where a namespace lives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub group: u64,
    /// A move under way, to this group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moving_to: Option<u64>,
}

/// The groups of this member: the states of those it holds, and every
/// group's voters.
pub struct Registry {
    pub states: BTreeMap<u64, AppState>,
    pub routers: BTreeMap<u64, Router>,
    pub voters: BTreeMap<u64, Vec<u64>>,
    /// Every member's API address.
    pub apis: BTreeMap<u64, String>,
    pub me: u64,
}

/// Shared by every group's state; set once all groups are up.
pub type Slot = Arc<OnceLock<Arc<Registry>>>;

impl Registry {
    fn db0(&self) -> &Arc<Grafeno> {
        &self.states[&0].db
    }

    /// Where `ns` lives (group 0 unless the directory says otherwise).
    pub fn placement(&self, ns: &str) -> Placement {
        placement_in(self.db0(), ns)
    }

    /// A member holding `group` other than this one, to hand requests to.
    fn host_of(&self, group: u64) -> Option<String> {
        self.voters
            .get(&group)?
            .iter()
            .filter(|id| **id != self.me)
            .find_map(|id| self.apis.get(id).cloned())
    }
}

/// The directory's entry for `ns` in group 0's database.
pub fn placement_in(db0: &Grafeno, ns: &str) -> Placement {
    db0.begin_read()
        .ok()
        .and_then(|read| read.state(DIRECTORY.0, DIRECTORY.1, ns).ok().flatten())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Every namespace the directory places, with its placement.
pub fn placements(db0: &Grafeno) -> grafeno::Result<Vec<(String, Placement)>> {
    Ok(db0
        .begin_read()?
        .state_entries(DIRECTORY.0, DIRECTORY.1, "", usize::MAX)?
        .into_iter()
        .filter_map(|(ns, bytes)| Some((ns, serde_json::from_slice(&bytes).ok()?)))
        .collect())
}

/// The group a request under `/v1` is for: `cluster/groups/{g}/…` and
/// `backup?group={g}` name it, a namespace's routes go to the namespace's
/// group, the rest to group 0.
fn target(registry: &Registry, uri: &axum::http::Uri) -> u64 {
    let Some(rest) = uri.path().strip_prefix("/v1/") else {
        return 0;
    };
    // A backup of another group's file: `/v1/backup?group=N`.
    if rest == "backup"
        && let Some(group) = uri
            .query()
            .unwrap_or("")
            .split('&')
            .find_map(|pair| pair.strip_prefix("group="))
    {
        return group.parse().unwrap_or(0);
    }
    let parts: Vec<&str> = rest.split('/').collect();
    let decoded = |segment: &str| crate::auth::decode_segment(segment);
    match parts.as_slice() {
        ["cluster", "groups", group, _, ..] => group.parse().unwrap_or(0),
        ["namespaces", ns, ..] => registry.placement(&decoded(ns)).group,
        [ns, _, ..] if !grafeno::RESERVED.contains(&decoded(ns).as_str()) => {
            registry.placement(&decoded(ns)).group
        }
        _ => 0,
    }
}

/// Hands a request to the router of the group it is for (see the module
/// documentation).
pub async fn dispatch(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(registry) = state.groups.get().cloned() else {
        return next.run(request).await;
    };
    let group = target(&registry, request.uri());
    if group == state.group {
        return next.run(request).await;
    }
    if let Some(router) = registry.routers.get(&group) {
        use tower::ServiceExt;
        // A fresh request: what routing here put in its extensions (the
        // path's parameters) must not reach the other router.
        let (parts, body) = request.into_parts();
        let uri = parts
            .extensions
            .get::<axum::extract::OriginalUri>()
            .map_or_else(|| parts.uri.clone(), |original| original.0.clone());
        let mut fresh = Request::new(body);
        *fresh.method_mut() = parts.method;
        *fresh.uri_mut() = uri;
        *fresh.version_mut() = parts.version;
        *fresh.headers_mut() = parts.headers;
        return match router.clone().oneshot(fresh).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
    }
    // Not held here: to a member that holds it, once.
    let refuse = |message: String| {
        crate::cluster::refuse(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            &message,
        )
    };
    if request.headers().contains_key(GROUP_HOP) {
        return refuse(format!("this member does not hold group {group}"));
    }
    let Some(api) = registry.host_of(group) else {
        return refuse(format!("no member holds group {group}"));
    };
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, crate::cluster::MAX_FORWARDED_BODY).await else {
        return refuse("the body is too large".into());
    };
    let uri = parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .map_or_else(|| parts.uri.clone(), |original| original.0.clone());
    let me = registry.me;
    let secret = state.writer.secret().map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        crate::cluster::send_on(
            &api,
            &parts.method,
            &uri,
            &parts.headers,
            bytes,
            (GROUP_HOP, me.to_string()),
            secret.as_deref(),
        )
    })
    .await
    .unwrap_or_else(|_| refuse("the hand-over failed".into()))
}

// ------------------------------------------------------------ handlers

/// `GET /v1/cluster/groups`: every group, its voters, and this member's
/// view of those it holds.
pub async fn groups(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let Some(registry) = state.groups.get().cloned() else {
        return Ok(Json(json!([])));
    };
    let list: Vec<Value> = registry
        .voters
        .iter()
        .map(|(id, voters)| {
            let status = registry
                .states
                .get(id)
                .and_then(|state| state.writer.cluster())
                .map(|node| node.status());
            json!({ "id": id, "voters": voters, "held": status.is_some(), "status": status })
        })
        .collect();
    Ok(Json(Value::Array(list)))
}

/// `GET /v1/cluster/placement`: the namespaces the directory places.
pub async fn placement(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db.clone();
    let list = tokio::task::spawn_blocking(move || placements(&db))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
    Ok(Json(json!(
        list.into_iter()
            .map(|(ns, p)| json!({ "namespace": ns, "group": p.group, "moving_to": p.moving_to }))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveInput {
    namespace: String,
    group: u64,
}

/// `POST /v1/cluster/placement`: moves a namespace to a group (on group
/// 0's leader, where a follower forwards it).
pub async fn place(
    State(state): State<AppState>,
    headers: HeaderMap,
    Body(input): Body<MoveInput>,
) -> ApiResult<Json<Value>> {
    let Some(registry) = state.groups.get().cloned() else {
        return Err(ApiError::bad_request("this server has one group"));
    };
    if !state.leading() {
        return Err(not_leader());
    }
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        move_namespace(
            &registry,
            &input.namespace,
            input.group,
            authorization.as_deref(),
        )
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map(Json)
}

fn not_leader() -> ApiError {
    ApiError::from(grafeno::Error::Busy(format!(
        "{}: the cluster's group 0 leader moves namespaces",
        crate::cluster::NOT_LEADER_MESSAGE
    )))
}

#[derive(Serialize, Deserialize)]
pub struct ImportInput {
    pub ops: Vec<Op>,
    #[serde(default)]
    pub changes: Vec<(String, grafeno::Change)>,
    #[serde(default)]
    pub at: Option<grafeno::Timestamp>,
}

/// `POST /v1/cluster/groups/{g}/import`: writes effects copied from
/// another group (dispatched to group `g`, forwarded to its leader).
pub async fn import(
    State(state): State<AppState>,
    Body(input): Body<ImportInput>,
) -> ApiResult<Json<Value>> {
    let input = Arc::new(input);
    let writer = state.writer.clone();
    tokio::task::spawn_blocking(move || {
        writer.write(move |tx| tx.import_ops(&input.ops, input.changes.clone(), input.at))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(ApiError::from)?;
    Ok(Json(json!({ "position": applied(&state) })))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FenceInput {
    pub namespace: String,
    pub to: u64,
    /// `fence`, `unfence` or `drop`.
    pub action: String,
}

/// `POST /v1/cluster/groups/{g}/fence`: fences a namespace for a move,
/// lifts the fence, or drops what a moved namespace left (in group `g`).
pub async fn fence(
    State(state): State<AppState>,
    Body(input): Body<FenceInput>,
) -> ApiResult<Json<Value>> {
    let writer = state.writer.clone();
    let input = Arc::new(input);
    tokio::task::spawn_blocking(move || {
        writer.write(move |tx| {
            let to = input.to.to_string();
            match input.action.as_str() {
                "fence" => tx.fence_namespace(&input.namespace, &to),
                "unfence" => tx.unfence_namespace(&input.namespace),
                "drop" => tx.drop_moved_namespace(&input.namespace, &to),
                other => Err(grafeno::Error::Invalid(format!("no action {other:?}"))),
            }
        })
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(ApiError::from)?;
    Ok(Json(json!({ "position": applied(&state) })))
}

fn applied(state: &AppState) -> u64 {
    state
        .writer
        .cluster()
        .map_or(0, |node| node.status().applied)
}

// ----------------------------------------------------------------- moving

/// Calls this member's own API (as the caller), so that writes reach each
/// group's leader wherever it is.
struct Caller<'a> {
    agent: ureq::Agent,
    base: String,
    authorization: Option<&'a str>,
}

impl Caller<'_> {
    /// Every write of a move is idempotent (final values, change log
    /// entries with their numbers, the fence, the directory), so one that
    /// meets a change of leader or a busy member is sent again, for up to
    /// a minute.
    fn post(&self, path: &str, body: &impl Serialize) -> Result<Value, ApiError> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match self.post_once(path, body) {
                Err((status, _))
                    if matches!(status, 0 | 421 | 503) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(200));
                }
                outcome => return outcome.map_err(|(_, error)| error),
            }
        }
    }

    fn post_once(&self, path: &str, body: &impl Serialize) -> Result<Value, (u16, ApiError)> {
        let mut request = self.agent.post(&format!("{}{path}", self.base));
        if let Some(authorization) = self.authorization {
            request = request.header("authorization", authorization);
        }
        let mut response = request.send_json(body).map_err(|e| {
            (
                0,
                ApiError::from(grafeno::Error::Busy(format!("{path}: {e}"))),
            )
        })?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status != 200 {
            return Err((
                status,
                ApiError::from(grafeno::Error::Busy(format!(
                    "{path} answered {status}: {}",
                    value["error"]["message"].as_str().unwrap_or(&text)
                ))),
            ));
        }
        Ok(value)
    }

    fn import(
        &self,
        group: u64,
        ops: Vec<Op>,
        changes: Vec<(String, grafeno::Change)>,
        at: Option<grafeno::Timestamp>,
    ) -> Result<(), ApiError> {
        self.post(
            &format!("/v1/cluster/groups/{group}/import"),
            &ImportInput { ops, changes, at },
        )
        .map(|_| ())
    }

    fn fence(&self, group: u64, ns: &str, to: u64, action: &str) -> Result<u64, ApiError> {
        let answer = self.post(
            &format!("/v1/cluster/groups/{group}/fence"),
            &FenceInput {
                namespace: ns.to_owned(),
                to,
                action: action.to_owned(),
            },
        )?;
        Ok(answer["position"].as_u64().unwrap_or(0))
    }

    fn place(&self, ns: &str, placement: Placement) -> Result<(), ApiError> {
        self.post(
            "/v1/cluster/groups/0/directory",
            &json!({ "namespace": ns, "placement": placement }),
        )
        .map(|_| ())
    }
}

/// `POST /v1/cluster/groups/0/directory`: sets a namespace's placement
/// (internal, on group 0).
pub async fn set_directory(
    State(state): State<AppState>,
    Body(input): Body<Value>,
) -> ApiResult<Json<Value>> {
    let ns = input["namespace"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("namespace?"))?
        .to_owned();
    let placement: Placement = serde_json::from_value(input["placement"].clone())
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let bytes = serde_json::to_vec(&placement).map_err(|e| ApiError::internal(e.to_string()))?;
    let writer = state.writer.clone();
    tokio::task::spawn_blocking(move || {
        writer.write(move |tx| tx.put_state(DIRECTORY.0, DIRECTORY.1, &ns, &bytes))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(ApiError::from)?;
    Ok(Json(json!({ "position": applied(&state) })))
}

/// How far a copy has come: the source's log position and the namespace's
/// last change log sequence number copied.
#[derive(Clone, Copy)]
struct Copied {
    position: u64,
    seq: u64,
}

/// Copies into group `to` what the source's entries after `from` did to
/// `ns` (once the source has applied `at_least`), in writes of about
/// [`CHUNK`] effects, and the namespace's change log entries after the
/// last copied, as they are (sequence numbers and clocks); returns how far
/// it came and how many entries touched the namespace.
fn catch_up(
    caller: &Caller,
    source: &Node,
    ns: &str,
    to: u64,
    from: Copied,
    at_least: u64,
) -> Result<(Copied, usize), ApiError> {
    let engine = |e| ApiError::from(crate::cluster::engine_error(e));
    source
        .wait_for(Duration::from_secs(10), |s| s.applied >= at_least)
        .ok_or_else(|| engine(grafeno_cluster::Error::Timeout))?;
    let (index, read) = source.snapshot().map_err(engine)?;
    let position = read.log_position()?;
    if position.first > from.position + 1 && from.position < position.last {
        return Err(ApiError::from(grafeno::Error::Busy(format!(
            "the source group's log no longer holds entry {}: try the move again",
            from.position + 1
        ))));
    }
    let mut batch: Vec<Op> = Vec::new();
    let mut touched = 0;
    let mut reached = from.position;
    loop {
        let entries = read.log_entries(reached, 256)?;
        if entries.is_empty() {
            break;
        }
        for (at, entry) in entries {
            reached = at;
            // The move's fence stays in the source.
            let ops: Vec<Op> = entry
                .ops_of(ns)
                .into_iter()
                .filter(|op| {
                    !matches!(op, Op::State { kind, key, .. }
                        if (kind.as_str(), key.as_str()) == grafeno::FENCE)
                })
                .collect();
            if ops.is_empty() {
                continue;
            }
            touched += 1;
            batch.extend(ops);
            if batch.len() >= CHUNK {
                caller.import(to, std::mem::take(&mut batch), Vec::new(), None)?;
            }
        }
    }
    let page = read.changes(ns, from.seq, usize::MAX)?;
    let mut seq = from.seq;
    for change in page.changes {
        seq = seq.max(change.seq);
        batch.push(Op::Change {
            ns: ns.to_owned(),
            seq: change.seq,
            change: Some(change),
        });
    }
    if !batch.is_empty() {
        caller.import(to, batch, Vec::new(), None)?;
    }
    Ok((
        Copied {
            position: reached.max(index),
            seq,
        },
        touched,
    ))
}

/// Moves `ns` to group `to` without stopping it (see the module
/// documentation). Runs on group 0's leader, which must hold the source
/// group.
pub fn move_namespace(
    registry: &Registry,
    ns: &str,
    to: u64,
    authorization: Option<&str>,
) -> Result<Value, ApiError> {
    let started = Instant::now();
    if !registry.voters.contains_key(&to) {
        return Err(ApiError::bad_request(format!("there is no group {to}")));
    }
    let current = registry.placement(ns);
    let from = current.group;
    if from == to {
        return Ok(json!({ "namespace": ns, "from": from, "to": to, "moved": false }));
    }
    let Some(source) = registry
        .states
        .get(&from)
        .and_then(|state| state.writer.cluster().cloned())
    else {
        return Err(ApiError::bad_request(format!(
            "this member (group 0's leader) does not hold group {from}, where {ns:?} is"
        )));
    };
    let Some(api) = registry.apis.get(&registry.me) else {
        return Err(ApiError::bad_request(
            "this member has no `api` address in `peers`",
        ));
    };
    let caller = Caller {
        agent: ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(300)))
            .build()
            .into(),
        base: format!("http://{api}"),
        authorization,
    };
    caller.place(
        ns,
        Placement {
            group: from,
            moving_to: Some(to),
        },
    )?;
    let mut fenced = false;
    let outcome = (|| -> Result<Value, ApiError> {
        // Whatever an earlier attempt left in the target goes first.
        caller.import(
            to,
            vec![Op::Bulk(grafeno::Bulk::DropNamespace { ns: ns.to_owned() })],
            Vec::new(),
            None,
        )?;
        let (copied_at, read) = source
            .snapshot()
            .map_err(|e| ApiError::from(crate::cluster::engine_error(e)))?;
        let mut records = 0usize;
        let mut failure = None;
        read.namespace_ops(ns, CHUNK, |ops| {
            records += ops.len();
            if let Err(error) = caller.import(to, ops, Vec::new(), None) {
                failure = Some(error);
                return Err(grafeno::Error::Busy("the copy failed".into()));
            }
            Ok(())
        })
        .map_err(|error| failure.take().unwrap_or_else(|| ApiError::from(error)))?;
        let mut copied = Copied {
            position: copied_at,
            seq: read.changes(ns, 0, 0)?.last,
        };
        drop(read);
        // Catch up while writes go on, until a round finds little.
        let mut rounds = 0;
        loop {
            let (reached, touched) = catch_up(&caller, &source, ns, to, copied, 0)?;
            copied = reached;
            rounds += 1;
            if touched < SETTLED || rounds >= 20 {
                break;
            }
        }
        // The pause: fence the source, copy what came before the fence.
        let paused = Instant::now();
        let fence_at = caller.fence(from, ns, to, "fence")?;
        fenced = true;
        let (_, last) = catch_up(&caller, &source, ns, to, copied, fence_at)?;
        caller.place(
            ns,
            Placement {
                group: to,
                moving_to: None,
            },
        )?;
        let paused_ms = paused.elapsed().as_secs_f64() * 1000.0;
        Ok(json!({
            "namespace": ns,
            "from": from,
            "to": to,
            "moved": true,
            "records": records,
            "catch_up_rounds": rounds,
            "entries_in_pause": last,
            "paused_ms": paused_ms,
            "ms": started.elapsed().as_secs_f64() * 1000.0,
        }))
    })();
    match outcome {
        Ok(value) => {
            // Members still routing by the old directory find it a while.
            let ns = ns.to_owned();
            let caller = Caller {
                agent: caller.agent.clone(),
                base: caller.base.clone(),
                authorization: None,
            };
            let authorization = authorization.map(str::to_owned);
            std::thread::spawn(move || {
                std::thread::sleep(DROP_AFTER);
                let caller = Caller {
                    authorization: authorization.as_deref(),
                    ..caller
                };
                let _ = caller.fence(from, &ns, to, "drop");
            });
            Ok(value)
        }
        Err(error) => {
            if fenced {
                let _ = caller.fence(from, ns, to, "unfence");
            }
            let _ = caller.place(ns, current);
            Err(error)
        }
    }
}

/// The namespaces of every group this member holds, each from the group
/// the directory places it in.
pub fn namespaces(registry: &Registry) -> grafeno::Result<Vec<grafeno::NamespaceInfo>> {
    let mut all = Vec::new();
    for (group, state) in &registry.states {
        for info in state.db.begin_read()?.namespaces()? {
            if info.name == DIRECTORY.0 && *group == 0 {
                continue;
            }
            if registry.placement(&info.name).group == *group {
                all.push(info);
            }
        }
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(all)
}
