//! CSV and GraphML: a namespace goes out and comes back the same; files
//! from other tools come in with their own ids; re-imports update rather
//! than duplicate.

use grafeno::{
    Direction, EdgeFilter, Grafeno, InterchangeOptions, NewEdge, NewNode, NodeId, Timestamp,
};
use serde_json::json;

fn sample() -> (Grafeno, NodeId) {
    let db = Grafeno::in_memory().unwrap();
    let ana = db
        .write(|tx| {
            let ana = tx.create_node(
                "a",
                NewNode::new("Person")
                    .label("Admin")
                    .prop("name", "Ana, \"the\" lead")
                    .prop("age", 34)
                    .prop("score", 9.5)
                    .prop("active", true)
                    .prop("tags", json!(["rust", "graphs"]))
                    .text("Ana writes notes\non two lines"),
            )?;
            let acme = tx.create_node("a", NewNode::new("Company").prop("name", "Acme"))?;
            tx.create_node("a", NewNode::new("Company").prop("name", "<Initech> & co"))?;
            tx.create_edge(
                "a",
                NewEdge::new(ana, "works_at", acme)
                    .prop("since", 2020)
                    .valid_from(Timestamp::parse("2020-01-01").unwrap())
                    .valid_to(Timestamp::parse("2024-06-01").unwrap()),
            )?;
            let retracted = tx.create_edge("a", NewEdge::new(acme, "owns", ana))?;
            tx.retract_edge("a", retracted)?;
            Ok(ana)
        })
        .unwrap();
    (db, ana)
}

fn check_copy(db: &Grafeno, ns: &str, ana: NodeId) {
    let read = db.begin_read().unwrap();
    let stats = read.stats(ns).unwrap();
    assert_eq!(
        (stats.nodes, stats.edges),
        (3, 1),
        "retracted facts stay out"
    );
    let node = read.node(ns, ana).unwrap().expect("the same id");
    assert_eq!(node.labels, ["Admin", "Person"]);
    assert_eq!(node.props["name"], json!("Ana, \"the\" lead"));
    assert_eq!(node.props["age"], json!(34));
    assert_eq!(node.props["score"], json!(9.5));
    assert_eq!(node.props["active"], json!(true));
    assert_eq!(node.props["tags"], json!(["rust", "graphs"]));
    assert_eq!(
        read.text(ns, ana).unwrap().as_deref(),
        Some("Ana writes notes\non two lines")
    );
    let jobs = read
        .neighbors(
            ns,
            ana,
            Direction::Out,
            &EdgeFilter::any().label("works_at"),
        )
        .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].edge.props["since"], json!(2020));
    assert_eq!(
        jobs[0].edge.valid_to,
        Some(Timestamp::parse("2024-06-01").unwrap())
    );
    assert!(read.check(ns).unwrap().is_ok());
}

#[test]
fn csv_round_trip() {
    let (db, ana) = sample();
    let (mut nodes, mut edges) = (Vec::new(), Vec::new());
    let read = db.begin_read().unwrap();
    assert_eq!(read.export_nodes_csv("a", &mut nodes).unwrap(), 3);
    assert_eq!(read.export_edges_csv("a", &mut edges).unwrap(), 1);
    let head = String::from_utf8(nodes.clone()).unwrap();
    assert!(
        head.starts_with(
            "id:ID,:LABEL,active:boolean,age:int,name:string,score:float,tags:json,:TEXT\n"
        ),
        "{head}"
    );
    let options = InterchangeOptions::default();
    let copy = Grafeno::in_memory().unwrap();
    let stats = copy
        .import_nodes_csv("b", nodes.as_slice(), &options)
        .unwrap();
    assert_eq!((stats.nodes, stats.failed), (3, 0), "{stats:?}");
    let stats = copy
        .import_edges_csv("b", edges.as_slice(), &options)
        .unwrap();
    assert_eq!((stats.edges, stats.failed), (1, 0), "{stats:?}");
    check_copy(&copy, "b", ana);
    // Again: the same ids, so nothing doubles.
    copy.import_nodes_csv("b", nodes.as_slice(), &options)
        .unwrap();
    copy.import_edges_csv("b", edges.as_slice(), &options)
        .unwrap();
    check_copy(&copy, "b", ana);
}

#[test]
fn graphml_round_trip() {
    let (db, ana) = sample();
    let mut out = Vec::new();
    assert_eq!(
        db.begin_read()
            .unwrap()
            .export_graphml("a", &mut out)
            .unwrap(),
        (3, 1)
    );
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(text.contains("&lt;Initech&gt; &amp; co"), "{text}");
    assert!(text.contains("attr.type=\"long\""));
    let copy = Grafeno::in_memory().unwrap();
    let options = InterchangeOptions::default();
    let stats = copy.import_graphml("b", out.as_slice(), &options).unwrap();
    assert_eq!(
        (stats.nodes, stats.edges, stats.failed),
        (3, 1, 0),
        "{stats:?}"
    );
    check_copy(&copy, "b", ana);
    copy.import_graphml("b", out.as_slice(), &options).unwrap();
    check_copy(&copy, "b", ana);
}

#[test]
fn files_from_other_tools() {
    let db = Grafeno::in_memory().unwrap();
    let options = InterchangeOptions::default();
    // A spreadsheet export: its own ids, no types, no labels.
    let nodes = "id,name,age\n1,Ana,34\n2,\"Bob, Jr.\",28\n3,Carla,\n";
    let stats = db
        .import_nodes_csv("x", nodes.as_bytes(), &options)
        .unwrap();
    assert_eq!(stats.nodes, 3);
    let edges = "source,target,type,weight\n1,2,KNOWS,0.5\n2,3,,1\n1,9,KNOWS,1\n";
    let stats = db
        .import_edges_csv("x", edges.as_bytes(), &options)
        .unwrap();
    assert_eq!((stats.edges, stats.failed), (2, 1), "{stats:?}");
    assert!(stats.errors[0].contains("\"9\""), "{:?}", stats.errors);
    // The same people again, changed: updated in place.
    let again = "id,name,age\n1,Ana,35\n";
    db.import_nodes_csv("x", again.as_bytes(), &options)
        .unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(read.stats("x").unwrap().nodes, 3);
    let people = read.nodes("x").unwrap();
    let ana = people.iter().find(|n| n.props["name"] == "Ana").unwrap();
    assert_eq!(ana.props["age"], json!(35));
    assert_eq!(ana.labels, ["Node"]);
    let bob = people
        .iter()
        .find(|n| n.props["name"] == "Bob, Jr.")
        .unwrap();
    let out = read
        .neighbors("x", bob.id, Direction::Out, &EdgeFilter::any())
        .unwrap();
    assert_eq!(out[0].edge.label, "RELATED_TO", "no type: the default");
    assert!(
        people.iter().any(|n| !n.props.contains_key("age")),
        "an empty cell is no property"
    );
    drop(read);

    // GraphML as analysis libraries write it: keys d0, d1; undirected.
    let graphml = r#"<?xml version='1.0' encoding='utf-8'?>
<graphml xmlns="http://graphml.graphdrawing.org/xmlns">
  <key id="d0" for="node" attr.name="name" attr.type="string"/>
  <key id="d1" for="edge" attr.name="weight" attr.type="double"/>
  <graph edgedefault="undirected">
    <node id="a"><data key="d0">Ada &amp; co</data></node>
    <node id="b"><data key="d0"><![CDATA[Bea <b>]]></data></node>
    <node id="c"/>
    <edge source="a" target="b"><data key="d1">2.5</data></edge>
    <edge source="b" target="c"/>
  </graph>
</graphml>"#;
    let stats = db
        .import_graphml("g", graphml.as_bytes(), &options)
        .unwrap();
    assert_eq!(
        (stats.nodes, stats.edges, stats.failed),
        (3, 2, 0),
        "{stats:?}"
    );
    let read = db.begin_read().unwrap();
    let names: Vec<String> = read
        .nodes("g")
        .unwrap()
        .iter()
        .filter_map(|n| {
            n.props
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .collect();
    assert!(
        names.contains(&"Ada & co".to_owned()) && names.contains(&"Bea <b>".to_owned()),
        "{names:?}"
    );
    let weights: Vec<serde_json::Value> = read
        .edges("g")
        .unwrap()
        .iter()
        .filter_map(|e| e.props.get("weight").cloned())
        .collect();
    assert_eq!(weights, [json!(2.5)]);
}

#[test]
fn malformed_input_is_reported() {
    let db = Grafeno::in_memory().unwrap();
    let options = InterchangeOptions::default();
    let bad = "id:ID,age:int\n1,old\n2,30\n";
    let stats = db.import_nodes_csv("x", bad.as_bytes(), &options).unwrap();
    assert_eq!((stats.nodes, stats.failed), (1, 1));
    assert!(stats.errors[0].contains("row 2"), "{:?}", stats.errors);
    assert!(
        db.import_nodes_csv("x", "id\n\"open".as_bytes(), &options)
            .is_err()
    );
    assert!(
        db.import_graphml("x", "<graphml><node id='a'></graphml>".as_bytes(), &options)
            .is_err()
    );
}
