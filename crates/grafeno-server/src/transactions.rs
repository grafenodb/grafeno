//! Explicit transactions over HTTP (16.4): `BEGIN`, statements, `COMMIT`
//! or `ROLLBACK`, across several requests.
//!
//! A reading transaction keeps one snapshot for its whole life. A writing
//! one holds the database's writer (on a cluster, the leader's write gate)
//! from its start to its end, so writes are still one at a time and its
//! isolation is serializable; other writes wait for it, which is why it is
//! rolled back when idle longer than its timeout, and when older than the
//! longest life allowed. On a cluster its statements run on the leader in
//! a transaction that captures what they do, and its `COMMIT` replicates
//! all of it as one entry.
//!
//! A statement that fails inside a writing transaction may have done part
//! of its work, which cannot be taken back alone: the transaction is then
//! failed, and only `ROLLBACK` (or a `COMMIT`, which rolls it back) ends it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grafeno::{ReadTx, Timestamp, WriteTx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth;
use crate::http::{ApiError, ApiResult, AppState, Checked, check_statement};

/// How long transactions may live and wait (`[transactions]` in
/// `grafeno.toml`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionSettings {
    /// Rolled back after this long without a request.
    pub idle: Duration,
    /// Rolled back at this age, busy or not.
    pub lifetime: Duration,
    /// How long `BEGIN` of a writing transaction waits for the one before.
    pub wait: Duration,
    /// Transactions open at once at most.
    pub max_open: usize,
}

impl Default for TransactionSettings {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(30),
            lifetime: Duration::from_secs(600),
            wait: Duration::from_secs(5),
            max_open: 256,
        }
    }
}

enum Work {
    Read(Arc<ReadTx>),
    Write {
        tx: WriteTx,
        _permit: tokio::sync::OwnedSemaphorePermit,
    },
    Cluster(grafeno_cluster::Transaction),
}

struct Session {
    id: String,
    owner: String,
    /// The cluster group it runs in: its statements must name namespaces
    /// of that group.
    group: u64,
    read_only: bool,
    started: Instant,
    started_at: Timestamp,
    timeout: Duration,
    last: Mutex<Instant>,
    /// `None` once ended. The reason of a failure, when a statement failed.
    work: Mutex<(Option<Work>, Option<String>)>,
    statements: Mutex<u64>,
}

impl Session {
    fn expired(&self, settings: &TransactionSettings) -> Option<&'static str> {
        let idle = self
            .last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed();
        if idle > self.timeout {
            Some("it was idle longer than its timeout")
        } else if self.started.elapsed() > settings.lifetime {
            Some("it lived longer than transactions may")
        } else {
            None
        }
    }

    fn describe(&self) -> Value {
        let failed = self
            .work
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone();
        json!({
            "id": self.id,
            "read_only": self.read_only,
            "started_at": self.started_at,
            "timeout_secs": self.timeout.as_secs_f64(),
            "statements": *self.statements.lock().unwrap_or_else(|e| e.into_inner()),
            "failed": failed,
        })
    }
}

/// The open transactions of a server.
pub struct Transactions {
    settings: TransactionSettings,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// One writing transaction at a time on a single server.
    writer: Arc<tokio::sync::Semaphore>,
}

impl Transactions {
    pub fn new(settings: TransactionSettings) -> Self {
        Self {
            settings,
            sessions: Mutex::default(),
            writer: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    pub fn settings(&self) -> TransactionSettings {
        self.settings
    }

    /// Open transactions: (reading, writing).
    pub fn open(&self) -> (usize, usize) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let writing = sessions.values().filter(|s| !s.read_only).count();
        (sessions.len() - writing, writing)
    }

    /// Rolls back the transactions past their time.
    pub fn reap(&self) {
        let expired: Vec<Arc<Session>> = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let ids: Vec<String> = sessions
                .values()
                .filter(|s| s.expired(&self.settings).is_some())
                .map(|s| s.id.clone())
                .collect();
            ids.iter().filter_map(|id| sessions.remove(id)).collect()
        };
        for session in expired {
            let _ = end(&session, false);
        }
    }

    fn take(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
    }

    /// The caller's transaction `id`, refreshed; an expired one is rolled
    /// back and reported as gone. `group`: the cluster group the request
    /// reached.
    fn get(&self, id: &str, group: u64) -> ApiResult<Arc<Session>> {
        self.reap();
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| {
                ApiError::not_found_message(format!(
                    "no transaction {id:?}: it ended, timed out, or never began"
                ))
            })?;
        if session.owner != auth::current().name() {
            return Err(ApiError::forbidden_message(
                "this transaction belongs to another caller",
            ));
        }
        if session.group != group {
            return Err(ApiError::bad_request(format!(
                "this transaction runs in cluster group {}: its statements must name namespaces of that group",
                session.group
            )));
        }
        *session.last.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        Ok(session)
    }
}

/// Ends a session: commits it, or rolls it back.
fn end(session: &Session, commit: bool) -> Result<(), String> {
    let (work, failed) = {
        let mut guard = session.work.lock().unwrap_or_else(|e| e.into_inner());
        (guard.0.take(), guard.1.clone())
    };
    let Some(work) = work else {
        return Err("the transaction already ended".into());
    };
    if commit && let Some(reason) = failed {
        rollback(work);
        return Err(format!(
            "the transaction was rolled back, as a statement in it failed: {reason}"
        ));
    }
    if !commit {
        rollback(work);
        return Ok(());
    }
    match work {
        Work::Read(_) => Ok(()),
        Work::Write { tx, _permit } => tx.commit().map_err(|e| e.to_string()),
        Work::Cluster(_) => unreachable!("cluster transactions commit through the node"),
    }
}

fn rollback(work: Work) {
    match work {
        Work::Read(_) => {}
        Work::Write { tx, _permit } => {
            let _ = tx.abort();
        }
        Work::Cluster(transaction) => {
            let _ = transaction.rollback();
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginInput {
    /// A reading transaction: one snapshot, no writes.
    #[serde(default)]
    pub read_only: bool,
    /// Seconds without a request before it is rolled back (at most the
    /// server's own timeout).
    #[serde(default)]
    pub timeout_secs: Option<f64>,
}

/// `POST /v1/transactions`: begins a transaction.
pub async fn begin(state: &AppState, input: BeginInput) -> ApiResult<Value> {
    let transactions = state.transactions.clone();
    let settings = transactions.settings;
    transactions.reap();
    if transactions
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
        >= settings.max_open
    {
        return Err(ApiError::with_status(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            format!(
                "{} transactions are open, the most allowed",
                settings.max_open
            ),
        ));
    }
    let timeout = input
        .timeout_secs
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map_or(settings.idle, Duration::from_secs_f64)
        .min(settings.idle);
    let work = if input.read_only {
        let db = state.db.clone();
        let read = tokio::task::spawn_blocking(move || db.begin_read())
            .await
            .map_err(|e| ApiError::internal(e.to_string()))??;
        Work::Read(Arc::new(read))
    } else if let Some(node) = state.writer.cluster().cloned() {
        let wait = settings.wait;
        let transaction = tokio::task::spawn_blocking(move || node.begin_transaction(wait))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|error| match error {
                grafeno_cluster::Error::Timeout => busy(wait),
                other => ApiError::from(crate::cluster::engine_error(other)),
            })?;
        Work::Cluster(transaction)
    } else {
        let permit =
            tokio::time::timeout(settings.wait, transactions.writer.clone().acquire_owned())
                .await
                .map_err(|_| busy(settings.wait))?
                .map_err(|e| ApiError::internal(e.to_string()))?;
        let db = state.db.clone();
        let tx = tokio::task::spawn_blocking(move || db.begin_write())
            .await
            .map_err(|e| ApiError::internal(e.to_string()))??;
        Work::Write {
            tx,
            _permit: permit,
        }
    };
    let session = Arc::new(Session {
        id: grafeno::NodeId::new().to_string(),
        owner: auth::current().name().to_owned(),
        group: state.group,
        read_only: input.read_only,
        started: Instant::now(),
        started_at: Timestamp::now(),
        timeout,
        last: Mutex::new(Instant::now()),
        work: Mutex::new((Some(work), None)),
        statements: Mutex::new(0),
    });
    let described = session.describe();
    transactions
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session.id.clone(), session);
    Ok(described)
}

fn busy(wait: Duration) -> ApiError {
    ApiError::with_status(
        axum::http::StatusCode::CONFLICT,
        "transaction_busy",
        format!(
            "another writing transaction held the writer for {:.1} s: try again",
            wait.as_secs_f64()
        ),
    )
}

/// `GET /v1/transactions/{id}`.
pub fn describe(state: &AppState, id: &str) -> ApiResult<Value> {
    Ok(state.transactions.get(id, state.group)?.describe())
}

/// `POST /v1/{ns}/transactions/{id}/query`: runs a statement in it.
pub async fn query(
    state: &AppState,
    id: &str,
    ns: String,
    query: String,
    params: grafeno_query::Params,
) -> ApiResult<Value> {
    let session = state.transactions.get(id, state.group)?;
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let Checked { limits, kind, .. } = check_statement(state, &ns, &query, cancel)?;
    if kind == grafeno_query::StatementKind::Access {
        return Err(ApiError::bad_request(
            "users, roles and grants are managed outside explicit transactions",
        ));
    }
    if session.read_only && grafeno_query::is_write(&query)? {
        return Err(ApiError::bad_request(
            "this transaction only reads (it began with read_only)",
        ));
    }
    let actor = auth::current().actor();
    let origin = crate::audit::origin();
    let db = state.db.clone();
    let session2 = session.clone();
    let outcome = tokio::task::spawn_blocking(move || -> ApiResult<Value> {
        let mut guard = session2.work.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(reason) = &guard.1 {
            return Err(ApiError::bad_request(format!(
                "a statement in this transaction failed ({reason}): only ROLLBACK can end it"
            )));
        }
        let failed;
        let result = match guard.0.as_mut() {
            None => return Err(ApiError::bad_request("the transaction already ended")),
            Some(Work::Read(read)) => {
                return grafeno_query::execute_read_with(read, &ns, &query, &params, &limits)
                    .map(|r| r.to_json())
                    .map_err(ApiError::from);
            }
            Some(Work::Write { tx, .. }) => {
                let read = db.begin_read()?;
                tx.act_as(actor);
                tx.set_origin(origin);
                let result = grafeno_query::execute_in(tx, &read, &ns, &query, &params, &limits);
                tx.act_as(None);
                failed = result.as_ref().err().map(|e| e.to_string());
                result
            }
            Some(Work::Cluster(transaction)) => {
                let read = db.begin_read()?;
                let tx = transaction.tx();
                tx.act_as(actor);
                tx.set_origin(origin);
                let result = grafeno_query::execute_in(tx, &read, &ns, &query, &params, &limits);
                tx.act_as(None);
                failed = result.as_ref().err().map(|e| e.to_string());
                result
            }
        };
        // A statement that failed may have written part of its work.
        if let Some(reason) = failed {
            guard.1 = Some(reason);
        }
        result.map(|r| r.to_json()).map_err(ApiError::from)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    *session.statements.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    outcome
}

/// `POST /v1/transactions/{id}/commit`.
pub async fn commit(state: &AppState, id: &str) -> ApiResult<Value> {
    let session = state.transactions.get(id, state.group)?;
    let session = state.transactions.take(&session.id).unwrap_or(session);
    let node = state.writer.cluster().cloned();
    let outcome = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let is_cluster = matches!(
            session.work.lock().unwrap_or_else(|e| e.into_inner()).0,
            Some(Work::Cluster(_))
        );
        if !is_cluster {
            return end(&session, true);
        }
        let (work, failed) = {
            let mut guard = session.work.lock().unwrap_or_else(|e| e.into_inner());
            (guard.0.take(), guard.1.clone())
        };
        let Some(Work::Cluster(transaction)) = work else {
            return Err("the transaction already ended".into());
        };
        if let Some(reason) = failed {
            let _ = transaction.rollback();
            return Err(format!(
                "the transaction was rolled back, as a statement in it failed: {reason}"
            ));
        }
        let node = node.ok_or("this member is no longer in a cluster")?;
        node.commit_transaction(transaction).map_err(|error| {
            format!("the transaction did not commit, and nothing of it happened: {error}")
        })
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    match outcome {
        Ok(()) => Ok(json!({ "id": id, "committed": true })),
        Err(message) => Err(ApiError::with_status(
            axum::http::StatusCode::CONFLICT,
            "rolled_back",
            message,
        )),
    }
}

/// `POST /v1/transactions/{id}/rollback`.
pub async fn rollback_route(state: &AppState, id: &str) -> ApiResult<Value> {
    let session = state.transactions.get(id, state.group)?;
    let session = state.transactions.take(&session.id).unwrap_or(session);
    tokio::task::spawn_blocking(move || end(&session, false))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::bad_request)?;
    Ok(json!({ "id": id, "rolled_back": true }))
}
