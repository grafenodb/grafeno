//! Many connections at once: a server in this process on a database file,
//! and thousands of HTTP/1.1 keep-alive connections, each sending its next
//! request as soon as the previous answer arrives (a closed loop). The mix
//! is reads (a node by id, a lookup through an index), GQ queries (one hop
//! out, an aggregate) and writes (a new node, a property set), so the
//! writer, the read pool and the admission queue are all in play.
//!
//! `cargo run --release -p grafeno-server --example many_connections -- 1000 2000 4000`
//! (connections per run; `SECONDS=20` per run, `CSV=path` writes the rows,
//! `MAX_REQUESTS=` and `QUEUE_REQUESTS=` change the server's limits,
//! `WRITES=0` leaves the writes out (`WRITES=only` sends only them), `HEALTH=1` sends only `GET /health`,
//! `SPREAD=1` prints more percentiles, `SERVER_THREADS=` and
//! `CLIENT_THREADS=` size the two runtimes, `KEEP_FILE_LIMIT=1` leaves the
//! open-files limit as it is)
//!
//! Each run prints the requests answered per second, the latency
//! percentiles of each kind, the errors by status, and how many requests
//! waited in the admission queue.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use grafeno::Grafeno;
use grafeno_server::http::{AppState, router};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const TOKEN: &str = "load-secret";
const PEOPLE: usize = 100_000;
/// Threads of the server's runtime and of the clients'.
const SERVER_THREADS: usize = 12;
const CLIENT_THREADS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    ReadNode,
    Lookup,
    Query,
    Write,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::ReadNode => "read node",
            Kind::Lookup => "indexed lookup",
            Kind::Query => "GQ query",
            Kind::Write => "write",
        }
    }
}

/// One keep-alive connection that sends requests and reads the answers.
struct Connection {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl Connection {
    async fn open(addr: SocketAddr) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (read, write) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read),
            writer: write,
        })
    }

    /// Sends one request and reads its answer: the status and the body.
    async fn send(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let payload = body.map(|body| body.to_string()).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nhost: grafeno\r\nauthorization: Bearer {TOKEN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{payload}",
            payload.len()
        );
        self.writer.write_all(request.as_bytes()).await?;
        let mut line = String::new();
        self.reader.read_line(&mut line).await?;
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| std::io::Error::other(format!("bad status line {line:?}")))?;
        let mut length = 0usize;
        loop {
            line.clear();
            if self.reader.read_line(&mut line).await? == 0 {
                return Err(std::io::Error::other("closed"));
            }
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        self.reader.read_exact(&mut body).await?;
        Ok((status, body))
    }
}

#[derive(Default)]
struct Tally {
    latencies: BTreeMap<Kind, Vec<f64>>,
    statuses: BTreeMap<u16, u64>,
    failed: u64,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// One user: a connection, then requests in a closed loop until `stop`.
async fn user(addr: SocketAddr, seed: u64, ids: Arc<Vec<String>>, stop: Arc<AtomicBool>) -> Tally {
    let writes = std::env::var("WRITES").map_or(true, |value| value != "0");
    let health = std::env::var_os("HEALTH").is_some();
    let writes_only = std::env::var("WRITES").is_ok_and(|value| value == "only");
    let mut tally = Tally::default();
    let mut connection = match Connection::open(addr).await {
        Ok(connection) => connection,
        Err(_) => {
            tally.failed += 1;
            return tally;
        }
    };
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    while !stop.load(Ordering::Relaxed) {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut pick = (state >> 33) % 100;
        // `WRITES=0` leaves the writes out (their share goes to reads).
        if pick >= 80 && !writes {
            pick %= 35;
        }
        let person = ((state >> 13) % PEOPLE as u64) as usize;
        // `WRITES=only`: only the writes.
        if writes_only {
            pick = 80 + pick % 20;
        }
        // `HEALTH=1`: only `GET /health` (no token, no database).
        if health {
            pick = 200;
        }
        let (kind, method, path, body) = match pick {
            200 => (Kind::ReadNode, "GET", "/health".to_owned(), None),
            0..=34 => (
                Kind::ReadNode,
                "GET",
                format!("/v1/load/nodes/{}", ids[person]),
                None,
            ),
            35..=59 => (
                Kind::Lookup,
                "POST",
                "/v1/load/query".to_owned(),
                Some(
                    json!({"query": "MATCH (p:Person {code: $code}) RETURN p.name", "params": {"code": format!("C{person}")}}),
                ),
            ),
            60..=79 => (
                Kind::Query,
                "POST",
                "/v1/load/query".to_owned(),
                Some(
                    json!({"query": "MATCH (p:Person {code: $code})-[:knows]->(f) RETURN count(f), max(f.age)", "params": {"code": format!("C{person}")}}),
                ),
            ),
            _ if pick % 2 == 0 => (
                Kind::Write,
                "POST",
                "/v1/load/nodes".to_owned(),
                Some(json!({"labels": ["Visit"], "props": {"person": person, "at": seed}})),
            ),
            _ => (
                Kind::Write,
                "PATCH",
                format!("/v1/load/nodes/{}", ids[person]),
                Some(json!({"props": {"seen": state % 1000}})),
            ),
        };
        let started = Instant::now();
        match connection.send(method, &path, body.as_ref()).await {
            Ok((status, _)) => {
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                *tally.statuses.entry(status).or_default() += 1;
                if status < 300 {
                    tally.latencies.entry(kind).or_default().push(ms);
                } else if status == 503 {
                    // The server said to come back: as a client would.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            Err(_) => {
                tally.failed += 1;
                match Connection::open(addr).await {
                    Ok(fresh) => connection = fresh,
                    Err(_) => return tally,
                }
            }
        }
    }
    tally
}

fn main() {
    let runs: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|arg| arg.parse().ok())
        .collect();
    let runs = if runs.is_empty() {
        vec![1_000, 2_000]
    } else {
        runs
    };
    // As `grafeno serve` does: room for a file descriptor per connection
    // (`KEEP_FILE_LIMIT=1` leaves the limit as it is, to see what it does).
    if std::env::var_os("KEEP_FILE_LIMIT").is_none() {
        grafeno_server::http::open_files_for_connections();
    }
    let seconds: u64 = std::env::var("SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(
            std::env::var("CLIENT_THREADS")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(CLIENT_THREADS),
        )
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let path = std::env::temp_dir().join(format!("grafeno-connections-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Arc::new(Grafeno::open(&path).unwrap());
        // 100,000 people with a code (indexed), each knowing five others.
        let ids: Vec<String> = db
            .write(|tx| {
                let mut ids = Vec::with_capacity(PEOPLE);
                for i in 0..PEOPLE {
                    ids.push(tx.create_node(
                        "load",
                        grafeno::NewNode::new("Person")
                            .prop("code", format!("C{i}"))
                            .prop("name", format!("P{i}"))
                            .prop("age", (i % 90) as i64),
                    )?);
                }
                for i in 0..PEOPLE {
                    for k in 1..=5 {
                        tx.create_edge(
                            "load",
                            grafeno::NewEdge::new(ids[i], "knows", ids[(i * 7 + k * 101) % PEOPLE]),
                        )?;
                    }
                }
                tx.create_index("load", grafeno::IndexDef::new("Person", "code"))?;
                Ok(ids.into_iter().map(|id| id.to_string()).collect())
            })
            .unwrap();
        let ids = Arc::new(ids);
        let mut state = AppState::new(db.clone(), Some(TOKEN));
        if let Some(max) = std::env::var("MAX_REQUESTS").ok().and_then(|s| s.parse().ok()) {
            state = state.with_max_requests(max);
        }
        if let Some(queue) = std::env::var("QUEUE_REQUESTS").ok().and_then(|s| s.parse().ok()) {
            state = state.with_queue(queue, Duration::from_secs(5));
        }
        let metrics = state.metrics.clone();
        let writes = state.writer.clone();
        // The server on a runtime of its own, as in `grafeno serve`, so that
        // the clients' tasks do not take its threads.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            // `BLOCKING_THREADS=`: the threads reads run on (512 by default).
            if let Some(n) = std::env::var("BLOCKING_THREADS")
                .ok()
                .and_then(|n| n.parse().ok())
            {
                builder.max_blocking_threads(n);
            }
            let server = builder
                .worker_threads(
                    std::env::var("SERVER_THREADS")
                        .ok()
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(SERVER_THREADS),
                )
                .enable_all()
                .build()
                .unwrap();
            server.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, router(state)).await.unwrap();
            });
        });
        let mut csv = String::from("connections,kind,requests,per_second,p50_ms,p99_ms,max_ms,errors\n");
        println!("| connections | kind | requests | per second | p50 ms | p99 ms | max ms |");
        println!("| ---: | --- | ---: | ---: | ---: | ---: | ---: |");
        for connections in runs {
            let queued_before = metrics.queued_total();
            let writer_before = writes.metrics();
            let stop = Arc::new(AtomicBool::new(false));
            let users: Vec<_> = (0..connections)
                .map(|n| tokio::spawn(user(addr, n as u64 + 1, ids.clone(), stop.clone())))
                .collect();
            let started = Instant::now();
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            stop.store(true, Ordering::Relaxed);
            let mut total = Tally::default();
            for user in users {
                let tally = user.await.unwrap();
                for (kind, mut times) in tally.latencies {
                    total.latencies.entry(kind).or_default().append(&mut times);
                }
                for (status, count) in tally.statuses {
                    *total.statuses.entry(status).or_default() += count;
                }
                total.failed += tally.failed;
            }
            let elapsed = started.elapsed().as_secs_f64();
            let errors: u64 = total
                .statuses
                .iter()
                .filter(|(status, _)| **status >= 300)
                .map(|(_, count)| *count)
                .sum::<u64>()
                + total.failed;
            let mut all = Vec::new();
            for (kind, times) in &mut total.latencies {
                times.sort_by(f64::total_cmp);
                all.extend_from_slice(times);
                println!(
                    "| {connections} | {} | {} | {:.0} | {:.1} | {:.1} | {:.0} |",
                    kind.name(),
                    times.len(),
                    times.len() as f64 / elapsed,
                    percentile(times, 0.5),
                    percentile(times, 0.99),
                    times.last().copied().unwrap_or(0.0)
                );
                csv.push_str(&format!(
                    "{connections},{},{},{:.0},{:.2},{:.2},{:.1},\n",
                    kind.name(),
                    times.len(),
                    times.len() as f64 / elapsed,
                    percentile(times, 0.5),
                    percentile(times, 0.99),
                    times.last().copied().unwrap_or(0.0)
                ));
            }
            all.sort_by(f64::total_cmp);
            if std::env::var_os("SPREAD").is_some() {
                let at = |p: f64| percentile(&all, p);
                println!(
                    "  spread: p50 {:.1} p75 {:.1} p90 {:.1} p95 {:.1} p97 {:.1} p98 {:.1} p99 {:.1} ms",
                    at(0.5), at(0.75), at(0.9), at(0.95), at(0.97), at(0.98), at(0.99)
                );
            }
            println!(
                "| {connections} | **all** | {} | {:.0} | {:.1} | {:.1} | {:.0} |",
                all.len(),
                all.len() as f64 / elapsed,
                percentile(&all, 0.5),
                percentile(&all, 0.99),
                all.last().copied().unwrap_or(0.0)
            );
            csv.push_str(&format!(
                "{connections},all,{},{:.0},{:.2},{:.2},{:.1},{errors}\n",
                all.len(),
                all.len() as f64 / elapsed,
                percentile(&all, 0.5),
                percentile(&all, 0.99),
                all.last().copied().unwrap_or(0.0)
            ));
            let writer_after = writes.metrics();
            let groups = writer_after.groups - writer_before.groups;
            let committed = writer_after.writes - writer_before.writes;
            let commit_s = writer_after.commit_seconds.sum - writer_before.commit_seconds.sum;
            println!(
                "  writer: {committed} writes in {groups} commits ({:.0} a commit), {:.1} ms a commit at the disk, retried {}",
                committed as f64 / groups.max(1) as f64,
                commit_s * 1000.0 / groups.max(1) as f64,
                writer_after.retried - writer_before.retried
            );
            println!(
                "  errors: {errors} (by status {:?}, connections lost {}), queued {}",
                total
                    .statuses
                    .iter()
                    .filter(|(status, _)| **status >= 300)
                    .collect::<Vec<_>>(),
                total.failed,
                metrics.queued_total() - queued_before
            );
        }
        if let Ok(file) = std::env::var("CSV") {
            std::fs::write(file, csv).unwrap();
        }
        drop(db);
        let _ = std::fs::remove_file(&path);
    });
}
