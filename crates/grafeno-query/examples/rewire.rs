//! Rewiring edges in bulk: `n` Person nodes each visit one of 1,000
//! Paciente nodes; their edges are moved to the Prueba node of the same
//! code, keeping the old ones as history (ended, not deleted), through the
//! writer as the server runs it, while an agent writes and a reader reads.
//!
//! `cargo run --release -p grafeno-query --example rewire -- 1000000`
//! (`ONE=200000` also times the same rewiring as one statement in one
//! transaction on that many people; `CSV=path` writes the rows; `LAPS=1`
//! prints the time of every 20 batches and each wait over a second)

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use grafeno::{Grafeno, NewEdge, NewNode, Writer, WriterOptions};
use grafeno_query::{Batch, Limits, Params, QueryError, WriteStats};

const REWIRE: &str = "MATCH (p:Person)-[r:visits]->(x:Paciente)
 CALL { WITH p, r, x
        MATCH (t:Prueba {code: x.code})
        CREATE (p)-[:visits {since: r.since}]->(t)
        SET r.@valid_to = datetime() }
 IN TRANSACTIONS OF 5000 ROWS";

const REWIRE_ONE: &str = "MATCH (p:Person)-[r:visits]->(x:Paciente)
 MATCH (t:Prueba {code: x.code})
 CREATE (p)-[:visits {since: r.since}]->(t)
 SET r.@valid_to = datetime()";

fn build(people: usize) -> (Arc<Grafeno>, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "grafeno-rewire-{}-{people}.redb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = Arc::new(Grafeno::open(&path).unwrap());
    let targets = db
        .write(|tx| {
            let mut pacientes = Vec::new();
            for code in 0..1_000i64 {
                pacientes.push(tx.create_node("h", NewNode::new("Paciente").prop("code", code))?);
                tx.create_node("h", NewNode::new("Prueba").prop("code", code))?;
            }
            tx.create_index("h", grafeno::IndexDef::new("Prueba", "code"))?;
            Ok(pacientes)
        })
        .unwrap();
    for chunk in 0..people.div_ceil(50_000) {
        db.write(|tx| {
            for i in chunk * 50_000..((chunk + 1) * 50_000).min(people) {
                let person = tx.create_node("h", NewNode::new("Person").prop("i", i as i64))?;
                tx.create_edge(
                    "h",
                    NewEdge::new(person, "visits", targets[i % 1_000]).prop("since", i as i64),
                )?;
            }
            Ok(())
        })
        .unwrap();
    }
    (db, path)
}

/// Latencies of an agent writing a memory every 20 ms and a reader reading
/// every 20 ms, while `load` runs.
fn under_load(
    db: &Arc<Grafeno>,
    writer: &Arc<Writer>,
    load: impl FnOnce() -> f64,
) -> (f64, Vec<f64>, Vec<f64>) {
    let stop = Arc::new(AtomicBool::new(false));
    let agent = {
        let (writer, stop) = (writer.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut times = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let started = Instant::now();
                writer
                    .write(|tx| tx.create_node("agent", NewNode::new("Memory").prop("n", 1)))
                    .unwrap();
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                if ms > 1000.0 && std::env::var_os("LAPS").is_some() {
                    eprintln!(
                        "agent waited {ms:.0} ms at {:?}",
                        std::time::SystemTime::now()
                    );
                }
                times.push(ms);
                std::thread::sleep(Duration::from_millis(20));
            }
            times
        })
    };
    let reader = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut times = Vec::new();
            let mut i = 0i64;
            while !stop.load(Ordering::Relaxed) {
                i += 1;
                let started = Instant::now();
                let mut params = Params::new();
                params.insert("code".into(), grafeno_query::Value::Int(i % 1_000));
                grafeno_query::execute_read(
                    &db.begin_read().unwrap(),
                    "h",
                    "MATCH (t:Prueba {code: $code}) RETURN t.code",
                    &params,
                )
                .unwrap();
                times.push(started.elapsed().as_secs_f64() * 1000.0);
                std::thread::sleep(Duration::from_millis(20));
            }
            times
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    let seconds = load();
    stop.store(true, Ordering::Relaxed);
    (seconds, agent.join().unwrap(), reader.join().unwrap())
}

fn summary(times: &mut [f64]) -> (f64, f64, f64) {
    times.sort_by(f64::total_cmp);
    let at = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize];
    (at(0.5), at(0.99), *times.last().unwrap())
}

fn main() {
    let people: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(1_000_000);
    let mut csv = String::from(
        "rewiring,people,seconds,edges_per_second,agent_p50_ms,agent_p99_ms,agent_max_ms,reader_p50_ms,reader_p99_ms,reader_max_ms\n",
    );
    println!(
        "| rewiring | people | seconds | edges/s | agent p50 / p99 / max ms | reader p50 / p99 / max ms |"
    );
    println!("| --- | ---: | ---: | ---: | --- | --- |");
    let mut report =
        |name: &str, people: usize, (seconds, mut agent, mut reader): (f64, Vec<f64>, Vec<f64>)| {
            let (a50, a99, amax) = summary(&mut agent);
            let (r50, r99, rmax) = summary(&mut reader);
            println!(
                "| {name} | {people} | {seconds:.1} | {:.0} | {a50:.1} / {a99:.1} / {amax:.0} | {r50:.1} / {r99:.1} / {rmax:.0} |",
                people as f64 / seconds
            );
            std::io::stdout().flush().unwrap();
            csv.push_str(&format!(
            "\"{name}\",{people},{seconds:.2},{:.0},{a50:.2},{a99:.2},{amax:.1},{r50:.2},{r99:.2},{rmax:.1}\n",
            people as f64 / seconds
        ));
        };

    // Idle, then IN TRANSACTIONS through the bulk lane.
    let (db, path) = build(people);
    let writer = Arc::new(Writer::new(db.clone(), WriterOptions::default()));
    report(
        "none (agent and reader alone)",
        0,
        under_load(&db, &writer, || {
            std::thread::sleep(Duration::from_secs(5));
            5.0
        }),
    );
    let result = under_load(&db, &writer, || {
        let started = Instant::now();
        let read = db.begin_read().unwrap();
        let mut batches = 0usize;
        let mut lap = Instant::now();
        let mut commit = |batch: Batch| -> Result<WriteStats, QueryError> {
            batches += 1;
            if std::env::var_os("LAPS").is_some() && batches % 20 == 0 {
                eprintln!(
                    "batches {batches}: {:.2} s for the last 20",
                    lap.elapsed().as_secs_f64()
                );
                lap = Instant::now();
            }
            let db = db.clone();
            writer
                .write_bulk(move |tx| {
                    let read = db.begin_read()?;
                    batch
                        .run(tx, &read)
                        .map_err(|e| grafeno::Error::Invalid(e.to_string()))
                })
                .map_err(QueryError::from)
        };
        let done = grafeno_query::execute_in_transactions(
            read,
            "h",
            REWIRE,
            &Params::new(),
            &Limits::standard(),
            &mut commit,
        )
        .unwrap();
        let flushing = Instant::now();
        writer.flush().unwrap();
        if std::env::var_os("LAPS").is_some() {
            eprintln!(
                "flush {:.1} s at {:?}",
                flushing.elapsed().as_secs_f64(),
                std::time::SystemTime::now()
            );
        }
        assert_eq!(done.stats.edges_created as usize, people);
        started.elapsed().as_secs_f64()
    });
    report("IN TRANSACTIONS OF 5000 ROWS, bulk lane", people, result);
    let check = grafeno_query::execute_read(
        &db.begin_read().unwrap(),
        "h",
        "MATCH (:Person)-[:visits]->(:Paciente) VALID AT datetime() RETURN count(*)",
        &Params::new(),
    )
    .unwrap();
    assert_eq!(check.rows[0][0], grafeno_query::Value::Int(0));
    drop(writer);
    drop(db);
    let _ = std::fs::remove_file(&path);

    // The same as one statement, one transaction, in the interactive lane.
    if let Some(one) = std::env::var("ONE")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
    {
        let (db, path) = build(one);
        let writer = Arc::new(Writer::new(db.clone(), WriterOptions::default()));
        let result = under_load(&db, &writer, || {
            let started = Instant::now();
            let db2 = db.clone();
            writer
                .write(move |tx| {
                    let read = db2.begin_read()?;
                    grafeno_query::execute_in(
                        tx,
                        &read,
                        "h",
                        REWIRE_ONE,
                        &Params::new(),
                        &Limits::none(),
                    )
                    .map_err(|e| grafeno::Error::Invalid(e.to_string()))
                })
                .unwrap();
            started.elapsed().as_secs_f64()
        });
        report("one statement, one transaction", one, result);
        drop(writer);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(file) = std::env::var("CSV") {
        std::fs::write(file, csv).unwrap();
    }
}
