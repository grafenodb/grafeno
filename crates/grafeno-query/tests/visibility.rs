//! Phase 14.4: random queries under random rights never return a hidden
//! node, edge or property, and never change one.
//!
//! Each case draws a graph (nodes with labels `A`, `B`, `C`, properties
//! `x`, `secret` and `team`, written by `me` or `other`; edges `L1`, `L2`)
//! and the rights of the account `me` (reading the namespace or not, label
//! grants and denials for nodes and edges, hidden properties, a record
//! policy, write grants). A direct model of the rules says what is visible;
//! read queries must return exactly that, and write statements must leave
//! every hidden record as it was (and change nothing when they fail).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use grafeno::{
    Account, AccountKind, Act, Actor, Applies, Condition, Edge, EdgeId, Grafeno, Node, NodeId,
    Object, Policy, PolicyValue, Privilege, Rights, RoleDef, Timestamp, View,
};
use grafeno_query::{Limits, Params, Value, execute_in, execute_read_with};
use proptest::prelude::*;

const NS: &str = "v";
const LABELS: [&str; 3] = ["A", "B", "C"];
const EDGE_LABELS: [&str; 2] = ["L1", "L2"];

#[derive(Clone, Debug)]
struct Spec {
    /// Per node: labels (bits of A, B, C), x, secret, team (red?), written by me?
    nodes: Vec<(u8, i64, i64, bool, bool)>,
    /// Per edge: from, to, label (L1?), w.
    edges: Vec<(usize, usize, bool, i64)>,
    rights: RightsSpec,
}

#[derive(Clone, Debug)]
struct RightsSpec {
    namespace_read: bool,
    label_grants: u8,
    label_denials: u8,
    edge_grants: u8,
    edge_denials: u8,
    /// Labels on which `secret` is hidden.
    secret_hidden: u8,
    /// 0: none; 1: my own records only; 2: label A to my team; for reads.
    policy: u8,
    /// The account's team.
    red: bool,
    writes: u8,
}

fn spec() -> impl Strategy<Value = Spec> {
    let nodes = prop::collection::vec(
        (1u8..8, 0i64..3, 0i64..3, any::<bool>(), any::<bool>()),
        3..12,
    );
    (nodes, any::<[u8; 9]>(), any::<bool>(), any::<bool>())
        .prop_flat_map(|(nodes, bits, read, red)| {
            let n = nodes.len();
            let edges = prop::collection::vec((0..n, 0..n, any::<bool>(), 0i64..3), 0..n * 2);
            (Just(nodes), edges, Just(bits), Just(read), Just(red))
        })
        .prop_map(|(nodes, edges, bits, namespace_read, red)| Spec {
            nodes,
            edges,
            rights: RightsSpec {
                namespace_read,
                label_grants: bits[0] & 7,
                label_denials: bits[1] & bits[2] & 7,
                edge_grants: bits[3] & 3,
                edge_denials: bits[4] & bits[5] & 3,
                secret_hidden: bits[6] & 7,
                policy: bits[7] % 3,
                red,
                writes: bits[8],
            },
        })
}

fn labels_of(bits: u8) -> Vec<String> {
    LABELS
        .iter()
        .enumerate()
        .filter(|(i, _)| bits & (1 << i) != 0)
        .map(|(_, l)| (*l).to_owned())
        .collect()
}

fn has(bits: u8, label: &str, of: &[&str]) -> bool {
    of.iter()
        .position(|l| *l == label)
        .is_some_and(|i| bits & (1 << i) != 0)
}

/// The role `r` for these rights, and the account `me`.
fn rights_for(r: &RightsSpec) -> (Account, Arc<Rights>) {
    let ns = || NS.to_owned();
    let mut privileges = Vec::new();
    if r.namespace_read {
        privileges.push(Privilege::grant(
            Act::Read,
            Object::Namespace { namespace: ns() },
        ));
    }
    for (i, label) in LABELS.iter().enumerate() {
        let object = Object::Label {
            namespace: ns(),
            label: (*label).into(),
            edges: false,
        };
        if r.label_grants & (1 << i) != 0 {
            privileges.push(Privilege::grant(Act::Read, object.clone()));
        }
        if r.label_denials & (1 << i) != 0 {
            privileges.push(Privilege::deny(Act::Read, object.clone()));
        }
        if r.secret_hidden & (1 << i) != 0 {
            privileges.push(Privilege::deny(
                Act::Read,
                Object::Property {
                    namespace: ns(),
                    label: (*label).into(),
                    property: "secret".into(),
                },
            ));
        }
    }
    for (i, label) in EDGE_LABELS.iter().enumerate() {
        let object = Object::Label {
            namespace: ns(),
            label: (*label).into(),
            edges: true,
        };
        if r.edge_grants & (1 << i) != 0 {
            privileges.push(Privilege::grant(Act::Read, object.clone()));
        }
        if r.edge_denials & (1 << i) != 0 {
            privileges.push(Privilege::deny(Act::Read, object.clone()));
        }
    }
    for (i, act) in [Act::Create, Act::Update, Act::Delete]
        .into_iter()
        .enumerate()
    {
        if r.writes & (1 << i) != 0 {
            privileges.push(Privilege::grant(act, Object::Namespace { namespace: ns() }));
        }
    }
    if r.writes & 8 != 0 {
        privileges.push(Privilege::deny(
            Act::Write,
            Object::Property {
                namespace: ns(),
                label: "B".into(),
                property: "x".into(),
            },
        ));
    }
    let policies = match r.policy {
        1 => vec![Policy {
            namespace: ns(),
            label: None,
            applies: Applies::Read,
            condition: Condition::RecordedByCaller,
        }],
        2 => vec![Policy {
            namespace: ns(),
            label: Some("A".into()),
            applies: Applies::All,
            condition: Condition::Property {
                key: "team".into(),
                equals: PolicyValue::Attribute {
                    name: "team".into(),
                },
            },
        }],
        _ => Vec::new(),
    };
    let role = RoleDef {
        name: "r".into(),
        privileges,
        grants: Vec::new(),
        roles: Vec::new(),
        policies,
        description: None,
        created_at: Timestamp::now(),
    };
    let mut attributes = serde_json::Map::new();
    attributes.insert(
        "team".into(),
        serde_json::json!(if r.red { "red" } else { "blue" }),
    );
    let account = Account {
        name: "me".into(),
        kind: AccountKind::Service,
        password_hash: None,
        grants: Vec::new(),
        roles: vec!["r".into()],
        superuser: false,
        own_only: false,
        disabled: false,
        expires_at: None,
        attributes,
        description: None,
        created_at: Timestamp::now(),
    };
    let rights = Rights::compute(&account, &HashMap::from([("r".to_owned(), role)]));
    (account, Arc::new(rights))
}

/// The rules, written directly from the design.
struct Model<'a> {
    r: &'a RightsSpec,
}

impl Model<'_> {
    fn policy_holds(
        &self,
        labels: Option<&[String]>,
        props: &grafeno::Props,
        by: Option<&str>,
    ) -> bool {
        match self.r.policy {
            1 => by == Some("me"),
            2 => match labels {
                Some(labels) if labels.iter().any(|l| l == "A") => {
                    props.get("team")
                        == Some(&serde_json::json!(if self.r.red { "red" } else { "blue" }))
                }
                _ => true,
            },
            _ => true,
        }
    }

    fn node(&self, node: &Node) -> bool {
        let denied = node
            .labels
            .iter()
            .any(|l| has(self.r.label_denials, l, &LABELS));
        let granted = node
            .labels
            .iter()
            .any(|l| has(self.r.label_grants, l, &LABELS));
        !denied
            && (self.r.namespace_read || granted)
            && self.policy_holds(Some(&node.labels), &node.props, node.recorded_by.as_deref())
    }

    fn edge(&self, edge: &Edge, nodes: &HashMap<NodeId, Node>) -> bool {
        !has(self.r.edge_denials, &edge.label, &EDGE_LABELS)
            && (self.r.namespace_read || has(self.r.edge_grants, &edge.label, &EDGE_LABELS))
            && self.policy_holds(None, &edge.props, edge.recorded_by.as_deref())
            && nodes.get(&edge.from).is_some_and(|n| self.node(n))
            && nodes.get(&edge.to).is_some_and(|n| self.node(n))
    }

    fn property(&self, labels: &[String], key: &str) -> bool {
        key != "secret" || !labels.iter().any(|l| has(self.r.secret_hidden, l, &LABELS))
    }
}

fn build(spec: &Spec) -> (Grafeno, HashMap<NodeId, Node>, HashMap<EdgeId, Edge>) {
    let db = Grafeno::in_memory().unwrap();
    let mut nodes = HashMap::new();
    let mut edges = HashMap::new();
    let mut ids = Vec::new();
    db.write(|tx| {
        for (labels, x, secret, red, mine) in &spec.nodes {
            let mut props = serde_json::Map::new();
            props.insert("x".into(), serde_json::json!(x));
            props.insert("secret".into(), serde_json::json!(secret));
            props.insert(
                "team".into(),
                serde_json::json!(if *red { "red" } else { "blue" }),
            );
            let node = Node {
                id: NodeId::new(),
                labels: labels_of(*labels),
                props,
                sources: Vec::new(),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
                recorded_by: Some(if *mine { "me" } else { "other" }.into()),
            };
            tx.restore_node(NS, node.clone(), None, None)?;
            ids.push(node.id);
            nodes.insert(node.id, node);
        }
        for (i, (from, to, l1, w)) in spec.edges.iter().enumerate() {
            let mut props = serde_json::Map::new();
            props.insert("w".into(), serde_json::json!(w));
            let edge = Edge {
                id: EdgeId::new(),
                label: if *l1 { "L1" } else { "L2" }.into(),
                from: ids[*from],
                to: ids[*to],
                props,
                sources: Vec::new(),
                valid_from: None,
                valid_to: None,
                recorded_at: Timestamp::now(),
                expired_at: None,
                recorded_by: Some(if i % 2 == 0 { "me" } else { "other" }.into()),
            };
            tx.restore_edge(NS, edge.clone())?;
            edges.insert(edge.id, edge);
        }
        Ok(())
    })
    .unwrap();
    (db, nodes, edges)
}

/// Every node, edge and property in a value, checked against the model.
fn check_value(
    value: &Value,
    model: &Model,
    nodes: &HashMap<NodeId, Node>,
    edges: &HashMap<EdgeId, Edge>,
) -> Result<(), String> {
    match value {
        Value::Node(node) => {
            let stored = &nodes[&node.id];
            if !model.node(stored) {
                return Err(format!("hidden node {:?} returned", stored.labels));
            }
            for key in node.props.keys() {
                if !model.property(&stored.labels, key) {
                    return Err(format!(
                        "hidden property {key} of {:?} returned",
                        stored.labels
                    ));
                }
            }
            for (key, _) in stored.props.iter() {
                if model.property(&stored.labels, key) && !node.props.contains_key(key) {
                    return Err(format!("visible property {key} missing"));
                }
            }
            Ok(())
        }
        Value::Edge(edge) => {
            if !model.edge(&edges[&edge.id], nodes) {
                return Err(format!("hidden edge {} returned", edge.label));
            }
            Ok(())
        }
        Value::Path(path) => {
            for node in &path.nodes {
                check_value(&Value::Node(node.clone()), model, nodes, edges)?;
            }
            for edge in &path.edges {
                check_value(&Value::Edge(edge.clone()), model, nodes, edges)?;
            }
            Ok(())
        }
        Value::List(items) => items
            .iter()
            .try_for_each(|v| check_value(v, model, nodes, edges)),
        Value::Map(map) => map
            .values()
            .try_for_each(|v| check_value(v, model, nodes, edges)),
        _ => Ok(()),
    }
}

fn reads(db: &Grafeno, view: &Arc<View>, query: &str) -> Result<Vec<Vec<Value>>, String> {
    let limits = Limits {
        view: Some(view.clone()),
        ..Limits::none()
    };
    execute_read_with(
        &db.begin_read().unwrap(),
        NS,
        query,
        &Params::new(),
        &limits,
    )
    .map(|r| r.rows)
    .map_err(|e| e.to_string())
}

fn ints(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => -1,
        })
        .collect()
}

/// Found by the property test: an account that may write here but read
/// nothing got the stored counts from the catalog procedures.
#[test]
fn a_caller_who_reads_nothing_counts_nothing() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        for _ in 0..3 {
            tx.create_node(NS, grafeno::NewNode::new("A").prop("x", 1))?;
        }
        Ok(())
    })
    .unwrap();
    let spec = RightsSpec {
        namespace_read: false,
        label_grants: 0,
        label_denials: 0,
        edge_grants: 0,
        edge_denials: 0,
        secret_hidden: 0,
        policy: 0,
        red: false,
        writes: 0b110,
    };
    let (account, rights) = rights_for(&spec);
    let view = Arc::new(View::new(rights, Some(&account)));
    for query in [
        "CALL db.labels() YIELD label RETURN label",
        "CALL db.edgeLabels() YIELD label RETURN label",
        "CALL db.propertyKeys() YIELD key RETURN key",
    ] {
        assert_eq!(
            reads(&db, &view, query).unwrap(),
            Vec::<Vec<Value>>::new(),
            "{query}"
        );
    }
    let stats = reads(
        &db,
        &view,
        "CALL db.stats() YIELD nodes, edges RETURN nodes, edges",
    )
    .unwrap();
    assert_eq!(stats, vec![vec![Value::Int(0), Value::Int(0)]]);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

    #[test]
    fn reads_return_only_what_is_visible(spec in spec()) {
        let (db, nodes, edges) = build(&spec);
        let (account, rights) = rights_for(&spec.rights);
        let view = Arc::new(View::new(rights, Some(&account)));
        let model = Model { r: &spec.rights };
        let visible: BTreeSet<NodeId> = nodes.values().filter(|n| model.node(n)).map(|n| n.id).collect();
        let visible_edges: BTreeSet<EdgeId> = edges.values().filter(|e| model.edge(e, &nodes)).map(|e| e.id).collect();

        for query in [
            "MATCH (n) RETURN n",
            "MATCH (a)-[r]->(b) RETURN a, r, b",
            "MATCH (n:A) RETURN n, n.secret",
            "MATCH (n) WHERE n.secret = 1 RETURN n",
            "MATCH p = (a)-[*1..3]->(b) RETURN p",
            "MATCH (n) RETURN keys(n), properties(n)",
            "MATCH (n)-[r]-(m) RETURN n, r, m",
        ] {
            let rows = reads(&db, &view, query).map_err(|e| TestCaseError::fail(format!("{query}: {e}")))?;
            for row in &rows {
                for value in row {
                    check_value(value, &model, &nodes, &edges)
                        .map_err(|e| TestCaseError::fail(format!("{query}: {e}")))?;
                }
            }
        }
        // Exactly the visible records, counted as if the rest did not exist.
        let rows = reads(&db, &view, "MATCH (n) RETURN n").unwrap();
        let got: BTreeSet<NodeId> = rows.iter().filter_map(|r| match &r[0] { Value::Node(n) => Some(n.id), _ => None }).collect();
        prop_assert_eq!(&got, &visible);
        prop_assert_eq!(ints(&reads(&db, &view, "MATCH (n) RETURN count(n)").unwrap()), vec![visible.len() as i64]);
        prop_assert_eq!(ints(&reads(&db, &view, "MATCH ()-[r]->() RETURN count(r)").unwrap()), vec![visible_edges.len() as i64]);
        // A filter on a hidden property finds nothing.
        let hidden_secret: Vec<&Node> = nodes.values().filter(|n| model.node(n) && !model.property(&n.labels, "secret")).collect();
        let rows = reads(&db, &view, "MATCH (n) WHERE n.secret IS NOT NULL RETURN n").unwrap();
        for row in &rows {
            if let Value::Node(n) = &row[0] {
                prop_assert!(!hidden_secret.iter().any(|h| h.id == n.id));
            }
        }
        // Reachability goes only through what is visible.
        let mut reach: BTreeSet<(NodeId, NodeId)> = BTreeSet::new();
        for &start in &visible {
            let mut frontier = vec![start];
            let mut seen = HashSet::from([start]);
            for _ in 0..3 {
                let mut next = Vec::new();
                for id in frontier {
                    for e in edges.values() {
                        // A variable-length pattern reaches each end once, at
                        // its shortest distance: never back to its start.
                        if e.from == id && visible_edges.contains(&e.id) && e.to != start {
                            reach.insert((start, e.to));
                            if seen.insert(e.to) {
                                next.push(e.to);
                            }
                        }
                    }
                }
                frontier = next;
            }
        }
        let rows = reads(&db, &view, "MATCH (a)-[*1..3]->(b) RETURN DISTINCT a, b").unwrap();
        let got: BTreeSet<(NodeId, NodeId)> = rows.iter().filter_map(|r| match (&r[0], &r[1]) {
            (Value::Node(a), Value::Node(b)) => Some((a.id, b.id)),
            _ => None,
        }).collect();
        prop_assert_eq!(got, reach);
        // The catalog counts the visible.
        let labels = reads(&db, &view, "CALL db.labels() YIELD label, count RETURN label, count").unwrap();
        let mut expected: BTreeMap<String, i64> = BTreeMap::new();
        for id in &visible {
            for l in &nodes[id].labels {
                *expected.entry(l.clone()).or_default() += 1;
            }
        }
        let got: BTreeMap<String, i64> = labels.iter().filter_map(|r| match (&r[0], &r[1]) {
            (Value::Str(l), Value::Int(c)) => Some((l.clone(), *c)),
            _ => None,
        }).collect();
        prop_assert_eq!(got, expected);
    }

    #[test]
    fn writes_never_change_what_is_hidden(spec in spec(), which in 0usize..6) {
        let (db, nodes, edges) = build(&spec);
        let (account, rights) = rights_for(&spec.rights);
        let view = Arc::new(View::new(rights, Some(&account)));
        let model = Model { r: &spec.rights };
        let statement = [
            "MATCH (n) SET n.x = 99",
            "MATCH (n) SET n.secret = 7",
            "MATCH (n) DETACH DELETE n",
            "MATCH (a)-[r]->(b) DELETE r",
            "MATCH (n:A) SET n:C",
            "MATCH (a), (b) CREATE (a)-[:L1]->(b)",
        ][which];
        let mut tx = db.begin_write().unwrap();
        tx.act_as(Some(Actor::new("me").with_view(view.clone())));
        let read = db.begin_read().unwrap();
        let limits = Limits { view: Some(view.clone()), ..Limits::none() };
        let outcome = execute_in(&mut tx, &read, NS, statement, &Params::new(), &limits);
        drop(read);
        let failed = outcome.is_err();
        if failed { tx.abort().unwrap() } else { tx.commit().unwrap() }

        let after = db.begin_read().unwrap();
        for node in nodes.values() {
            let now = after.node(NS, node.id).unwrap();
            if !model.node(node) || failed {
                prop_assert_eq!(now.as_ref(), Some(node), "{} changed a node it may not see (or failed and changed it)", statement);
            }
        }
        for edge in edges.values() {
            let now = after.edge(NS, edge.id).unwrap();
            if !model.edge(edge, &nodes) || failed {
                prop_assert_eq!(now.as_ref(), Some(edge), "{} changed an edge it may not see", statement);
            }
        }
        if failed {
            prop_assert_eq!(after.stats(NS).unwrap().edges as usize, edges.len());
        }
    }
}
