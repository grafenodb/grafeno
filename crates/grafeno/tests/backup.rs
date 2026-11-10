//! Online backups: a consistent copy while writes go on.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use grafeno::{Grafeno, IndexDef, NewEdge, NewNode, NodeId, Principal, Timestamp};

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("grafeno-{name}-{}.redb", NodeId::new()))
}

/// The export of a database without its header line (which has a time).
fn content(db: &Grafeno) -> String {
    let mut out = Vec::new();
    db.begin_read().unwrap().export(&[], &mut out).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_backup_is_the_snapshot_while_writes_go_on() {
    let source = temp("backup-source");
    let db = Arc::new(Grafeno::open(&source).unwrap());
    db.write(|tx| {
        let mut previous = None;
        for i in 0..300 {
            let id = tx.create_node(
                "vault",
                NewNode::new("Note")
                    .prop("i", i)
                    .text(format!("note {i} about graphs"))
                    .embedding(vec![(i as f32).cos(), (i as f32).sin(), 1.0]),
            )?;
            if let Some(previous) = previous {
                tx.create_edge("vault", NewEdge::new(previous, "next", id))?;
            }
            previous = Some(id);
        }
        tx.create_index(
            "vault",
            IndexDef {
                name: "Note_i".into(),
                label: "Note".into(),
                key: "i".into(),
                unique: false,
                fuzzy: false,
                ..IndexDef::default()
            },
        )?;
        tx.create_namespace("empty", Some("still here"))?;
        let (account, key) = Principal {
            name: "agent".into(),
            secret_hash: "00".repeat(32),
            grants: Vec::new(),
            owner: true,
            own_only: false,
            description: None,
            created_at: Timestamp::now(),
        }
        .split();
        tx.create_account(&account)?;
        tx.create_api_key(&key)
    })
    .unwrap();

    let snapshot = db.begin_read().unwrap();
    let expected = content(&db);
    // Writes keep coming while the backup runs.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                db.write(|tx| tx.create_node("vault", NewNode::new("Late").prop("n", n)))
                    .unwrap();
                n += 1;
            }
            n
        })
    };
    let target = temp("backup-copy");
    let stats = snapshot.backup(&target).unwrap();
    stop.store(true, Ordering::Relaxed);
    let late = writer.join().unwrap();
    assert!(stats.entries > 1_000 && stats.bytes > 0, "{stats:?}");
    assert!(!target.with_extension("partial").exists());

    let copy = Grafeno::open(&target).unwrap();
    assert_eq!(
        content(&copy),
        expected,
        "the snapshot, without the {late} later writes"
    );
    let read = copy.begin_read().unwrap();
    for ns in ["vault", "empty"] {
        assert!(read.check(ns).unwrap().is_ok(), "{ns}");
    }
    assert_eq!(read.search_text("vault", "graphs", 5).unwrap().len(), 5);
    assert_eq!(
        read.search_vector("vault", &[1.0, 0.0, 1.0], 3)
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        (
            read.accounts().unwrap().len(),
            read.api_keys().unwrap().len()
        ),
        (1, 1),
        "accounts and keys are backed up too"
    );
    assert_eq!(
        read.namespace("empty")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("still here")
    );

    // A backup never overwrites a file.
    assert!(db.begin_read().unwrap().backup(&target).is_err());
    drop((copy, read, snapshot, db));
    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&target);
}

#[test]
fn restore_preserves_the_backup_and_recovers_an_independent_database() {
    let files = tempfile::tempdir().unwrap();
    let source = files.path().join("activa.redb");
    let backup = files.path().join("copia íntegra.redb");
    let restored = files.path().join("memoria recuperada.redb");
    let db = Grafeno::open(&source).unwrap();
    let id = db
        .write(|tx| {
            tx.create_node(
                "lab",
                NewNode::new("Person")
                    .prop("name", "Ana")
                    .text("memoria de grafos"),
            )
        })
        .unwrap();
    db.begin_read().unwrap().backup(&backup).unwrap();
    let original = std::fs::read(&backup).unwrap();
    let expected = content(&db);
    assert!(Grafeno::recover_backup(&backup, &restored).unwrap() > 0);
    assert_eq!(std::fs::read(&backup).unwrap(), original);
    let recovered = Grafeno::open(&restored).unwrap();
    assert_eq!(content(&recovered), expected);
    assert_eq!(
        recovered
            .begin_read()
            .unwrap()
            .search_text("lab", "grafos", 5)
            .unwrap()[0]
            .node,
        id
    );
    recovered
        .write(|tx| tx.create_node("lab", NewNode::new("OnlyRecovered")))
        .unwrap();
    assert_eq!(content(&db), expected);
    assert_eq!(std::fs::read(&backup).unwrap(), original);
}

#[test]
fn backup_preserves_existing_destinations_and_legacy_partial_files() {
    let files = tempfile::tempdir().unwrap();
    let active = files.path().join("activa.redb");
    let db = Grafeno::open(&active).unwrap();
    let target = files.path().join("copia.redb");
    let legacy = target.with_extension("partial");
    std::fs::write(&legacy, b"archivo ajeno").unwrap();
    db.begin_read().unwrap().backup(&target).unwrap();
    let original = std::fs::read(&target).unwrap();
    assert!(db.begin_read().unwrap().backup(&target).is_err());
    assert!(db.begin_read().unwrap().backup(&active).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), original);
    assert_eq!(std::fs::read(&legacy).unwrap(), b"archivo ajeno");
    assert!(
        db.write(|tx| tx.create_node("lab", NewNode::new("StillUsable")))
            .is_ok()
    );
}

#[test]
fn concurrent_backups_to_one_destination_have_exactly_one_winner() {
    let files = tempfile::tempdir().unwrap();
    let db = Arc::new(Grafeno::in_memory().unwrap());
    let target = files.path().join("concurrente.redb");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (db, target, barrier) = (db.clone(), target.clone(), barrier.clone());
            std::thread::spawn(move || {
                let read = db.begin_read().unwrap();
                barrier.wait();
                read.backup(&target)
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let recovered = Grafeno::open(&target).unwrap();
    assert!(
        recovered
            .begin_read()
            .unwrap()
            .namespaces()
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 1);
}

#[test]
fn invalid_or_missing_backups_leave_no_destination_or_temporary_files() {
    let files = tempfile::tempdir().unwrap();
    let source = files.path().join("invalida.redb");
    let target = files.path().join("recuperada.redb");
    for bytes in [b"".as_slice(), b"no es una base", &[0; 256]] {
        std::fs::write(&source, bytes).unwrap();
        assert!(Grafeno::recover_backup(&source, &target).is_err());
        assert!(!target.exists());
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 1);
    }
    assert!(Grafeno::recover_backup(&files.path().join("ausente.redb"), &target).is_err());
    assert!(!target.exists());
    assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 1);
}

#[test]
fn recovery_refuses_existing_files_and_directories_without_changes() {
    let files = tempfile::tempdir().unwrap();
    let source = files.path().join("copia.redb");
    let db = Grafeno::in_memory().unwrap();
    db.begin_read().unwrap().backup(&source).unwrap();
    let original = std::fs::read(&source).unwrap();
    assert!(Grafeno::recover_backup(&source, &source).is_err());
    let target = files.path().join("existente.redb");
    std::fs::write(&target, b"no borrar").unwrap();
    assert!(Grafeno::recover_backup(&source, &target).is_err());
    assert!(Grafeno::recover_backup(&source, files.path()).is_err());
    assert!(Grafeno::recover_backup(&source, &files.path().join("ausente/copia.redb")).is_err());
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert_eq!(std::fs::read(&target).unwrap(), b"no borrar");
    assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 2);
}

#[cfg(unix)]
#[test]
fn backup_and_recovery_refuse_symbolic_and_hard_links_even_if_dangling() {
    use std::os::unix::fs::symlink;
    let files = tempfile::tempdir().unwrap();
    let active = files.path().join("activa.redb");
    let db = Grafeno::open(&active).unwrap();
    let backup = files.path().join("copia.redb");
    db.begin_read().unwrap().backup(&backup).unwrap();
    let original = std::fs::read(&backup).unwrap();
    let hard = files.path().join("duro.redb");
    std::fs::hard_link(&active, &hard).unwrap();
    let symbolic = files.path().join("simbolico.redb");
    symlink(&active, &symbolic).unwrap();
    let missing = files.path().join("no-existe.redb");
    let dangling = files.path().join("colgante.redb");
    symlink(&missing, &dangling).unwrap();
    for path in [&hard, &symbolic, &dangling] {
        assert!(db.begin_read().unwrap().backup(path).is_err());
        assert!(Grafeno::recover_backup(&backup, path).is_err());
    }
    assert_eq!(std::fs::read(&backup).unwrap(), original);
    assert!(!missing.exists());
    assert!(std::fs::symlink_metadata(&dangling).unwrap().is_symlink());
    assert!(
        db.write(|tx| tx.create_node("lab", NewNode::new("Preserved")))
            .is_ok()
    );
}

#[test]
fn truncated_backups_and_unrelated_redb_files_are_not_published() {
    let files = tempfile::tempdir().unwrap();
    let source = files.path().join("copia.redb");
    let target = files.path().join("recuperada.redb");
    let db = Grafeno::in_memory().unwrap();
    db.begin_read().unwrap().backup(&source).unwrap();
    let mut bytes = std::fs::read(&source).unwrap();
    bytes.truncate(bytes.len() / 2);
    std::fs::write(&source, &bytes).unwrap();
    assert!(Grafeno::recover_backup(&source, &target).is_err());
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    assert!(!target.exists());
    let unrelated = files.path().join("ajena.redb");
    drop(redb::Database::create(&unrelated).unwrap());
    let original = std::fs::read(&unrelated).unwrap();
    assert!(Grafeno::recover_backup(&unrelated, &target).is_err());
    assert_eq!(std::fs::read(&unrelated).unwrap(), original);
    assert!(!target.exists());
    assert_eq!(std::fs::read_dir(files.path()).unwrap().count(), 2);
}
