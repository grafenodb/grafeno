//! Crashes in the middle of long operations: a child process runs a bulk
//! import, a one-transaction import, index rebuilds, namespace drops or a
//! layout upgrade, and is killed at a random moment. Reopening must find
//! every committed transaction whole, nothing half-done, and consistent
//! indexes.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use grafeno::{Grafeno, ImportOptions, NewEdge, NewNode, NodeId};

const MODE: &str = "GRAFENO_CRASH_MODE";
const DB: &str = "GRAFENO_CRASH_DB";
const INPUT: &str = "GRAFENO_CRASH_INPUT";
const NS: &str = "crash";
/// Nodes in the dump the import modes load.
const NODES: u64 = 6_000;

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("grafeno-crash-{name}-{}", NodeId::new()))
}

/// A small random number, without a crate: the clock's nanoseconds.
fn random_below(n: u64) -> u64 {
    let nanos = Instant::now().elapsed().as_nanos() as u64
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
    let mut x = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    x ^= x >> 29;
    x % n.max(1)
}

/// The child: runs `mode` on the database until it is killed.
#[test]
#[ignore = "run by the crash tests in a child process"]
fn crash_child() {
    let (Ok(mode), Ok(path)) = (std::env::var(MODE), std::env::var(DB)) else {
        return;
    };
    let path = PathBuf::from(path);
    let input = std::env::var(INPUT).map(PathBuf::from).ok();
    println!("ready");
    match mode.as_str() {
        "bulk" | "import" => {
            let db = Grafeno::open(&path).unwrap();
            let file = std::io::BufReader::new(std::fs::File::open(input.unwrap()).unwrap());
            let options = if mode == "bulk" {
                ImportOptions { chunk: Some(500) }
            } else {
                ImportOptions::default()
            };
            db.import_with(file, &options).unwrap();
            println!("done");
        }
        "rebuild" => {
            let db = Grafeno::open(&path).unwrap();
            loop {
                db.write(|tx| tx.rebuild_indexes(NS)).unwrap();
                println!("rebuilt");
            }
        }
        "drop" => {
            let db = Grafeno::open(&path).unwrap();
            loop {
                db.write(|tx| fill(tx, "temp", 400)).unwrap();
                db.write(|tx| tx.drop_namespace("temp")).unwrap();
                println!("dropped");
            }
        }
        "migrate" => {
            let db = Grafeno::open(&path).unwrap();
            let id = db
                .begin_read()
                .unwrap()
                .migrations()
                .unwrap()
                .last()
                .unwrap()
                .id
                .clone();
            db.run_migration(&id, &grafeno::RunControl::default())
                .unwrap();
            println!("done");
        }
        "upgrade" => {
            let db = Grafeno::open(&path).unwrap();
            drop(db);
            println!("done");
        }
        other => panic!("unknown mode {other}"),
    }
}

/// `count` nodes with text and embeddings, chained by edges.
fn fill(tx: &mut grafeno::WriteTx, ns: &str, count: u64) -> grafeno::Result<()> {
    let mut previous = None;
    for i in 0..count {
        let id = tx.create_node(
            ns,
            NewNode::new("Step")
                .prop("i", i)
                .text(format!("step {i} of the crash test with words{}", i % 13))
                .embedding(vec![(i as f32).cos(), (i as f32).sin(), 1.0]),
        )?;
        if let Some(previous) = previous {
            tx.create_edge(ns, NewEdge::new(previous, "next", id))?;
        }
        previous = Some(id);
    }
    Ok(())
}

/// Runs the child in `mode` and kills it after a random delay (after it
/// says it is ready). Returns whether it finished on its own first.
fn run_and_kill(
    mode: &str,
    db: &Path,
    input: Option<&Path>,
    delay_ms: std::ops::Range<u64>,
) -> bool {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "crash_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(MODE, mode)
        .env(DB, db)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(input) = input {
        command.env(INPUT, input);
    }
    let mut child = command.spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    // Wait until the child runs the operation.
    for line in lines.by_ref() {
        if line.unwrap().ends_with("ready") {
            break;
        }
    }
    let delay = Duration::from_millis(delay_ms.start + random_below(delay_ms.end - delay_ms.start));
    let reader = std::thread::spawn(move || {
        let mut rest = String::new();
        for line in lines {
            rest.push_str(&line.unwrap_or_default());
            rest.push('\n');
        }
        rest
    });
    std::thread::sleep(delay);
    let _ = child.kill();
    child.wait().unwrap();
    let rest = reader.join().unwrap();
    rest.contains("done")
}

fn dump() -> PathBuf {
    let source = Grafeno::in_memory().unwrap();
    source.write(|tx| fill(tx, NS, NODES)).unwrap();
    let path = temp("dump.jsonl");
    let mut out = Vec::new();
    source.begin_read().unwrap().export(&[], &mut out).unwrap();
    std::fs::write(&path, out).unwrap();
    path
}

fn consistent(db: &Grafeno, ns: &str) -> (u64, u64) {
    let read = db.begin_read().unwrap();
    let report = read.check(ns).unwrap();
    assert!(report.is_ok(), "{ns}: {:#?}", report.problems);
    let stats = read.stats(ns).unwrap();
    // Every node came with its text and embedding, in the same chunk.
    assert_eq!(stats.texts, stats.nodes, "{ns}: a node without its text");
    assert_eq!(
        stats.vectors, stats.nodes,
        "{ns}: a node without its vector"
    );
    (stats.nodes, stats.edges)
}

#[test]
fn imports_survive_being_killed() {
    let input = dump();
    for round in 0..6 {
        let mode = if round % 2 == 0 { "bulk" } else { "import" };
        let db_path = temp("import.redb");
        let finished = run_and_kill(mode, &db_path, Some(&input), 300..6_000);
        let db = Grafeno::open(&db_path).unwrap();
        let (nodes, edges) = consistent(&db, NS);
        eprintln!("{mode}: {nodes} of {NODES} nodes after the crash (finished: {finished})");
        if mode == "import" {
            // One transaction: all of it or nothing.
            assert!(nodes == 0 || nodes == NODES, "{mode}: {nodes} nodes");
        }
        if finished {
            assert_eq!(nodes, NODES);
        }
        assert!(nodes <= NODES && edges < NODES);
        drop(db);
        let _ = std::fs::remove_file(&db_path);
    }
    let _ = std::fs::remove_file(&input);
}

#[test]
fn rebuilds_and_drops_survive_being_killed() {
    let db_path = temp("rebuild.redb");
    {
        let db = Grafeno::open(&db_path).unwrap();
        db.write(|tx| fill(tx, NS, 1_500)).unwrap();
    }
    for _ in 0..3 {
        run_and_kill("rebuild", &db_path, None, 50..800);
        let db = Grafeno::open(&db_path).unwrap();
        assert_eq!(consistent(&db, NS), (1_500, 1_499));
    }
    for _ in 0..4 {
        run_and_kill("drop", &db_path, None, 50..800);
        let db = Grafeno::open(&db_path).unwrap();
        assert_eq!(
            consistent(&db, NS),
            (1_500, 1_499),
            "the other namespace is untouched"
        );
        // The dropped namespace is either gone or whole: whole fills of
        // 400, as a run killed between a fill and its drop leaves one for
        // the next run to fill again.
        let temp_ns = db.begin_read().unwrap().namespace("temp").unwrap();
        eprintln!(
            "drop: temp is {:?} after the crash",
            temp_ns.as_ref().map(|info| info.nodes)
        );
        if temp_ns.is_some() {
            let (nodes, _) = consistent(&db, "temp");
            assert!(nodes % 400 == 0, "{nodes} nodes in temp");
        }
    }
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn upgrades_survive_being_killed() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/layout-v2.redb.gz");
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&source).unwrap())
        .read_to_end(&mut bytes)
        .unwrap();
    for _ in 0..5 {
        let db_path = temp("upgrade.redb");
        std::fs::write(&db_path, &bytes).unwrap();
        // The upgrade runs while the database opens: kill it then.
        run_and_kill("upgrade", &db_path, None, 0..40);
        let db = Grafeno::open(&db_path).unwrap();
        let read = db.begin_read().unwrap();
        assert!(read.check("fixture").unwrap().is_ok());
        let stats = read.stats("fixture").unwrap();
        assert_eq!((stats.nodes, stats.edges), (41, 79));
        drop((read, db));
        let _ = std::fs::remove_file(&db_path);
    }
}

#[test]
fn migrations_survive_being_killed() {
    for round in 0..6 {
        let db_path = temp("migrate.redb");
        let (before, id) = {
            let db = Grafeno::open(&db_path).unwrap();
            db.write(|tx| fill(tx, NS, 4_000)).unwrap();
            let encoding = if round % 2 == 0 {
                grafeno::Encoding::Envelope
            } else {
                grafeno::Encoding::Json
            };
            let options = grafeno::MigrationOptions {
                encoding: Some(encoding),
                reindex: true,
                verify: true,
                batch_size: 40,
                ..Default::default()
            };
            let plan = db.plan_migration(&options).unwrap();
            db.start_migration(&plan).unwrap();
            (
                db.begin_read().unwrap().namespace_digest(NS).unwrap(),
                plan.id,
            )
        };
        let finished = run_and_kill("migrate", &db_path, None, 300..2_500);
        // Reopened, it checks out, holds what it held, and resumes to the
        // end with every record as before.
        let db = Grafeno::open(&db_path).unwrap();
        assert_eq!(consistent(&db, NS), (4_000, 3_999));
        assert_eq!(
            db.begin_read().unwrap().namespace_digest(NS).unwrap(),
            before
        );
        let state = db.migration(&id).unwrap().unwrap().state;
        eprintln!("migrate: {state:?} after the crash (finished: {finished})");
        let done = db
            .run_migration(
                &id,
                &grafeno::RunControl {
                    exclusive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(done.state, grafeno::MigrationState::Completed, "{done:?}");
        assert_eq!(
            db.begin_read().unwrap().namespace_digest(NS).unwrap(),
            before
        );
        drop(db);
        let _ = std::fs::remove_file(&db_path);
    }
}
