//! Following the writes: the change log as a stream (server-sent events),
//! webhooks that push it, streaming ingestion of NDJSON with idempotency
//! keys, and the background work that runs from the log (extraction and
//! community summaries).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grafeno::{Change, ChangePage, Grafeno, NodeId, ReadTx, Timestamp, WriteTx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::api::{EdgeInput, NodeInput};
use crate::http::AppState;

// ------------------------------------------------------------- commits

/// Every commit that appended changes bumps a counter that streams and
/// background work wait on.
pub struct Events {
    commits: watch::Sender<u64>,
}

impl Events {
    pub fn attach(db: &Grafeno) -> Arc<Self> {
        let events = Arc::new(Self {
            commits: watch::Sender::new(0),
        });
        let notify = events.clone();
        db.on_commit(move |_| notify.commits.send_modify(|n| *n += 1));
        events
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.commits.subscribe()
    }
}

// ------------------------------------------------------------- the stream

/// The state of one server-sent events stream of a namespace's changes.
pub struct Feed {
    pub db: Arc<Grafeno>,
    pub ns: String,
    pub cursor: u64,
    pub commits: watch::Receiver<u64>,
    pub queue: VecDeque<axum::response::sse::Event>,
    pub gap_told: bool,
    /// What the subscriber may see (14.4): changes of hidden records are
    /// left out.
    pub view: Option<Arc<grafeno::View>>,
    /// The events of this subscription, with their values (16.10),
    /// instead of the whole change log.
    pub subscription: Option<String>,
}

impl Feed {
    /// The next event: what is queued, else the next page of the log, else
    /// waits for a commit. `None` when the server shuts down.
    pub async fn next(mut self) -> Option<(axum::response::sse::Event, Self)> {
        use axum::response::sse::Event;
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some((event, self));
            }
            // Marks what this read sees, so a commit after it wakes us.
            self.commits.borrow_and_update();
            let (db, ns, since) = (self.db.clone(), self.ns.clone(), self.cursor);
            let view = self.view.clone();
            let subscription = self.subscription.clone();
            let page = tokio::task::spawn_blocking(move || -> grafeno::Result<ChangePage> {
                let read = db.begin_read()?.with_view(view);
                match subscription {
                    Some(name) => read.events(&ns, &name, since, 500),
                    None => read.changes(&ns, since, 500),
                }
            })
            .await
            .ok()?
            .ok()?;
            if page.gap && !self.gap_told {
                self.gap_told = true;
                let data = json!({ "oldest": page.oldest, "last": page.last });
                self.queue
                    .push_back(Event::default().event("gap").data(data.to_string()));
            }
            // Past hidden changes too, so they are not read again.
            self.cursor = page.next(self.cursor);
            let kind = if self.subscription.is_some() {
                "event"
            } else {
                "change"
            };
            for change in page.changes {
                let data = serde_json::to_string(&change).unwrap_or_default();
                self.queue.push_back(
                    Event::default()
                        .id(change.seq.to_string())
                        .event(kind)
                        .data(data),
                );
            }
            if self.queue.is_empty() && self.commits.changed().await.is_err() {
                return None;
            }
        }
    }
}

// ------------------------------------------------------------- webhooks

/// State kind of webhooks, keyed by id.
pub const WEBHOOK: &str = "webhook";

/// A URL that receives the namespace's changes, in order, in batches.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Webhook {
    pub id: String,
    pub url: String,
    /// Signs each delivery (`X-Grafeno-Signature: sha256=<HMAC>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Only changes of nodes or edges with one of these labels (all when
    /// empty); the others are skipped.
    #[serde(default)]
    pub labels: Vec<String>,
    /// The last change delivered (or skipped).
    pub cursor: u64,
    pub created_at: Timestamp,
    #[serde(default)]
    pub delivered: u64,
    /// Failed attempts in a row.
    #[serde(default)]
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery: Option<Timestamp>,
}

impl Webhook {
    /// As shown to clients: the secret is not.
    pub fn public(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or_default();
        if let Some(map) = value.as_object_mut()
            && map.remove("secret").is_some()
        {
            map.insert("signed".into(), json!(true));
        }
        value
    }

    fn wants(&self, change: &Change) -> bool {
        self.labels.is_empty() || change.labels.iter().any(|l| self.labels.contains(l))
    }
}

pub fn webhooks(read: &ReadTx, ns: &str) -> grafeno::Result<Vec<Webhook>> {
    let mut out = Vec::new();
    for (_, bytes) in read.state_entries(ns, WEBHOOK, "", 10_000)? {
        if let Ok(hook) = serde_json::from_slice(&bytes) {
            out.push(hook);
        }
    }
    Ok(out)
}

pub fn save_webhook(tx: &mut WriteTx, ns: &str, hook: &Webhook) -> grafeno::Result<()> {
    let bytes = serde_json::to_vec(hook).map_err(|e| grafeno::Error::Invalid(e.to_string()))?;
    tx.put_state(ns, WEBHOOK, &hook.id, &bytes)
}

/// HMAC-SHA256 (RFC 2104), hex.
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut key = if secret.len() > BLOCK {
        Sha256::digest(secret).to_vec()
    } else {
        secret.to_vec()
    };
    key.resize(BLOCK, 0);
    let inner: Vec<u8> = key.iter().map(|b| b ^ 0x36).collect();
    let outer: Vec<u8> = key.iter().map(|b| b ^ 0x5c).collect();
    let mut hash = Sha256::new();
    hash.update(&inner);
    hash.update(body);
    let inner_hash = hash.finalize();
    let mut hash = Sha256::new();
    hash.update(&outer);
    hash.update(inner_hash);
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Changes per delivery at most.
const DELIVERY_BATCH: usize = 200;

/// Delivers what each webhook has not received yet. Failing webhooks wait
/// longer each time (1 s, doubling, up to 10 minutes) in `backoff`.
pub fn deliver(state: &AppState, backoff: &mut HashMap<String, Instant>) -> grafeno::Result<usize> {
    let read = state.db.begin_read()?;
    let mut delivered = 0;
    for info in read.namespaces()? {
        let ns = info.name;
        for mut hook in webhooks(&read, &ns)? {
            if backoff.get(&hook.id).is_some_and(|at| Instant::now() < *at) {
                continue;
            }
            let page = read.changes(&ns, hook.cursor, DELIVERY_BATCH)?;
            if page.changes.is_empty() && !(page.gap && hook.cursor < page.oldest) {
                continue;
            }
            let next = page.next(hook.cursor);
            let changes: Vec<&Change> = page.changes.iter().filter(|c| hook.wants(c)).collect();
            let outcome = if changes.is_empty() {
                Ok(())
            } else {
                let body = serde_json::to_vec(&json!({
                    "namespace": ns,
                    "webhook": hook.id,
                    "changes": changes,
                    "last": page.last,
                    "gap": page.gap,
                }))
                .unwrap_or_default();
                post_hook(&hook, &body, state.background.webhooks_to_private)
            };
            match outcome {
                Ok(()) => {
                    delivered += changes.len();
                    hook.delivered += changes.len() as u64;
                    hook.cursor = next.max(if page.gap {
                        page.oldest.saturating_sub(1)
                    } else {
                        0
                    });
                    hook.failures = 0;
                    hook.last_error = None;
                    if !changes.is_empty() {
                        hook.last_delivery = Some(Timestamp::now());
                    }
                    backoff.remove(&hook.id);
                }
                Err(error) => {
                    hook.failures += 1;
                    hook.last_error = Some(error);
                    let wait = Duration::from_secs(1 << hook.failures.min(10))
                        .min(Duration::from_secs(600));
                    backoff.insert(hook.id.clone(), Instant::now() + wait);
                }
            }
            let ns = ns.clone();
            state.writer.write(move |tx| save_webhook(tx, &ns, &hook))?;
        }
        delivered += deliver_events(state, &read, &ns, backoff)?;
    }
    Ok(delivered)
}

/// State kind of where an event subscription's webhook is, keyed by the
/// subscription's name (16.10).
pub const EVENT_DELIVERY: &str = "event.delivery";

/// Where the webhook of an event subscription is.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventDelivery {
    /// The subscription it is for (a new one with the name starts over).
    pub created_at: Option<Timestamp>,
    /// The last change log entry delivered (or skipped).
    pub cursor: u64,
    #[serde(default)]
    pub delivered: u64,
    #[serde(default)]
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery: Option<Timestamp>,
}

pub fn event_delivery(
    read: &ReadTx,
    ns: &str,
    name: &str,
) -> grafeno::Result<Option<EventDelivery>> {
    Ok(read
        .state(ns, EVENT_DELIVERY, name)?
        .and_then(|bytes| serde_json::from_slice(&bytes).ok()))
}

/// Posts the events of the namespace's subscriptions delivered to a
/// webhook, in order, as [`deliver`] does for webhooks.
fn deliver_events(
    state: &AppState,
    read: &ReadTx,
    ns: &str,
    backoff: &mut HashMap<String, Instant>,
) -> grafeno::Result<usize> {
    let mut delivered = 0;
    for subscription in read.subscriptions(ns)? {
        let grafeno::Delivery::Webhook { url, secret } = &subscription.delivery else {
            continue;
        };
        let key = format!("event\0{ns}\0{}", subscription.name);
        if backoff.get(&key).is_some_and(|at| Instant::now() < *at) {
            continue;
        }
        let mut place = event_delivery(read, ns, &subscription.name)?
            .filter(|place| place.created_at == Some(subscription.created_at))
            .unwrap_or_else(|| EventDelivery {
                created_at: Some(subscription.created_at),
                ..Default::default()
            });
        let page = read.events(ns, &subscription.name, place.cursor, DELIVERY_BATCH)?;
        let next = page.next(place.cursor);
        if page.changes.is_empty() && next == place.cursor {
            continue;
        }
        let outcome = if page.changes.is_empty() {
            Ok(())
        } else {
            let body = serde_json::to_vec(&json!({
                "namespace": ns,
                "subscription": subscription.name,
                "events": page.changes,
                "last": page.last,
                "gap": page.gap,
            }))
            .unwrap_or_default();
            // Made in GQ, where the server's settings are not known: its
            // address is checked here, before each delivery.
            if state.background.webhooks_to_private {
                post(url, secret.as_deref(), &body)
            } else {
                destination_allowed(url).and_then(|()| post(url, secret.as_deref(), &body))
            }
        };
        match outcome {
            Ok(()) => {
                let count = page.changes.len();
                delivered += count;
                place.delivered += count as u64;
                place.cursor = next;
                place.failures = 0;
                place.last_error = None;
                if count > 0 {
                    place.last_delivery = Some(Timestamp::now());
                }
                backoff.remove(&key);
            }
            Err(error) => {
                place.failures += 1;
                place.last_error = Some(error);
                let wait =
                    Duration::from_secs(1 << place.failures.min(10)).min(Duration::from_secs(600));
                backoff.insert(key, Instant::now() + wait);
            }
        }
        let (ns, name) = (ns.to_owned(), subscription.name.clone());
        let bytes = serde_json::to_vec(&place).unwrap_or_default();
        state
            .writer
            .write(move |tx| tx.put_state(&ns, EVENT_DELIVERY, &name, &bytes))?;
    }
    Ok(delivered)
}

fn post_hook(hook: &Webhook, body: &[u8], private: bool) -> Result<(), String> {
    // Looked up again for each delivery: a name may point elsewhere now.
    if !private {
        destination_allowed(&hook.url)?;
    }
    post(&hook.url, hook.secret.as_deref(), body)
}

/// Whether an address is on the public internet: not this machine, a
/// private or link-local network (cloud metadata services live there), a
/// shared or reserved range, or a multicast group.
pub fn public_address(address: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match address {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (18..20).contains(&b))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_address(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first == 0x2001 && v6.segments()[1] == 0x0db8))
        }
    }
}

/// Refuses a webhook URL whose host is, or resolves to, an address that
/// is not public ([`public_address`]). Every address the name has must be
/// public, so that a name cannot answer with a public one at first and a
/// private one later from the same lookup.
pub fn destination_allowed(url: &str) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    let uri: axum::http::Uri = url.parse().map_err(|_| format!("{url:?} is not a URL"))?;
    let host = uri
        .host()
        .ok_or_else(|| format!("{url:?} names no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });
    let addresses: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("{host} cannot be looked up: {error}"))?
        .collect();
    if addresses.is_empty() {
        return Err(format!("{host} has no address"));
    }
    if let Some(private) = addresses.iter().find(|a| !public_address(a.ip())) {
        return Err(format!(
            "{host} is {}, which is not a public address: webhooks reach only the public internet \
             unless [background] webhooks_to_private = true",
            private.ip()
        ));
    }
    Ok(())
}

fn post(url: &str, secret: Option<&str>, body: &[u8]) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        // A redirect would go to an address nobody checked.
        .max_redirects(0)
        .build()
        .into();
    let mut request = agent
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "grafeno-webhook");
    if let Some(secret) = secret {
        request = request.header(
            "X-Grafeno-Signature",
            &format!("sha256={}", sign(secret.as_bytes(), body)),
        );
    }
    match request.send(body) {
        Ok(response) if response.status().is_success() => Ok(()),
        Ok(response) => Err(format!("the URL answered {}", response.status())),
        Err(error) => Err(error.to_string()),
    }
}

// ------------------------------------------------------------- ingestion

/// State kind of idempotency keys: key -> what it created.
pub const INGEST_KEY: &str = "ingest.key";

/// One NDJSON line: a node or an edge, with an optional idempotency key.
/// An edge's `from` and `to` may be `$key` for a node ingested with that
/// key, in this request or an earlier one.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngestLine {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub node: Option<NodeInput>,
    #[serde(default)]
    pub edge: Option<EdgeInput>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct IngestReport {
    pub lines: usize,
    pub nodes: usize,
    pub edges: usize,
    /// Lines whose key was seen before: nothing written again.
    pub duplicates: usize,
    pub failed: usize,
    /// The first errors, with their line numbers (from 1).
    pub errors: Vec<Value>,
}

impl IngestReport {
    pub fn add(&mut self, other: IngestReport) {
        self.lines += other.lines;
        self.nodes += other.nodes;
        self.edges += other.edges;
        self.duplicates += other.duplicates;
        self.failed += other.failed;
        for error in other.errors {
            if self.errors.len() < 100 {
                self.errors.push(error);
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Seen {
    id: String,
    kind: String,
}

fn key_ref(tx: &WriteTx, ns: &str, name: &str) -> grafeno::Result<Option<NodeId>> {
    let Some(bytes) = tx.state(ns, INGEST_KEY, name)? else {
        return Ok(None);
    };
    let seen: Seen =
        serde_json::from_slice(&bytes).map_err(|e| grafeno::Error::Invalid(e.to_string()))?;
    Ok((seen.kind == "node")
        .then(|| crate::api::parse_id(&seen.id).ok())
        .flatten())
}

/// Writes a batch of lines (numbered from `first`) in one transaction;
/// each line fails alone. Safe to run again: keys already written are
/// duplicates.
pub fn ingest_batch(
    tx: &mut WriteTx,
    ns: &str,
    first: usize,
    lines: &[Result<IngestLine, String>],
) -> grafeno::Result<IngestReport> {
    let mut report = IngestReport::default();
    let mut refs: HashMap<String, NodeId> = HashMap::new();
    for (offset, line) in lines.iter().enumerate() {
        report.lines += 1;
        let number = first + offset;
        let outcome = (|| -> grafeno::Result<&str> {
            let line = line
                .as_ref()
                .map_err(|e| grafeno::Error::Invalid(e.clone()))?;
            if let Some(key) = &line.key
                && tx.state(ns, INGEST_KEY, key)?.is_some()
            {
                return Ok("duplicate");
            }
            let (id, kind) = match (&line.node, &line.edge) {
                (Some(node), None) => {
                    let id = match node.id {
                        Some(id) => {
                            tx.upsert_node(ns, id, node.clone().into_new())?;
                            id
                        }
                        None => tx.create_node(ns, node.clone().into_new())?,
                    };
                    if let Some(key) = &line.key {
                        refs.insert(format!("${key}"), id);
                    }
                    (id.to_string(), "node")
                }
                (None, Some(edge)) => {
                    for end in [&edge.from, &edge.to] {
                        if let Some(name) = end.strip_prefix('$')
                            && !refs.contains_key(end.as_str())
                            && let Some(id) = key_ref(tx, ns, name)?
                        {
                            refs.insert(end.clone(), id);
                        }
                    }
                    let id = tx.create_edge(ns, edge.clone().into_new(&refs)?)?;
                    (id.to_string(), "edge")
                }
                _ => {
                    return Err(grafeno::Error::Invalid(
                        "a line has a `node` or an `edge`".into(),
                    ));
                }
            };
            if let Some(key) = &line.key {
                let seen = serde_json::to_vec(&Seen {
                    id,
                    kind: kind.into(),
                })
                .unwrap_or_default();
                tx.put_state(ns, INGEST_KEY, key, &seen)?;
            }
            Ok(kind)
        })();
        match outcome {
            Ok("duplicate") => report.duplicates += 1,
            Ok("node") => report.nodes += 1,
            Ok(_) => report.edges += 1,
            Err(error) => {
                report.failed += 1;
                if report.errors.len() < 100 {
                    report
                        .errors
                        .push(json!({ "line": number, "error": error.to_string() }));
                }
            }
        }
    }
    Ok(report)
}

// ------------------------------------------------------------- background

/// Work that runs from the change log, at its own pace.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundConfig {
    /// Namespaces whose new and changed texts are extracted (11.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extract: Vec<String>,
    /// Namespaces whose community summaries are kept fresh (11.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub communities: Vec<String>,
    /// At most one community refresh per namespace this often (300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub communities_every_secs: Option<u64>,
    /// Graph changes since the last refresh that call for another (50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub communities_min_changes: Option<u64>,
    /// Removals since a vector graph was built, as a share of its vectors,
    /// that make the server compact it (0.5; 0 never does).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_vectors_at: Option<f64>,
    /// Vector graphs with fewer vectors are left alone (1 000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_vectors_min: Option<u64>,
    /// Nor are graphs with more (50 000): compacting holds the writer
    /// about as long as inserting every vector again (seconds for tens of
    /// thousands, an hour for a million), so a large one is better
    /// compacted at a quiet time with `grafeno compact` or
    /// `POST /v1/{ns}/compact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_vectors_max: Option<u64>,
    /// Also compact a namespace's own vector graph when its estimated
    /// recall@10 (`ef` 64, 50 queries) falls below this (0, the default,
    /// does not measure: each measure reads every vector).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_vectors_below_recall: Option<f64>,
    /// How often vector graphs are looked at (300 seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_vectors_every_secs: Option<u64>,
    /// Let webhooks reach this machine and private networks: loopback,
    /// private, link-local and other addresses that are not on the public
    /// internet (false). Off, such a webhook is refused when it is made
    /// and when its address is looked up for each delivery, so that
    /// someone who may make webhooks cannot point the server at a service
    /// only it can reach.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub webhooks_to_private: bool,
}

impl BackgroundConfig {
    pub fn is_empty(&self) -> bool {
        self.extract.is_empty()
            && self.communities.is_empty()
            && self.communities_every_secs.is_none()
            && self.communities_min_changes.is_none()
            && self.compact_vectors_at.is_none()
            && self.compact_vectors_min.is_none()
            && self.compact_vectors_max.is_none()
            && self.compact_vectors_below_recall.is_none()
            && self.compact_vectors_every_secs.is_none()
            && !self.webhooks_to_private
    }

    fn compact_at(&self) -> f64 {
        self.compact_vectors_at.unwrap_or(0.5)
    }

    fn below_recall(&self) -> f64 {
        self.compact_vectors_below_recall.unwrap_or(0.0)
    }
}

/// One look at every vector graph: compacts those worn past
/// `compact_vectors_at`, or (when asked) whose sampled recall fell below
/// `compact_vectors_below_recall`. A compaction holds the writer while it
/// builds the graph again (about as long as inserting every vector);
/// searches keep the old graph until it commits. Returns what it did.
pub fn compact_vectors_step(state: &AppState) -> grafeno::Result<Option<Value>> {
    let config = &state.background;
    let (at, min, max, below) = (
        config.compact_at(),
        config.compact_vectors_min.unwrap_or(1_000),
        config.compact_vectors_max.unwrap_or(50_000),
        config.below_recall(),
    );
    if at <= 0.0 && below <= 0.0 {
        return Ok(None);
    }
    let mut done = Vec::new();
    let read = state.db.begin_read()?;
    for info in read.namespaces()? {
        let ns = info.name;
        for health in read.vector_health(&ns)? {
            if health.building || health.vectors < min || health.vectors > max {
                continue;
            }
            let mut why = None;
            if at > 0.0 && health.churn >= at {
                why = Some(json!({ "churn": health.churn }));
            } else if below > 0.0 && health.index.is_none() {
                let recall = read.vector_recall(&ns, 50, 64)?;
                if let Some(recall) = recall.filter(|recall| *recall < below) {
                    why = Some(json!({ "recall": recall }));
                }
            }
            let Some(why) = why else { continue };
            let (ns2, index) = (ns.clone(), health.index.clone());
            let report = state.writer.write_bulk(move |tx| match &index {
                Some(name) => tx.compact_vector_index(&ns2, name),
                None => tx.compact_vectors(&ns2),
            })?;
            done.push(json!({
                "namespace": ns,
                "index": health.index,
                "because": why,
                "removed": health.removed,
                "report": report,
            }));
        }
    }
    Ok((!done.is_empty()).then(|| json!({ "compacted": done })))
}

/// What a background task has done.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RunnerStatus {
    pub namespace: String,
    pub task: String,
    /// The last change it has seen.
    pub cursor: u64,
    pub runs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// What the last run did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_report: Option<Value>,
}

/// State kind of the runners' cursors.
const RUNNER: &str = "runner";

fn cursor(db: &Grafeno, ns: &str, task: &str) -> grafeno::Result<u64> {
    Ok(db
        .begin_read()?
        .state(ns, RUNNER, task)?
        .and_then(|bytes| bytes.try_into().ok().map(u64::from_le_bytes))
        .unwrap_or(0))
}

/// The labels that summaries write, which must not trigger a refresh.
fn own_label(label: &str) -> bool {
    label == grafeno_memory::COMMUNITY
        || label == grafeno_memory::MEMBER_OF
        || label == grafeno_memory::PART_OF
}

/// One step of a task: reads the changes since its cursor, runs if they
/// call for it, and moves the cursor. Returns whether it ran.
pub fn step(
    state: &AppState,
    ns: &str,
    task: &str,
    last_refresh: &mut Option<Instant>,
) -> grafeno::Result<Option<Value>> {
    let db = &state.db;
    let since = cursor(db, ns, task)?;
    let mut seen = since;
    let (mut texts, mut graph) = (0u64, 0u64);
    loop {
        let page = db.begin_read()?.changes(ns, seen, 5_000)?;
        if page.gap && seen < page.oldest {
            // Missed changes: treat everything as changed.
            texts += 1;
            graph += u64::MAX / 4;
        }
        for change in &page.changes {
            if change.parts.contains(&grafeno::Part::Text)
                && change
                    .labels
                    .iter()
                    .any(|l| grafeno_memory::SOURCE_LABELS.contains(&l.as_str()))
            {
                texts += 1;
            }
            if !change.labels.iter().any(|l| own_label(l)) {
                graph += 1;
            }
        }
        let next = page.next(seen);
        if next == seen {
            break;
        }
        seen = next;
    }
    let report = match task {
        "extract" if texts > 0 => Some(run_extract(state, ns)?),
        "communities" => {
            let every = Duration::from_secs(state.background.communities_every_secs.unwrap_or(300));
            let enough = graph >= state.background.communities_min_changes.unwrap_or(50);
            let due = last_refresh.is_none_or(|at| at.elapsed() >= every);
            if enough && due {
                *last_refresh = Some(Instant::now());
                let plan = grafeno_memory::plan_communities(
                    &db.begin_read()?,
                    ns,
                    &grafeno_memory::CommunityOptions::default(),
                )?;
                let plan = Arc::new(plan);
                let ns_owned = ns.to_owned();
                let report = state
                    .writer
                    .write(move |tx| grafeno_memory::apply_communities(tx, &ns_owned, &plan))?;
                Some(serde_json::to_value(report).unwrap_or_default())
            } else if !enough {
                // Not enough yet: keep counting from the same place.
                return Ok(None);
            } else {
                return Ok(None);
            }
        }
        _ => None,
    };
    if seen != since {
        let (ns, task) = (ns.to_owned(), task.to_owned());
        state
            .writer
            .write(move |tx| tx.put_state(&ns, RUNNER, &task, &seen.to_le_bytes()))?;
    }
    Ok(report)
}

fn run_extract(state: &AppState, ns: &str) -> grafeno::Result<Value> {
    let options = Arc::new(grafeno_memory::ExtractOptions::default());
    let plan = Arc::new(grafeno_memory::plan(&state.db.begin_read()?, ns, &options)?);
    let mut report = grafeno_memory::ExtractReport {
        unchanged: plan.unchanged,
        ..Default::default()
    };
    for batch in plan.sources.chunks(options.batch.max(1)) {
        let (db, plan, options, ns, batch) = (
            state.db.clone(),
            plan.clone(),
            options.clone(),
            ns.to_owned(),
            batch.to_vec(),
        );
        let done = state.writer.write_bulk(move |tx| {
            let read = db.begin_read()?;
            grafeno_memory::extract_batch(tx, &read, &ns, &plan, &batch, &options)
        })?;
        report.read += done.read;
        report.mentions += done.mentions;
        report.linked += done.linked;
        report.created += done.created;
        report.queued += done.queued;
    }
    Ok(serde_json::to_value(report).unwrap_or_default())
}

/// Starts the webhook deliverer and the background tasks on the current
/// Tokio runtime.
pub fn start(state: &AppState) {
    // Webhooks: after each commit, and every few seconds for retries.
    {
        let state = state.clone();
        let mut commits = state.events.subscribe();
        tokio::spawn(async move {
            let backoff = Arc::new(Mutex::new(HashMap::new()));
            loop {
                let (state, backoff) = (state.clone(), backoff.clone());
                // On a cluster, only the leader delivers: its cursors are
                // replicated, so a new leader resumes where it stopped.
                if state.leading() {
                    let _ = tokio::task::spawn_blocking(move || {
                        let mut backoff = backoff.lock().unwrap_or_else(|e| e.into_inner());
                        deliver(&state, &mut backoff)
                    })
                    .await;
                }
                tokio::select! {
                    changed = commits.changed() => if changed.is_err() { return },
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        });
    }
    // Vector graphs worn by removals are built again, on the leader.
    if state.background.compact_at() > 0.0 || state.background.below_recall() > 0.0 {
        let state = state.clone();
        let index = {
            let mut runners = state.runners.lock().unwrap_or_else(|e| e.into_inner());
            runners.push(RunnerStatus {
                namespace: "*".into(),
                task: "compact_vectors".into(),
                ..RunnerStatus::default()
            });
            runners.len() - 1
        };
        let every = Duration::from_secs(
            state
                .background
                .compact_vectors_every_secs
                .unwrap_or(300)
                .max(1),
        );
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                if !state.leading() {
                    continue;
                }
                let state2 = state.clone();
                let outcome =
                    tokio::task::spawn_blocking(move || compact_vectors_step(&state2)).await;
                if let Ok(report) = outcome {
                    let mut runners = state.runners.lock().unwrap_or_else(|e| e.into_inner());
                    let status = &mut runners[index];
                    match report {
                        Ok(Some(report)) => {
                            status.runs += 1;
                            status.last_run = Some(Timestamp::now());
                            status.last_report = Some(report);
                            status.last_error = None;
                        }
                        Ok(None) => {}
                        Err(error) => status.last_error = Some(error.to_string()),
                    }
                }
            }
        });
    }
    let tasks: Vec<(String, &str)> = state
        .background
        .extract
        .iter()
        .map(|ns| (ns.clone(), "extract"))
        .chain(
            state
                .background
                .communities
                .iter()
                .map(|ns| (ns.clone(), "communities")),
        )
        .collect();
    let mut started: HashSet<(String, &str)> = HashSet::new();
    for (ns, task) in tasks {
        if !started.insert((ns.clone(), task)) {
            continue;
        }
        let state = state.clone();
        let mut commits = state.events.subscribe();
        let index = {
            let mut runners = state.runners.lock().unwrap_or_else(|e| e.into_inner());
            runners.push(RunnerStatus {
                namespace: ns.clone(),
                task: task.into(),
                ..RunnerStatus::default()
            });
            runners.len() - 1
        };
        tokio::spawn(async move {
            let last_refresh = Arc::new(Mutex::new(None));
            loop {
                // On a cluster, background work runs on the leader alone.
                if !state.leading() {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                let (state2, ns2, last) = (state.clone(), ns.clone(), last_refresh.clone());
                let outcome = tokio::task::spawn_blocking(move || {
                    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
                    let report = step(&state2, &ns2, task, &mut last);
                    (report, cursor(&state2.db, &ns2, task).unwrap_or(0))
                })
                .await;
                if let Ok((report, at)) = outcome {
                    let mut runners = state.runners.lock().unwrap_or_else(|e| e.into_inner());
                    let status = &mut runners[index];
                    status.cursor = at;
                    match report {
                        Ok(Some(report)) => {
                            status.runs += 1;
                            status.last_run = Some(Timestamp::now());
                            status.last_report = Some(report);
                            status.last_error = None;
                        }
                        Ok(None) => {}
                        Err(error) => status.last_error = Some(error.to_string()),
                    }
                }
                // Its own pace: wait for a commit, then let writes settle.
                tokio::select! {
                    changed = commits.changed() => if changed.is_err() { return },
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::sign;

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2.
        assert_eq!(
            sign(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
