//! Limits that keep one query from taking the service down: time, records
//! read, rows held at once, memory of the values it builds, estimated cost,
//! and cancellation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::QueryError;
use crate::value::Value;

/// How deeply lists and maps may nest inside one value. Deeper values
/// would take more stack than a query thread has to build, compare, print
/// or drop.
pub(crate) const MAX_VALUE_DEPTH: usize = 128;

/// What one query may use. `None` is no limit.
#[derive(Clone, Debug, Default)]
pub struct Limits {
    /// Wall time, from parsing to the last row.
    pub timeout: Option<Duration>,
    /// Records read: nodes, edges, adjacency and index entries.
    pub max_reads: Option<u64>,
    /// Rows held at once between clauses, in a match, a sort or a group.
    pub max_rows: Option<usize>,
    /// Bytes, estimated, of one value the query builds (a string, a list, a
    /// map) and of the rows it holds at once.
    pub max_memory: Option<usize>,
    /// The planner's cost estimate (see `EXPLAIN`) above which a query is
    /// refused before it runs.
    pub max_cost: Option<f64>,
    /// Set by the caller to stop the query (when its client goes away).
    pub cancel: Option<Arc<AtomicBool>>,
    /// The namespaces the caller may see, for statements that list them
    /// (`SHOW NAMESPACES`); all when `None`.
    pub visible: Option<NamespaceFilter>,
    /// Who runs access statements and what they may manage (14.3);
    /// everything when `None`.
    pub access: Option<crate::access::AccessContext>,
    /// What the caller may see and change record by record (14.4): hidden
    /// nodes, edges and properties are absent; every record when `None`.
    pub view: Option<Arc<grafeno::View>>,
}

/// Which namespaces a caller may see.
#[derive(Clone)]
pub struct NamespaceFilter(pub Arc<dyn Fn(&str) -> bool + Send + Sync>);

impl NamespaceFilter {
    pub fn new(filter: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(filter))
    }

    pub fn allows(&self, ns: &str) -> bool {
        (self.0)(ns)
    }
}

impl std::fmt::Debug for NamespaceFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NamespaceFilter")
    }
}

impl Limits {
    /// The server's defaults: 30 seconds, 50 million records read, a
    /// million rows and 512 MiB in memory, no cost limit.
    pub fn standard() -> Self {
        Self {
            timeout: Some(Duration::from_secs(30)),
            max_reads: Some(50_000_000),
            max_rows: Some(1_000_000),
            max_memory: Some(512 * 1024 * 1024),
            max_cost: None,
            cancel: None,
            visible: None,
            access: None,
            view: None,
        }
    }

    /// No limits: for the library and the local shell.
    pub fn none() -> Self {
        Self::default()
    }
}

/// The running account of a query against its [`Limits`].
pub(crate) struct Budget {
    limits: Limits,
    started: Instant,
    reads: AtomicU64,
    work: AtomicU64,
    /// Bytes of rows being built by one step (a procedure), until the step
    /// ends and its rows are checked with [`Budget::held`].
    building: AtomicUsize,
}

impl Budget {
    pub(crate) fn new(limits: &Limits) -> Self {
        Self {
            limits: limits.clone(),
            started: Instant::now(),
            reads: AtomicU64::new(0),
            work: AtomicU64::new(0),
            building: AtomicUsize::new(0),
        }
    }

    /// Counts `bytes` more of rows a step is building, and checks the
    /// memory limit before the step ends.
    pub(crate) fn build(&self, bytes: usize) -> Result<(), QueryError> {
        let total = self
            .building
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        self.memory(total, "built rows")
    }

    pub(crate) fn max_cost(&self) -> Option<f64> {
        self.limits.max_cost
    }

    /// Counts `n` units of computation (an item of a loop, a kilobyte
    /// copied) and checks time and cancellation every thousand or so, so
    /// that queries that compute without reading still stop.
    pub(crate) fn work(&self, n: u64) -> Result<(), QueryError> {
        let before = self.work.fetch_add(n, Ordering::Relaxed);
        let after = before.saturating_add(n);
        if before >> 10 != after >> 10 {
            self.tick()?;
        }
        Ok(())
    }

    /// Checks a value the query built: how deeply it nests and how large it
    /// is, and counts the work of building it.
    pub(crate) fn value(&self, value: &Value) -> Result<(), QueryError> {
        let bytes = footprint(value, MAX_VALUE_DEPTH).ok_or_else(|| {
            QueryError::limit(format!(
                "the query built a value nested more than {MAX_VALUE_DEPTH} lists or maps deep"
            ))
        })?;
        self.work(bytes as u64 / 256)?;
        self.memory(bytes, "built a value")
    }

    /// Checks `bytes` against the memory limit.
    pub(crate) fn memory(&self, bytes: usize, what: &str) -> Result<(), QueryError> {
        if let Some(max) = self.limits.max_memory
            && bytes > max
        {
            return Err(QueryError::limit(format!(
                "the query {what} of more than {} MiB (limit max_memory); aggregate earlier or return less",
                max / (1024 * 1024)
            )));
        }
        Ok(())
    }

    /// Checks the rows held at once: how many, and an estimate of their
    /// memory from a sample of them.
    pub(crate) fn held<'r, R: 'r>(
        &self,
        rows: &'r [R],
        values: impl Fn(&'r R) -> Box<dyn Iterator<Item = &'r Value> + 'r>,
    ) -> Result<(), QueryError> {
        self.building.store(0, Ordering::Relaxed);
        self.rows(rows.len())?;
        if self.limits.max_memory.is_none() || rows.is_empty() {
            return Ok(());
        }
        let step = (rows.len() / 32).max(1);
        let (mut sampled, mut bytes) = (0usize, 0usize);
        for row in rows.iter().step_by(step) {
            sampled += 1;
            // A row is a map from column names to values.
            bytes += 64;
            for value in values(row) {
                bytes += 96 + footprint(value, MAX_VALUE_DEPTH).unwrap_or(0);
            }
        }
        self.memory(bytes / sampled * rows.len(), "held rows")
    }

    pub(crate) fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Counts `n` records read, and checks every limit.
    pub(crate) fn read(&self, n: usize) -> Result<(), QueryError> {
        let reads = self
            .reads
            .fetch_add(n as u64, Ordering::Relaxed)
            .saturating_add(n as u64);
        if let Some(max) = self.limits.max_reads
            && reads > max
        {
            return Err(QueryError::limit(format!(
                "the query read more than {max} records (limit max_reads); add an index, a label or a LIMIT"
            )));
        }
        self.tick()
    }

    /// Checks the time limit and cancellation.
    pub(crate) fn tick(&self) -> Result<(), QueryError> {
        if let Some(timeout) = self.limits.timeout
            && self.started.elapsed() > timeout
        {
            return Err(QueryError::limit(format!(
                "the query ran longer than {} s (limit timeout)",
                timeout.as_secs_f64()
            )));
        }
        if self
            .limits
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
        {
            return Err(QueryError::limit("the query was cancelled"));
        }
        Ok(())
    }

    /// A check of the time limit and cancellation that other threads can
    /// run (graph algorithms).
    pub(crate) fn stopper(&self) -> impl Fn() -> bool + Sync + 'static {
        let deadline = self.limits.timeout.map(|timeout| self.started + timeout);
        let cancel = self.limits.cancel.clone();
        move || {
            deadline.is_some_and(|deadline| Instant::now() > deadline)
                || cancel
                    .as_ref()
                    .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
        }
    }

    /// Checks how many rows are held at once.
    pub(crate) fn rows(&self, n: usize) -> Result<(), QueryError> {
        if let Some(max) = self.limits.max_rows
            && n > max
        {
            return Err(QueryError::limit(format!(
                "the query held more than {max} rows at once (limit max_rows); narrow the match or aggregate earlier"
            )));
        }
        Ok(())
    }
}

/// The bytes properties take: a tree node per eleven entries, and the
/// keys and values.
fn props_footprint(props: &grafeno::Props) -> usize {
    if props.is_empty() {
        return 0;
    }
    let mut bytes = props.len().div_ceil(11) * 640;
    for (key, value) in props {
        bytes += key.len() + json_footprint(value, 8);
    }
    bytes
}

fn json_footprint(value: &serde_json::Value, depth: usize) -> usize {
    match value {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(items) if depth > 0 => {
            items.len() * 32
                + items
                    .iter()
                    .map(|item| json_footprint(item, depth - 1))
                    .sum::<usize>()
        }
        serde_json::Value::Object(map) if depth > 0 => map
            .iter()
            .map(|(key, item)| 96 + key.len() + json_footprint(item, depth - 1))
            .sum(),
        _ => 0,
    }
}

/// The bytes a value takes, roughly; `None` when lists and maps nest deeper
/// than `depth`. A node or an edge counts whole: each row read its own copy
/// from storage.
pub(crate) fn footprint(value: &Value, depth: usize) -> Option<usize> {
    const SLOT: usize = std::mem::size_of::<Value>();
    Some(match value {
        Value::Node(node) => {
            160 + node
                .labels
                .iter()
                .map(|label| 40 + label.len())
                .sum::<usize>()
                + node.sources.len() * 16
                + props_footprint(&node.props)
        }
        Value::Edge(edge) => {
            200 + edge.label.len() + edge.sources.len() * 16 + props_footprint(&edge.props)
        }
        Value::HyperEdge(edge) => {
            200 + edge.label.len()
                + edge
                    .participants
                    .iter()
                    .map(|p| 40 + p.role.as_ref().map_or(0, String::len))
                    .sum::<usize>()
                + edge.sources.len() * 16
                + props_footprint(&edge.props)
        }
        Value::Str(s) => s.len(),
        Value::List(items) => {
            let depth = depth.checked_sub(1)?;
            let mut bytes = items.len() * SLOT;
            for item in items {
                if !matches!(
                    item,
                    Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)
                ) {
                    bytes += footprint(item, depth)?;
                }
            }
            bytes
        }
        Value::Map(entries) => {
            let depth = depth.checked_sub(1)?;
            let mut bytes = 0;
            for (key, item) in entries {
                bytes += 64 + key.len() + footprint(item, depth)?;
            }
            bytes
        }
        _ => 0,
    })
}
