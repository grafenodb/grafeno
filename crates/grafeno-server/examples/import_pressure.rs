//! Large loads while others work: a server in this process on a database
//! file, an agent writing a memory every 20 ms and a reader looking a node
//! up every 20 ms, while one load runs at a time: a JSON Lines import in
//! bulk mode, a GQ `CALL { … } IN TRANSACTIONS`, a CSV import, streaming
//! ingestion, and (for contrast) one GQ statement in one transaction.
//!
//! `cargo run --release -p grafeno-server --example import_pressure -- 2000000`
//! (nodes and as many edges for the JSON import; the others take a
//! quarter of it, the single statement a tenth; `CSV=path` writes the
//! rows, `ONLY=json,gq,csv,ingest,one` picks loads)

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use grafeno::{Grafeno, NewEdge, NewNode, NodeId};
use grafeno_server::http::{AppState, router};
use serde_json::{Value, json};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3600)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn post(agent: &ureq::Agent, url: &str, body: Vec<u8>, kind: &str) -> (u16, Value) {
    let mut response = agent
        .post(url)
        .header("content-type", kind)
        .send(&body[..])
        .unwrap();
    let status = response.status().as_u16();
    let value = response.body_mut().read_json().unwrap_or(Value::Null);
    (status, value)
}

fn post_json(agent: &ureq::Agent, url: &str, body: Value) -> (u16, Value) {
    post(
        agent,
        url,
        body.to_string().into_bytes(),
        "application/json",
    )
}

/// Lines of an export with `nodes` Reading nodes, each with an edge to the
/// one before: the record shapes are taken from a real export.
fn export_lines(start: usize, nodes: usize) -> Vec<u8> {
    let sample = Grafeno::in_memory().unwrap();
    sample
        .write(|tx| {
            let a = tx.create_node("load", NewNode::new("Reading").prop("i", 0))?;
            let b = tx.create_node("load", NewNode::new("Reading").prop("i", 1))?;
            tx.create_edge("load", NewEdge::new(a, "next", b))?;
            Ok(())
        })
        .unwrap();
    let mut dump = Vec::new();
    sample
        .begin_read()
        .unwrap()
        .export(&["load".to_string()], &mut dump)
        .unwrap();
    let records: Vec<Value> = dump
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let node = records
        .iter()
        .find(|r| r["type"] == "node")
        .unwrap()
        .clone();
    let edge = records
        .iter()
        .find(|r| r["type"] == "edge")
        .unwrap()
        .clone();
    let mut out = Vec::new();
    for record in records
        .iter()
        .filter(|r| r["type"] != "node" && r["type"] != "edge")
    {
        out.extend(record.to_string().into_bytes());
        out.push(b'\n');
    }
    let mut previous: Option<String> = None;
    for i in start..start + nodes {
        let id = NodeId::new().to_string();
        let mut line = node.clone();
        line["node"]["id"] = json!(id);
        line["node"]["props"] = json!({ "i": i });
        out.extend(line.to_string().into_bytes());
        out.push(b'\n');
        if let Some(previous) = previous {
            let mut line = edge.clone();
            line["edge"]["id"] = json!(grafeno::EdgeId::new().to_string());
            line["edge"]["from"] = json!(previous);
            line["edge"]["to"] = json!(id);
            out.extend(line.to_string().into_bytes());
            out.push(b'\n');
        }
        previous = Some(id);
    }
    out
}

fn summary(times: &mut [f64]) -> (f64, f64, f64) {
    if times.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    times.sort_by(f64::total_cmp);
    let at = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize];
    (at(0.5), at(0.99), *times.last().unwrap())
}

fn main() {
    let nodes: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(2_000_000);
    let only: Option<Vec<String>> = std::env::var("ONLY")
        .ok()
        .map(|list| list.split(',').map(str::to_owned).collect());
    let wanted = |name: &str| {
        only.as_ref()
            .is_none_or(|only| only.iter().any(|o| o == name))
    };
    let path = std::env::temp_dir().join(format!("grafeno-pressure-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Arc::new(Grafeno::open(&path).unwrap());
    // What the reader looks up: 10,000 indexed nodes.
    db.write(|tx| {
        for i in 0..10_000i64 {
            tx.create_node("read", NewNode::new("Item").prop("code", i))?;
        }
        tx.create_index("read", grafeno::IndexDef::new("Item", "code"))?;
        Ok(())
    })
    .unwrap();
    let state = AppState::new(db.clone(), None);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, router(state)).await.unwrap();
            });
    });

    let mut csv = String::from(
        "load,records,seconds,records_per_second,agent_p50_ms,agent_p99_ms,agent_max_ms,reader_p50_ms,reader_p99_ms,reader_max_ms\n",
    );
    println!(
        "| load | records | seconds | records/s | agent p50 / p99 / max ms | reader p50 / p99 / max ms |"
    );
    println!("| --- | ---: | ---: | ---: | --- | --- |");
    let mut run = |name: &str, records: usize, load: &dyn Fn() -> ()| {
        let stop = Arc::new(AtomicBool::new(false));
        let agent_thread = {
            let (stop, url) = (stop.clone(), url.clone());
            std::thread::spawn(move || {
                let client = agent();
                let mut times = Vec::new();
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    let started = Instant::now();
                    let (status, _) = post_json(
                        &client,
                        &format!("{url}/v1/agent/nodes"),
                        json!({"labels": ["Memory"], "props": {"n": n}}),
                    );
                    assert_eq!(status, 201);
                    times.push(started.elapsed().as_secs_f64() * 1000.0);
                    std::thread::sleep(Duration::from_millis(20));
                }
                times
            })
        };
        let reader_thread = {
            let (stop, url) = (stop.clone(), url.clone());
            std::thread::spawn(move || {
                let client = agent();
                let mut times = Vec::new();
                let mut n = 0i64;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    let started = Instant::now();
                    let (status, _) = post_json(
                        &client,
                        &format!("{url}/v1/read/query"),
                        json!({"query": "MATCH (i:Item {code: $code}) RETURN i.code", "params": {"code": n % 10_000}}),
                    );
                    assert_eq!(status, 200);
                    times.push(started.elapsed().as_secs_f64() * 1000.0);
                    std::thread::sleep(Duration::from_millis(20));
                }
                times
            })
        };
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        load();
        let seconds = started.elapsed().as_secs_f64();
        stop.store(true, Ordering::Relaxed);
        let (a50, a99, amax) = summary(&mut agent_thread.join().unwrap());
        let (r50, r99, rmax) = summary(&mut reader_thread.join().unwrap());
        let rate = if records == 0 {
            0.0
        } else {
            records as f64 / seconds
        };
        println!(
            "| {name} | {records} | {seconds:.1} | {rate:.0} | {a50:.1} / {a99:.1} / {amax:.0} | {r50:.1} / {r99:.1} / {rmax:.0} |"
        );
        std::io::stdout().flush().unwrap();
        csv.push_str(&format!(
            "\"{name}\",{records},{seconds:.2},{rate:.0},{a50:.2},{a99:.2},{amax:.1},{r50:.2},{r99:.2},{rmax:.1}\n"
        ));
    };

    run("none (agent and reader alone)", 0, &|| {
        std::thread::sleep(Duration::from_secs(5))
    });
    if wanted("json") {
        // In parts of 500,000 nodes (an import body holds at most 1 GiB).
        let parts: Vec<Vec<u8>> = (0..nodes.div_ceil(500_000))
            .map(|part| export_lines(part * 500_000, 500_000.min(nodes - part * 500_000)))
            .collect();
        run("JSON Lines import, bulk", nodes * 2 - 1, &|| {
            let client = agent();
            for part in &parts {
                let (status, answer) = post(
                    &client,
                    &format!("{url}/v1/import?bulk=true"),
                    part.clone(),
                    "application/x-ndjson",
                );
                assert_eq!(status, 200, "{answer}");
            }
        });
    }
    let quarter = nodes / 4;
    if wanted("gq") {
        run(
            "GQ CALL { … } IN TRANSACTIONS OF 2000 ROWS",
            quarter,
            &|| {
                let (status, answer) = post_json(
                    &agent(),
                    &format!("{url}/v1/gq/query"),
                    json!({"query": format!("UNWIND range(1, {quarter}) AS i CALL {{ WITH i CREATE (:Reading {{i: i}}) }} IN TRANSACTIONS OF 2000 ROWS")}),
                );
                assert_eq!(status, 200, "{answer}");
            },
        );
    }
    if wanted("csv") {
        let mut body = String::from("id,i:int\n");
        for i in 0..quarter {
            body.push_str(&format!("r{i},{i}\n"));
        }
        run("CSV import of nodes", quarter, &|| {
            let (status, answer) = post(
                &agent(),
                &format!("{url}/v1/csv/import/nodes.csv"),
                body.clone().into_bytes(),
                "text/csv",
            );
            assert_eq!(status, 200, "{answer}");
        });
    }
    if wanted("ingest") {
        let mut body = String::new();
        for i in 0..quarter {
            body.push_str(&json!({"node": {"labels": ["Reading"], "props": {"i": i}}}).to_string());
            body.push('\n');
        }
        run("streaming ingestion (NDJSON)", quarter, &|| {
            let (status, answer) = post(
                &agent(),
                &format!("{url}/v1/ingest/ingest"),
                body.clone().into_bytes(),
                "application/x-ndjson",
            );
            assert_eq!(status, 200, "{answer}");
        });
    }
    if wanted("one") {
        let tenth = nodes / 10;
        run("GQ, one statement in one transaction", tenth, &|| {
            let (status, answer) = post_json(
                &agent(),
                &format!("{url}/v1/one/query"),
                json!({"query": format!("UNWIND range(1, {tenth}) AS i CREATE (:Reading {{i: i}})")}),
            );
            assert_eq!(status, 200, "{answer}");
        });
    }
    if let Ok(file) = std::env::var("CSV") {
        std::fs::write(file, csv).unwrap();
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}
