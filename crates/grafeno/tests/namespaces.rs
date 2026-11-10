//! Namespaces as records: created, listed when empty, described, dropped,
//! exported and upgraded.

use grafeno::{Error, Grafeno, IndexDef, NewEdge, NewNode};

fn names(db: &Grafeno) -> Vec<String> {
    db.begin_read()
        .unwrap()
        .namespaces()
        .unwrap()
        .into_iter()
        .map(|info| info.name)
        .collect()
}

#[test]
fn empty_namespaces_stay_listed() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| tx.create_namespace("demo", Some("  A first try ")))
        .unwrap();
    db.write(|tx| tx.create_namespace("demo2", None)).unwrap();
    assert_eq!(names(&db), ["demo", "demo2"]);
    let info = db.begin_read().unwrap().namespace("demo").unwrap().unwrap();
    assert_eq!((info.nodes, info.edges), (0, 0));
    assert_eq!(info.description.as_deref(), Some("A first try"));

    // Creating it again says so instead of switching silently.
    let again = db
        .write(|tx| tx.create_namespace("demo", None))
        .unwrap_err();
    assert!(matches!(again, Error::Conflict(_)), "{again}");
    for bad in ["", " demo", "a/b", "tab\there", &"x".repeat(129)] {
        assert!(
            db.write(|tx| tx.create_namespace(bad, None)).is_err(),
            "{bad:?}"
        );
    }

    // Writing to a new name registers it; deleting its last node keeps it.
    let id = db
        .write(|tx| tx.create_node("notes", NewNode::new("Note")))
        .unwrap();
    assert_eq!(names(&db), ["demo", "demo2", "notes"]);
    db.write(|tx| tx.delete_node("notes", id)).unwrap();
    assert_eq!(names(&db), ["demo", "demo2", "notes"]);

    db.write(|tx| tx.describe_namespace("demo2", Some("Second")))
        .unwrap();
    assert!(matches!(
        db.write(|tx| tx.describe_namespace("nope", None)),
        Err(Error::NamespaceNotFound(_))
    ));
}

#[test]
fn dropping_removes_everything_and_nothing_else() {
    let db = Grafeno::in_memory().unwrap();
    let fill = |ns: &'static str| {
        db.write(move |tx| {
            let a = tx.create_node(
                ns,
                NewNode::new("Person")
                    .prop("name", "Ana")
                    .text("ownership and borrowing")
                    .embedding(vec![1.0, 0.0, 0.5]),
            )?;
            let b = tx.create_node(ns, NewNode::new("Person").prop("name", "Bob"))?;
            tx.create_edge(ns, NewEdge::new(a, "knows", b))?;
            tx.create_index(
                ns,
                IndexDef {
                    name: IndexDef::default_name("Person", "name", false),
                    label: "Person".into(),
                    key: "name".into(),
                    unique: false,
                    fuzzy: false,
                    ..IndexDef::default()
                },
            )?;
            Ok(())
        })
        .unwrap();
    };
    // "demo" sorts right before "demo2" and "demo-x": prefixes must not leak.
    for ns in ["demo", "demo2", "demo-x", "dem"] {
        fill(ns);
    }
    let dropped = db.write(|tx| tx.drop_namespace("demo")).unwrap();
    assert_eq!((dropped.nodes, dropped.edges), (2, 1));
    assert_eq!(names(&db), ["dem", "demo-x", "demo2"]);
    let read = db.begin_read().unwrap();
    let gone = read.stats("demo").unwrap();
    assert_eq!(
        (gone.nodes, gone.edges, gone.texts, gone.vectors),
        (0, 0, 0, 0)
    );
    assert!(gone.node_labels.is_empty());
    assert!(read.nodes("demo").unwrap().is_empty());
    assert!(read.search_text("demo", "ownership", 5).unwrap().is_empty());
    assert!(read.indexes("demo").unwrap().is_empty());
    for ns in ["demo2", "demo-x", "dem"] {
        let stats = read.stats(ns).unwrap();
        assert_eq!(
            (stats.nodes, stats.edges, stats.texts, stats.vectors),
            (2, 1, 1, 1),
            "{ns}"
        );
        assert_eq!(read.search_text(ns, "ownership", 5).unwrap().len(), 1);
        assert_eq!(read.indexes(ns).unwrap().len(), 1);
        assert!(read.check(ns).unwrap().is_ok(), "{ns}");
    }
    drop(read);
    assert!(matches!(
        db.write(|tx| tx.drop_namespace("demo")),
        Err(Error::NamespaceNotFound(_))
    ));
    // The name can be used again, from scratch.
    fill("demo");
    assert_eq!(db.begin_read().unwrap().stats("demo").unwrap().nodes, 2);
}

#[test]
fn exports_keep_empty_namespaces() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| tx.create_namespace("empty", Some("Nothing yet")))
        .unwrap();
    db.write(|tx| tx.create_node("full", NewNode::new("Note")))
        .unwrap();
    let mut dump = Vec::new();
    db.begin_read().unwrap().export(&[], &mut dump).unwrap();
    let copy = Grafeno::in_memory().unwrap();
    let stats = copy.import(dump.as_slice()).unwrap();
    assert_eq!(stats.namespaces, 2);
    assert_eq!(names(&copy), ["empty", "full"]);
    let original = db
        .begin_read()
        .unwrap()
        .namespace("empty")
        .unwrap()
        .unwrap();
    let restored = copy
        .begin_read()
        .unwrap()
        .namespace("empty")
        .unwrap()
        .unwrap();
    assert_eq!(original, restored, "same creation time and description");
}
