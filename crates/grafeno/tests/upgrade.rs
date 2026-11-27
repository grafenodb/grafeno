//! Databases written by every past layout version (fixtures made with the
//! code of that version, see `tests/fixtures/make.py`) open, upgrade and
//! answer like new.

use std::io::Read;
use std::path::PathBuf;

use grafeno::{Direction, EdgeFilter, Grafeno, NewNode, NodeId, Timestamp};

/// These tests are about layouts 18 and 19: without compression, which
/// takes a file to 20 (`tests/compression.rs`).
fn open_plain(path: &std::path::Path) -> grafeno::Result<Grafeno> {
    Grafeno::open_with(
        path,
        &grafeno::OpenOptions {
            compression: grafeno::Compression::Off,
            ..grafeno::OpenOptions::default()
        },
    )
}

const NS: &str = "fixture";

/// Decompresses the fixture of `version` into a temporary file.
fn fixture(version: u64) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/layout-v{version}.redb.gz"));
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&source).unwrap())
        .read_to_end(&mut bytes)
        .unwrap();
    let path =
        std::env::temp_dir().join(format!("grafeno-upgrade-v{version}-{}.redb", NodeId::new()));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Everything a fixture holds, read back after the upgrade.
fn verify(db: &Grafeno, version: u64) {
    let read = db.begin_read().unwrap();
    let stats = read.stats(NS).unwrap();
    assert_eq!((stats.nodes, stats.edges), (41, 79), "v{version}");
    assert_eq!(stats.node_labels["Person"], 40);
    assert_eq!(stats.edge_labels["works_at"], 40);
    assert_eq!((stats.texts, stats.vectors), (41, 40));
    for ns in [NS, "other"] {
        let report = read.check(ns).unwrap();
        assert!(report.is_ok(), "v{version} {ns}: {:#?}", report.problems);
    }
    let mut names: Vec<String> = read
        .namespaces()
        .unwrap()
        .into_iter()
        .map(|info| info.name)
        .collect();
    names.sort();
    let mut expected = vec!["fixture", "other"];
    if version >= 6 {
        expected.insert(0, "empty");
    }
    assert_eq!(names, expected, "v{version}");

    // Text, vectors, names, traversals and valid time all work.
    assert_eq!(read.search_text(NS, "topic3", 50).unwrap().len(), 8);
    let acme = read.search_text(NS, "engines", 1).unwrap()[0].node;
    assert_eq!(
        read.node(NS, acme).unwrap().unwrap().explicit_name(),
        Some("Acme")
    );
    let nearest = read.search_vector(NS, &[1.0, 0.0, 1.0], 1).unwrap()[0].node;
    assert_eq!(
        read.node(NS, nearest).unwrap().unwrap().explicit_name(),
        Some("Person 0")
    );
    let seven = read.find_by_name(NS, "person 7").unwrap();
    assert_eq!(seven.len(), 1, "v{version}: the names index");
    let at = |date: &str| {
        EdgeFilter::any()
            .label("works_at")
            .valid_at(Timestamp::parse(date).unwrap())
    };
    let in_2021 = read
        .neighbors(NS, acme, Direction::In, &at("2021-06-01"))
        .unwrap();
    let in_2023 = read
        .neighbors(NS, acme, Direction::In, &at("2023-06-01"))
        .unwrap();
    assert_eq!((in_2021.len(), in_2023.len()), (40, 20), "v{version}");
    let knows = read
        .k_hop(
            NS,
            seven[0],
            2,
            Direction::Both,
            &EdgeFilter::any().label("knows"),
        )
        .unwrap();
    assert_eq!(knows.len(), 4);

    let first = read.find_by_name(NS, "person 0").unwrap()[0];
    let node = read.node(NS, first).unwrap().unwrap();
    let importance = read.memory(NS, &node).unwrap().importance;
    if version >= 4 {
        assert!((importance - 0.9).abs() < 1e-6, "v{version}: {importance}");
    }
    if version >= 5 {
        let indexes = read.indexes(NS).unwrap();
        // Layout 17 files may hold an index on several properties (16.5),
        // layout 18 files a full-text index (16.6).
        let expected = match version {
            18.. => 3,
            17 => 2,
            _ => 1,
        };
        assert_eq!(indexes.len(), expected, "v{version}");
        assert!(
            indexes
                .iter()
                .filter(|index| index.def.name != "Company_name_text")
                .all(|index| index.entries == 40),
            "v{version}"
        );
        if version >= 18 {
            let found = read
                .fulltext_search(NS, "Company_name_text", "acme", 5, false)
                .unwrap();
            assert_eq!(found.len(), 1, "v{version}");
        }
    }
    if version >= 7 {
        // Principals became service accounts with a key each (layout 15).
        let accounts = read.accounts().unwrap();
        let expected = match version {
            15.. => 2,
            14 => 3,
            _ => 1,
        };
        assert_eq!(accounts.len(), expected, "v{version}");
        let reader = read.account("reader").unwrap().expect("the reader");
        assert_eq!(reader.kind, grafeno::AccountKind::Service);
        assert!(
            reader.may("fixture", grafeno::Role::Read)
                && !reader.may("fixture", grafeno::Role::Write)
        );
        let key = read.api_key("reader").unwrap().expect("its key");
        assert_eq!(
            (key.account.as_str(), key.secret_hash.as_str()),
            ("reader", "00".repeat(32).as_str())
        );
        if version >= 15 {
            let ana = read.account("ana").unwrap().unwrap();
            assert_eq!(ana.kind, grafeno::AccountKind::User);
            assert!(ana.own_only && ana.password_hash.is_some());
            assert!(
                read.api_key("ana-ro")
                    .unwrap()
                    .unwrap()
                    .scope
                    .unwrap()
                    .read_only
            );
            assert_eq!(read.sessions(Some("ana")).unwrap().len(), 1);
            if version == 15 {
                assert!(
                    read.roles().unwrap().is_empty(),
                    "roles start empty (layout 16)"
                );
            } else {
                // A role with a denial and a record policy (14.4), held by ana
                // with her attributes.
                let analyst = read.role("analyst").unwrap().unwrap();
                assert_eq!(analyst.privileges.len(), 1);
                assert_eq!(analyst.policies.len(), 1);
                assert_eq!(ana.roles, vec!["analyst".to_owned()]);
                assert_eq!(ana.attributes["team"], "red");
            }
        } else if version >= 14 {
            assert!(read.account("admin-key").unwrap().unwrap().superuser);
            let agent = read.account("agent").unwrap().unwrap();
            assert!(agent.own_only && agent.may("fixture", grafeno::Role::Write));
            assert_eq!(read.api_keys().unwrap().len(), 3);
        }
        if version < 15 {
            assert!(read.sessions(None).unwrap().is_empty());
        }
    }
    assert!(read.merges(NS, None).unwrap().is_empty());
    // The audit log (layout 17) starts empty: the upgrade audits nothing.
    // A layout 17 file has what its own Grafeno audited.
    assert_eq!(
        read.audit(0, 10).unwrap().events.is_empty(),
        version < 17,
        "v{version}"
    );
    if version >= 11 {
        assert_eq!(read.constraints(NS).unwrap().len(), 1, "v{version}");
    }
    // Before layout 12 the change log starts empty and fills from the first
    // commit after; a layout 12 file already has the fixture's writes.
    let last = read.changes(NS, 0, 10).unwrap().last;
    if version >= 12 {
        assert!(last > 0, "v{version}");
    } else {
        assert_eq!(last, 0, "v{version}");
    }
}

#[test]
fn every_past_layout_upgrades() {
    for version in 1..=18 {
        let path = fixture(version);
        assert_eq!(Grafeno::file_layout(&path).unwrap(), Some(version));
        {
            let db = open_plain(&path).unwrap_or_else(|e| panic!("v{version}: {e}"));
            verify(&db, version);
            // No hyperedges yet (layout 19): the reads answer empty.
            let read = db.begin_read().unwrap();
            let acme = read.search_text(NS, "engines", 1).unwrap()[0].node;
            let any = grafeno::HyperEdgeFilter::any();
            assert!(read.node_hyperedges(NS, acme, &any).unwrap().is_empty());
            assert!(read.hyperedges_page(NS, None, 10).unwrap().is_empty());
            drop(read);
            // It is a normal database now: it takes writes.
            db.write(|tx| tx.create_node(NS, NewNode::new("Note").text("after the upgrade")))
                .unwrap();
        }
        // The file as it was is kept beside it, for the previous version;
        // a layout 18 file is not upgraded, so not copied.
        let copy = grafeno::upgrade_copy_path(&path, version);
        let copies = if version < grafeno::BASE_LAYOUT {
            assert_eq!(Grafeno::file_layout(&copy).unwrap(), Some(version));
            vec![copy.clone()]
        } else {
            Vec::new()
        };
        assert_eq!(grafeno::upgrade_copies(&path), copies);
        // Layout 18 until it holds a hyperedge.
        assert_eq!(
            Grafeno::file_layout(&path).unwrap(),
            Some(grafeno::BASE_LAYOUT)
        );
        // Reopening upgrades nothing more and loses nothing.
        let db = open_plain(&path).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(read.stats(NS).unwrap().nodes, 42, "v{version}");
        // One more entry than the fixture wrote (none before layout 12).
        let logged = if version >= 12 { 121 } else { 1 };
        assert_eq!(read.changes(NS, 0, 10).unwrap().last, logged, "v{version}");
        assert!(read.check(NS).unwrap().is_ok());
        assert_eq!(read.search_text(NS, "upgrade", 5).unwrap().len(), 1);
        drop(read);
        // A hyperedge (layout 19) between upgraded nodes, and its node
        // cannot be deleted from under it.
        let people = db
            .begin_read()
            .unwrap()
            .nodes_with_label(NS, "Person")
            .unwrap();
        let team = db
            .write(|tx| {
                tx.create_hyperedge(
                    NS,
                    grafeno::NewHyperEdge::new("team")
                        .member(people[0], "lead")
                        .node(people[1])
                        .node(people[2]),
                )
            })
            .unwrap();
        assert!(db.write(|tx| tx.delete_node(NS, people[1])).is_err());
        let read = db.begin_read().unwrap();
        let found = read
            .node_hyperedges(NS, people[1], &grafeno::HyperEdgeFilter::any())
            .unwrap();
        assert_eq!(found.len(), 1, "v{version}");
        assert_eq!(found[0].id, team);
        drop(read);
        // A full-text index (layout 18) on the upgraded data.
        db.write(|tx| {
            tx.create_index(
                NS,
                grafeno::IndexDef {
                    name: "people".into(),
                    fulltext: Some(grafeno::Fulltext::default()),
                    ..grafeno::IndexDef::new("Person", "name")
                },
            )
        })
        .unwrap();
        let read = db.begin_read().unwrap();
        let found = read
            .fulltext_search(NS, "people", "\"person 17\"", 5, false)
            .unwrap();
        assert_eq!(found.len(), 1, "v{version}");
        assert!(read.check(NS).unwrap().is_ok());
        drop((read, db));
        // The hyperedge made it layout 19.
        assert_eq!(
            Grafeno::file_layout(&path).unwrap(),
            Some(grafeno::HYPEREDGE_LAYOUT)
        );
        assert_eq!(grafeno::upgrade_copies(&path), copies);
        std::fs::remove_file(&path).unwrap();
        if version < grafeno::BASE_LAYOUT {
            std::fs::remove_file(&copy).unwrap();
        }
    }
}

#[test]
fn the_copy_before_an_upgrade() {
    // A new file and a current one are not copied.
    let path = std::env::temp_dir().join(format!("grafeno-copy-{}.redb", NodeId::new()));
    assert_eq!(Grafeno::file_layout(&path).unwrap(), None);
    drop(open_plain(&path).unwrap());
    assert_eq!(Grafeno::copy_before_upgrade(&path).unwrap(), None);
    assert!(grafeno::upgrade_copies(&path).is_empty());
    std::fs::remove_file(&path).unwrap();

    // Turned off, an old file is upgraded without a copy.
    let path = fixture(14);
    let options = grafeno::OpenOptions {
        copy_before_upgrade: false,
        ..Default::default()
    };
    drop(Grafeno::open_with(&path, &options).unwrap());
    assert!(grafeno::upgrade_copies(&path).is_empty());
    std::fs::remove_file(&path).unwrap();

    // A copy left by an earlier upgrade gives way to the new one.
    let path = fixture(15);
    let stale = grafeno::upgrade_copy_path(&path, 9);
    std::fs::write(&stale, b"older").unwrap();
    let copy = Grafeno::copy_before_upgrade(&path).unwrap().unwrap();
    assert_eq!(copy, grafeno::upgrade_copy_path(&path, 15));
    assert!(!stale.exists());
    // The copy is the file as it was: the old layout, the same bytes.
    assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(&path).unwrap());
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&copy).unwrap();
}

/// A temporary file path.
fn fresh(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("grafeno-{name}-{}.redb", NodeId::new()))
}

#[test]
fn layout_19_only_once_hyperedges_are_used() {
    use grafeno::{
        BASE_LAYOUT, Constraint, HYPEREDGE_LAYOUT as SCHEMA_VERSION, IndexDef, NewHyperEdge,
    };
    let layout = |path: &std::path::Path| Grafeno::file_layout(path).unwrap();

    // A new file, with nodes and edges and every other feature: 18, which
    // Grafeno 0.4 opens.
    let path = fresh("layout");
    let (a, b) = {
        let db = open_plain(&path).unwrap();
        let ids = db
            .write(|tx| {
                let a = tx.create_node(NS, NewNode::new("Person").text("Ana"))?;
                let b = tx.create_node(NS, NewNode::new("Person").text("Bo"))?;
                tx.create_index(NS, IndexDef::new("Person", "name"))?;
                Ok((a, b))
            })
            .unwrap();
        // A transaction that writes a hyperedge and fails leaves it 18.
        let failed = db.write(|tx| {
            tx.create_hyperedge(NS, NewHyperEdge::new("pair").node(ids.0).node(ids.1))?;
            Err::<(), _>(grafeno::Error::Invalid("changed my mind".into()))
        });
        assert!(failed.is_err());
        ids
    };
    assert_eq!(layout(&path), Some(BASE_LAYOUT));
    // Reopening keeps it 18 and makes no copy.
    drop(open_plain(&path).unwrap());
    assert_eq!(layout(&path), Some(BASE_LAYOUT));
    assert!(grafeno::upgrade_copies(&path).is_empty());
    {
        let db = open_plain(&path).unwrap();
        db.write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("pair").node(a).node(b)))
            .unwrap();
        // Later writes find it marked.
        db.write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("again").node(a).node(b)))
            .unwrap();
    }
    assert_eq!(layout(&path), Some(SCHEMA_VERSION));
    // Deleting them does not take it back.
    {
        let db = open_plain(&path).unwrap();
        db.write(|tx| {
            for id in tx.hyperedge_ids(NS, None, 10)? {
                tx.delete_hyperedge(NS, id)?;
            }
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(layout(&path), Some(SCHEMA_VERSION));
    std::fs::remove_file(&path).unwrap();

    // An index or a rule on hyperedges marks it too, with no hyperedge.
    for kind in ["index", "rule"] {
        let path = fresh(kind);
        {
            let db = open_plain(&path).unwrap();
            db.write(|tx| {
                if kind == "index" {
                    tx.create_index(NS, IndexDef::on_hyperedges("pair", &["k"]))
                        .map(drop)
                } else {
                    tx.create_constraint(
                        NS,
                        Constraint::Exists {
                            name: "k".into(),
                            label: "pair".into(),
                            key: "k".into(),
                            edges: false,
                            hyperedges: true,
                        },
                    )
                    .map(drop)
                }
            })
            .unwrap();
        }
        assert_eq!(layout(&path), Some(SCHEMA_VERSION), "{kind}");
        std::fs::remove_file(&path).unwrap();
    }

    // A replica applying a hyperedge marks its own file.
    let leader = Grafeno::in_memory().unwrap();
    leader.enable_replication_log().unwrap();
    let start = leader.begin_read().unwrap().log_position().unwrap().last;
    let (x, y) = leader
        .write(|tx| {
            Ok((
                tx.create_node(NS, NewNode::new("Person"))?,
                tx.create_node(NS, NewNode::new("Person"))?,
            ))
        })
        .unwrap();
    leader
        .write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("pair").node(x).node(y)))
        .unwrap();
    let entries = leader
        .begin_read()
        .unwrap()
        .log_entries(start, usize::MAX)
        .unwrap();
    let path = fresh("member");
    {
        let member = open_plain(&path).unwrap();
        member.apply_entries(&entries[..1]).unwrap();
    }
    assert_eq!(layout(&path), Some(BASE_LAYOUT));
    {
        let member = open_plain(&path).unwrap();
        member.apply_entries(&entries[1..]).unwrap();
    }
    assert_eq!(layout(&path), Some(SCHEMA_VERSION));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_snapshot_brings_its_own_layout() {
    use grafeno::{BASE_LAYOUT, HYPEREDGE_LAYOUT as SCHEMA_VERSION, NewHyperEdge};
    // A backup of a file without hyperedges (layout 18).
    let plain = Grafeno::in_memory().unwrap();
    let (a, b) = plain
        .write(|tx| {
            Ok((
                tx.create_node(NS, NewNode::new("Person"))?,
                tx.create_node(NS, NewNode::new("Person"))?,
            ))
        })
        .unwrap();
    let backup = fresh("snapshot");
    plain.begin_read().unwrap().backup(&backup).unwrap();
    assert_eq!(Grafeno::file_layout(&backup).unwrap(), Some(BASE_LAYOUT));

    // A member at layout 19 receives it: it is 18 again, and its next
    // hyperedge marks it 19 once more.
    let path = fresh("receiver");
    {
        let member = open_plain(&path).unwrap();
        let (x, y) = member
            .write(|tx| {
                Ok((
                    tx.create_node(NS, NewNode::new("Person"))?,
                    tx.create_node(NS, NewNode::new("Person"))?,
                ))
            })
            .unwrap();
        member
            .write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("pair").node(x).node(y)))
            .unwrap();
        member.restore_backup(&backup).unwrap();
        // Still open, it knows it is 18 again: the next hyperedge marks it.
        member
            .write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("pair").node(a).node(b)))
            .unwrap();
    }
    assert_eq!(Grafeno::file_layout(&path).unwrap(), Some(SCHEMA_VERSION));
    // Restored and closed without a hyperedge, it stays 18.
    let path2 = fresh("receiver-two");
    {
        let member = open_plain(&path2).unwrap();
        let (x, y) = member
            .write(|tx| {
                Ok((
                    tx.create_node(NS, NewNode::new("Person"))?,
                    tx.create_node(NS, NewNode::new("Person"))?,
                ))
            })
            .unwrap();
        member
            .write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("pair").node(x).node(y)))
            .unwrap();
        member.restore_backup(&backup).unwrap();
    }
    assert_eq!(Grafeno::file_layout(&path2).unwrap(), Some(BASE_LAYOUT));
    std::fs::remove_file(&path2).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&backup).unwrap();
}
