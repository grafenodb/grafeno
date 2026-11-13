//! `GET /v1/metrics`: what the server and its writer are doing, in the
//! Prometheus text format.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use grafeno::{Histogram, HistogramSnapshot, WriterSnapshot};

/// The limits a query can hit, as they are named in their errors.
pub const LIMITS: [&str; 6] = [
    "timeout",
    "max_reads",
    "max_rows",
    "max_memory",
    "max_cost",
    "cancelled",
];

/// Counters of the HTTP side; the writer keeps its own.
pub struct ServerMetrics {
    /// Requests by status class: 2xx, 3xx, 4xx, 5xx.
    requests: [AtomicU64; 4],
    /// Requests turned away because too many were being served.
    busy: AtomicU64,
    request_seconds: Histogram,
    queries: AtomicU64,
    query_seconds: Histogram,
    /// Queries stopped by each of [`LIMITS`].
    limits: [AtomicU64; 6],
    /// Refusals counted instead of written to the audit log.
    audit_left_out: AtomicU64,
}

impl Default for ServerMetrics {
    fn default() -> Self {
        Self {
            requests: Default::default(),
            busy: AtomicU64::new(0),
            request_seconds: Histogram::seconds(),
            queries: AtomicU64::new(0),
            query_seconds: Histogram::seconds(),
            limits: Default::default(),
            audit_left_out: AtomicU64::new(0),
        }
    }
}

impl ServerMetrics {
    pub fn request(&self, status: u16, seconds: f64) {
        let class = (usize::from(status / 100)).clamp(2, 5) - 2;
        self.requests[class].fetch_add(1, Ordering::Relaxed);
        self.request_seconds.observe(seconds);
    }

    pub fn busy(&self) {
        self.busy.fetch_add(1, Ordering::Relaxed);
    }

    pub fn audit_left_out(&self) {
        self.audit_left_out.fetch_add(1, Ordering::Relaxed);
    }

    /// A GQ query ran for `seconds`; `limit_message` is the message of the
    /// limit it hit, if any.
    pub fn query(&self, seconds: f64, limit_message: Option<&str>) {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.query_seconds.observe(seconds);
        if let Some(message) = limit_message {
            let at = LIMITS
                .iter()
                .position(|limit| message.contains(limit))
                .unwrap_or(0);
            self.limits[at].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Everything, as Prometheus text.
    pub fn render(&self, writer: &WriterSnapshot, projection_cache_bytes: usize) -> String {
        let mut out = String::new();
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        family(
            &mut out,
            "grafeno_http_requests_total",
            "counter",
            "HTTP requests under /v1 answered, by status class.",
        );
        for (at, class) in ["2xx", "3xx", "4xx", "5xx"].iter().enumerate() {
            let _ = writeln!(
                out,
                "grafeno_http_requests_total{{status=\"{class}\"}} {}",
                load(&self.requests[at])
            );
        }
        counter(
            &mut out,
            "grafeno_http_busy_total",
            "Requests answered 503 because too many were being served.",
            load(&self.busy),
        );
        counter(
            &mut out,
            "grafeno_audit_refusals_left_out_total",
            "Refusals counted instead of written to the audit log (past ten a minute from one source).",
            load(&self.audit_left_out),
        );
        histogram(
            &mut out,
            "grafeno_http_request_seconds",
            "Time to answer a request under /v1.",
            &[],
            &self.request_seconds.snapshot(),
        );
        counter(
            &mut out,
            "grafeno_queries_total",
            "GQ queries run.",
            load(&self.queries),
        );
        histogram(
            &mut out,
            "grafeno_query_seconds",
            "Time a GQ query took, to its result or error.",
            &[],
            &self.query_seconds.snapshot(),
        );
        family(
            &mut out,
            "grafeno_query_limits_total",
            "counter",
            "GQ queries stopped by a limit, by limit.",
        );
        for (at, limit) in LIMITS.iter().enumerate() {
            let _ = writeln!(
                out,
                "grafeno_query_limits_total{{limit=\"{limit}\"}} {}",
                load(&self.limits[at])
            );
        }

        family(
            &mut out,
            "grafeno_write_queue",
            "gauge",
            "Writes waiting for the writer, by lane.",
        );
        let _ = writeln!(
            out,
            "grafeno_write_queue{{lane=\"interactive\"}} {}",
            writer.queued
        );
        let _ = writeln!(
            out,
            "grafeno_write_queue{{lane=\"bulk\"}} {}",
            writer.queued_bulk
        );
        counter(
            &mut out,
            "grafeno_writes_total",
            "Writes done, committed or failed.",
            writer.writes,
        );
        counter(
            &mut out,
            "grafeno_write_failures_total",
            "Writes that failed.",
            writer.failed,
        );
        counter(
            &mut out,
            "grafeno_write_rejected_total",
            "Writes refused because the queue was full (503).",
            writer.rejected,
        );
        counter(
            &mut out,
            "grafeno_write_retries_total",
            "Writes run again after another write of their group failed.",
            writer.retried,
        );
        counter(
            &mut out,
            "grafeno_write_groups_total",
            "Write transactions committed.",
            writer.groups,
        );
        histogram(
            &mut out,
            "grafeno_write_group_size",
            "Writes per committed transaction.",
            &[],
            &writer.group_size,
        );
        histogram(
            &mut out,
            "grafeno_commit_seconds",
            "Time a commit took.",
            &[],
            &writer.commit_seconds,
        );
        family(
            &mut out,
            "grafeno_write_seconds",
            "histogram",
            "From a write's arrival to its result, by lane.",
        );
        series(
            &mut out,
            "grafeno_write_seconds",
            &[("lane", "interactive")],
            &writer.latency_seconds,
        );
        series(
            &mut out,
            "grafeno_write_seconds",
            &[("lane", "bulk")],
            &writer.bulk_latency_seconds,
        );
        family(
            &mut out,
            "grafeno_projection_cache_bytes",
            "gauge",
            "Memory of the graph projections kept for algorithms.",
        );
        let _ = writeln!(
            out,
            "grafeno_projection_cache_bytes {projection_cache_bytes}"
        );
        out
    }
}

/// A cluster series: its name, help, and value in a status.
type Series = (
    &'static str,
    &'static str,
    fn(&grafeno_cluster::Status) -> u64,
);

/// The cluster's part of `/metrics`: one series per Raft group this
/// member holds (label `group`), and per other member on a leader.
pub fn render_cluster(statuses: &[(u64, grafeno_cluster::Status)]) -> String {
    let mut out = String::new();
    if statuses.is_empty() {
        return out;
    }
    let gauges: [Series; 7] = [
        (
            "grafeno_cluster_leader",
            "1 on the group's leader, 0 elsewhere",
            |s| u64::from(s.role == "leader"),
        ),
        ("grafeno_cluster_term", "The current Raft term", |s| s.term),
        (
            "grafeno_cluster_commit_index",
            "The last entry a majority holds",
            |s| s.commit,
        ),
        (
            "grafeno_cluster_applied_index",
            "The last entry applied to the database",
            |s| s.applied,
        ),
        (
            "grafeno_cluster_last_index",
            "The last entry in this member's log",
            |s| s.last_index,
        ),
        (
            "grafeno_cluster_snapshot_index",
            "The entry this member's snapshot covers",
            |s| s.snapshot_index,
        ),
        (
            "grafeno_cluster_entry_format",
            "The replication entry format the group writes",
            |s| u64::from(s.format),
        ),
    ];
    for (name, help, value) in gauges {
        family(&mut out, name, "gauge", help);
        for (group, status) in statuses {
            let _ = writeln!(out, "{name}{{group=\"{group}\"}} {}", value(status));
        }
    }
    let counters: [Series; 4] = [
        (
            "grafeno_cluster_elections_total",
            "Elections this member started",
            |s| s.elections,
        ),
        (
            "grafeno_cluster_leader_changes_total",
            "Changes of leader this member saw",
            |s| s.leader_changes,
        ),
        (
            "grafeno_cluster_snapshots_sent_total",
            "Snapshots sent to other members",
            |s| s.snapshots_sent,
        ),
        (
            "grafeno_cluster_snapshots_installed_total",
            "Snapshots installed from a leader",
            |s| s.snapshots_installed,
        ),
    ];
    for (name, help, value) in counters {
        family(&mut out, name, "counter", help);
        for (group, status) in statuses {
            let _ = writeln!(out, "{name}{{group=\"{group}\"}} {}", value(status));
        }
    }
    family(
        &mut out,
        "grafeno_cluster_peer_lag",
        "gauge",
        "On a leader: entries another member lacks",
    );
    for (group, status) in statuses {
        for peer in &status.peers {
            let _ = writeln!(
                out,
                "grafeno_cluster_peer_lag{{group=\"{group}\",member=\"{}\"}} {}",
                peer.id, peer.lag
            );
        }
    }
    out
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    family(out, name, "counter", help);
    let _ = writeln!(out, "{name} {value}");
}

fn histogram(
    out: &mut String,
    name: &str,
    help: &str,
    labels: &[(&str, &str)],
    data: &HistogramSnapshot,
) {
    family(out, name, "histogram", help);
    series(out, name, labels, data);
}

/// The `_bucket`, `_sum` and `_count` lines of one histogram.
fn series(out: &mut String, name: &str, labels: &[(&str, &str)], data: &HistogramSnapshot) {
    let with = |extra: Option<String>| {
        let mut parts: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
        parts.extend(extra);
        if parts.is_empty() {
            String::new()
        } else {
            format!("{{{}}}", parts.join(","))
        }
    };
    let mut cumulative = 0;
    for (at, count) in data.counts.iter().enumerate() {
        cumulative += count;
        let bound = data
            .bounds
            .get(at)
            .map_or("+Inf".to_owned(), |bound| bound.to_string());
        let _ = writeln!(
            out,
            "{name}_bucket{} {cumulative}",
            with(Some(format!("le=\"{bound}\"")))
        );
    }
    let _ = writeln!(out, "{name}_sum{} {}", with(None), data.sum);
    let _ = writeln!(out, "{name}_count{} {}", with(None), data.count());
}
