//! Storage migrations: planned without changes, run in batches that record
//! how far they got, resumed after an interruption with the same result as
//! a run without one, and a finished one changes nothing when run again.

use grafeno::{
    Encoding, Grafeno, HyperEdgeId, IndexDef, MigrationOptions, MigrationState, NewEdge, NewNode,
    RunControl, StepKind,
};

fn sample(path: &std::path::Path) -> Grafeno {
    let db = Grafeno::open(path).unwrap();
    db.write(|tx| {
        tx.create_index("a", IndexDef::new("Item", "code"))?;
        let mut previous = None;
        for i in 0..2_500i64 {
            let id = tx.create_node(
                "a",
                NewNode::new("Item")
                    .prop("code", format!("C{i}"))
                    .prop("tags", serde_json::json!(["x", i % 3]))
                    .text(format!("item {i} of the catalogue")),
            )?;
            if let Some(previous) = previous {
                tx.create_edge("a", NewEdge::new(previous, "next", id).prop("w", i))?;
            }
            previous = Some(id);
        }
        tx.create_node("b", NewNode::new("Other").prop("n", 1))?;
        Ok(())
    })
    .unwrap();
    db
}

fn digests(db: &Grafeno) -> Vec<grafeno::NamespaceDigest> {
    let read = db.begin_read().unwrap();
    ["a", "b"]
        .iter()
        .map(|ns| read.namespace_digest(ns).unwrap())
        .collect()
}

fn options(encoding: Encoding) -> MigrationOptions {
    MigrationOptions {
        encoding: Some(encoding),
        reindex: true,
        verify: true,
        batch_size: 300,
        ..MigrationOptions::default()
    }
}

#[test]
fn a_dry_run_changes_nothing_and_says_what_would() {
    let dir = tempfile::tempdir().unwrap();
    let db = sample(&dir.path().join("plan.redb"));
    let before = digests(&db);
    let plan = db.plan_migration(&options(Encoding::Envelope)).unwrap();
    assert_eq!(digests(&db), before);
    assert!(db.begin_read().unwrap().migrations().unwrap().is_empty());
    let reencode = plan
        .steps
        .iter()
        .find(|s| s.kind == StepKind::Reencode && s.namespace == "a")
        .unwrap();
    // 2,500 nodes and 2,499 edges are all in JSON.
    assert_eq!(reencode.estimate, 4_999);
    assert_eq!(plan.before[0].nodes, 2_500);
    // Unknown namespaces are refused.
    let missing = MigrationOptions {
        namespaces: vec!["nowhere".into()],
        ..MigrationOptions::default()
    };
    assert!(db.plan_migration(&missing).is_err());
}

#[test]
fn an_interrupted_migration_resumes_to_the_same_result() {
    let dir = tempfile::tempdir().unwrap();
    let whole = sample(&dir.path().join("whole.redb"));
    let cut = sample(&dir.path().join("cut.redb"));
    let exclusive = RunControl {
        exclusive: true,
        ..RunControl::default()
    };

    let plan = whole.plan_migration(&options(Encoding::Envelope)).unwrap();
    whole.start_migration(&plan).unwrap();
    let done = whole.run_migration(&plan.id, &exclusive).unwrap();
    assert_eq!(done.state, MigrationState::Completed, "{done:?}");

    let plan = cut.plan_migration(&options(Encoding::Envelope)).unwrap();
    cut.start_migration(&plan).unwrap();
    let before = digests(&cut);
    // Stopped after three batches, then twice more: each time it goes on
    // from its checkpoint.
    let mut paused = cut
        .run_migration(
            &plan.id,
            &RunControl {
                max_batches: Some(3),
                exclusive: true,
                ..RunControl::default()
            },
        )
        .unwrap();
    assert_eq!(paused.state, MigrationState::Paused);
    // The reindex of `a`, then two batches of 300 records.
    let step = &paused.steps[1];
    assert_eq!(step.kind, StepKind::Reencode);
    assert_eq!(step.state, MigrationState::InProgress);
    assert_eq!(step.done, 600);
    // A second migration waits for this one.
    assert!(
        cut.start_migration(&cut.plan_migration(&options(Encoding::Json)).unwrap())
            .is_err()
    );
    paused = cut
        .run_migration(
            &plan.id,
            &RunControl {
                max_batches: Some(5),
                exclusive: true,
                ..RunControl::default()
            },
        )
        .unwrap();
    assert_eq!(paused.state, MigrationState::Paused);
    let done = cut.run_migration(&plan.id, &exclusive).unwrap();
    assert_eq!(done.state, MigrationState::Completed, "{done:?}");
    let a = done
        .steps
        .iter()
        .find(|s| s.kind == StepKind::Reencode && s.namespace == "a")
        .unwrap();
    // Every record rewritten once, none twice.
    assert_eq!(a.done, 4_999);
    assert!(
        done.steps
            .iter()
            .all(|s| s.state == MigrationState::Completed)
    );
    // The same values as before and as the uninterrupted run.
    assert_eq!(digests(&cut), before);
    let check = |db: &Grafeno| {
        let read = db.begin_read().unwrap();
        ["a", "b"].iter().all(|ns| read.check(ns).unwrap().is_ok())
    };
    assert!(check(&cut) && check(&whole));

    // Finished: running it again changes nothing.
    let again = cut.run_migration(&plan.id, &exclusive).unwrap();
    assert_eq!(again, done);
    // A new plan finds nothing left to rewrite.
    let next = cut.plan_migration(&options(Encoding::Envelope)).unwrap();
    assert!(
        next.steps
            .iter()
            .filter(|s| s.kind == StepKind::Reencode)
            .all(|s| s.estimate == 0)
    );
}

#[test]
fn records_go_back_to_json_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("back.redb");
    let db = sample(&path);
    let before = digests(&db);
    let exclusive = RunControl {
        exclusive: true,
        ..RunControl::default()
    };
    for encoding in [Encoding::Envelope, Encoding::Json] {
        let plan = db.plan_migration(&options(encoding)).unwrap();
        db.start_migration(&plan).unwrap();
        let done = db.run_migration(&plan.id, &exclusive).unwrap();
        assert_eq!(done.state, MigrationState::Completed, "{done:?}");
        assert_eq!(digests(&db), before);
    }
    // Reads, traversals and lookups as before.
    let read = db.begin_read().unwrap();
    assert_eq!(read.stats("a").unwrap().nodes, 2_500);
    let found = read
        .index_lookup(
            "a",
            "Item_code",
            &grafeno::IndexRange::Equal(serde_json::json!("C7")),
            5,
        )
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        read.neighbors(
            "a",
            found[0],
            grafeno::Direction::Both,
            &grafeno::EdgeFilter::any()
        )
        .unwrap()
        .len(),
        2
    );
    let _ = HyperEdgeId::new();
    assert_eq!(read.migrations().unwrap().len(), 2);
}

#[test]
fn verifying_catches_a_change_to_the_data() {
    let dir = tempfile::tempdir().unwrap();
    let db = sample(&dir.path().join("verify.redb"));
    let plan = db
        .plan_migration(&MigrationOptions {
            verify: true,
            ..MigrationOptions::default()
        })
        .unwrap();
    db.start_migration(&plan).unwrap();
    // Something changes between the plan and the run.
    db.write(|tx| tx.create_node("a", NewNode::new("Item").prop("code", "late")))
        .unwrap();
    let done = db
        .run_migration(
            &plan.id,
            &RunControl {
                exclusive: true,
                ..RunControl::default()
            },
        )
        .unwrap();
    assert_eq!(done.state, MigrationState::RollbackRequired, "{done:?}");
    assert!(
        done.steps[0]
            .error
            .as_deref()
            .unwrap()
            .contains("holds something else")
    );
}

#[test]
fn a_packed_file_has_nothing_pending() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packed.redb");
    let db = sample(&path);
    db.write(|tx| tx.drop_namespace("a").map(|_| ())).unwrap();
    drop(db);
    let db = Grafeno::open(&path).unwrap();
    assert!(
        db.pending_work()
            .unwrap()
            .iter()
            .any(|p| p.kind == "compact")
    );
    drop(db);
    Grafeno::vacuum_with(&path, true).unwrap();
    // Opening a file with no free pages grows it (redb trims that when it
    // closes it): not something a compaction gives back.
    let db = Grafeno::open(&path).unwrap();
    let pending = db.pending_work().unwrap();
    assert!(pending.is_empty(), "{pending:?}");
}
