//! A closed-loop HTTP load against a running Grafeno server: each virtual
//! user sends its next request as soon as the previous answer arrives. It
//! measures the cases where an external benchmark saw the CPU per request
//! grow with concurrency (tools/bench-http/README.md):
//!
//! - `health`: `GET /health` (no token, no database);
//! - `namespaces`: `GET /v1/namespaces` (token, trivial);
//! - `return1`: GQ `RETURN 1` (token, a read snapshot, no data);
//! - `code`: GQ `MATCH (n:mms {code: $code}) RETURN n` through an index.
//!
//! `setup` loads the namespace `bench` with `mms` nodes (a code, a title, a
//! text) and the index on `code`, through GQ.
//!
//! ```text
//! http_load setup <url> <token> <nodes>
//! http_load run <url> <token> <case> <users> <seconds>
//! ```
//!
//! `run` prints one JSON line: requests, errors, requests per second and
//! latency percentiles in milliseconds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn query(agent: &ureq::Agent, url: &str, token: &str, text: &str, params: Value) -> Result<Value, String> {
    let mut response = agent
        .post(format!("{url}/v1/bench/query"))
        .header("authorization", format!("Bearer {token}"))
        .send_json(json!({ "query": text, "params": params }))
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body: Value = response.body_mut().read_json().map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!("HTTP {status}: {body}"));
    }
    Ok(body)
}

fn code(i: u64) -> String {
    format!("C{i:06}")
}

fn setup(url: &str, token: &str, nodes: u64) -> Result<(), String> {
    let agent = agent();
    let _ = agent
        .post(format!("{url}/v1/namespaces"))
        .header("authorization", format!("Bearer {token}"))
        .send_json(json!({ "name": "bench" }));
    query(&agent, url, token, "CREATE INDEX mms_code IF NOT EXISTS FOR (n:mms) ON (n.code)", json!({}))?;
    let chunk = 5_000;
    let mut start = 0;
    while start < nodes {
        let rows: Vec<Value> = (start..(start + chunk).min(nodes))
            .map(|i| {
                json!({
                    "code": code(i),
                    "title": format!("Entity {i} of the synthetic catalogue"),
                    "text": format!("Synthetic entity {i}: a disorder of the system number {}", i % 97),
                })
            })
            .collect();
        query(
            &agent,
            url,
            token,
            "UNWIND $rows AS r CREATE (:mms {code: r.code, title: r.title, text: r.text})",
            json!({ "rows": rows }),
        )?;
        start += chunk;
    }
    let count = query(&agent, url, token, "MATCH (n:mms) RETURN count(n) AS n", json!({}))?;
    eprintln!("bench: {} mms nodes", count["rows"][0][0]);
    Ok(())
}

fn run(url: &str, token: &str, case: &str, users: usize, seconds: u64) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let errors = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    let started = Instant::now();
    for user in 0..users {
        let (stop, errors) = (stop.clone(), errors.clone());
        let (url, token, case) = (url.to_owned(), token.to_owned(), case.to_owned());
        handles.push(std::thread::spawn(move || {
            let agent = agent();
            let mut latencies: Vec<u32> = Vec::new();
            let mut i = user as u64 * 7919;
            while !stop.load(Ordering::Relaxed) {
                let begun = Instant::now();
                let ok = match case.as_str() {
                    "health" => agent.get(format!("{url}/health")).call().is_ok_and(|r| r.status() == 200),
                    "namespaces" => agent
                        .get(format!("{url}/v1/namespaces"))
                        .header("authorization", format!("Bearer {token}"))
                        .call()
                        .is_ok_and(|r| r.status() == 200),
                    "return1" => query(&agent, &url, &token, "RETURN 1 AS one", json!({})).is_ok(),
                    "code" => {
                        i = i.wrapping_add(1);
                        query(
                            &agent,
                            &url,
                            &token,
                            "MATCH (n:mms {code: $code}) RETURN n.code, n.title",
                            json!({ "code": code(i % 100_000) }),
                        )
                        .is_ok()
                    }
                    other => panic!("unknown case {other}"),
                };
                if ok {
                    latencies.push(begun.elapsed().as_micros() as u32);
                } else {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            latencies
        }));
    }
    std::thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    let mut all: Vec<u32> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    let elapsed = started.elapsed().as_secs_f64();
    all.sort_unstable();
    let pct = |q: f64| -> f64 {
        if all.is_empty() {
            0.0
        } else {
            all[((all.len() - 1) as f64 * q) as usize] as f64 / 1000.0
        }
    };
    println!(
        "{}",
        json!({
            "case": case,
            "users": users,
            "requests": all.len(),
            "errors": errors.load(Ordering::Relaxed),
            "rps": (all.len() as f64 / elapsed).round(),
            "p50_ms": pct(0.50),
            "p95_ms": pct(0.95),
            "p99_ms": pct(0.99),
        })
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["setup", url, token, nodes] => setup(url, token, nodes.parse().expect("nodes")),
        ["run", url, token, case, users, seconds] => run(
            url,
            token,
            case,
            users.parse().expect("users"),
            seconds.parse().expect("seconds"),
        ),
        _ => Err("usage: http_load setup <url> <token> <nodes> | run <url> <token> <case> <users> <seconds>".into()),
    };
    if let Err(error) = result {
        eprintln!("http_load: {error}");
        std::process::exit(1);
    }
}
