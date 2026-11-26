//! `ReadTx::find_nodes`: nodes by their properties, sorted and paged with a
//! cursor, the same with or without indexes.

use grafeno::{Cmp, Find, Grafeno, IndexDef, NewNode, Node, OrderBy};
use serde_json::{Value as Json, json};

const NS: &str = "f";

fn people() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        for i in 0..1_200i64 {
            let mut node = NewNode::new("Person")
                .prop("country", ["PE", "AR", "CL"][(i % 3) as usize])
                .prop("age", i % 50);
            // Some without a name, a few with a list; names repeat.
            match i % 40 {
                0 => {}
                1 => node = node.prop("name", json!(["a", "b"])),
                _ => node = node.prop("name", format!("N{:04}", (i * 7919) % 700)),
            }
            tx.create_node(NS, node)?;
        }
        tx.create_node(NS, NewNode::new("Company").prop("name", "Acme"))?;
        Ok(())
    })
    .unwrap();
    db
}

/// Every page of `find`, put end to end, and the plans used.
fn all_pages(db: &Grafeno, find: &Find) -> (Vec<Node>, Vec<String>) {
    let mut find = find.clone();
    let mut nodes = Vec::new();
    let mut plans = Vec::new();
    loop {
        let page = db.begin_read().unwrap().find_nodes(NS, &find).unwrap();
        assert!(page.nodes.len() <= find.limit);
        nodes.extend(page.nodes);
        plans.push(page.plan);
        match page.next {
            Some(next) => find = find.after(next),
            None => return (nodes, plans),
        }
    }
}

/// What a lookup should return, by brute force.
fn expected(db: &Grafeno, find: &Find) -> Vec<Node> {
    let one = Find {
        limit: grafeno::MAX_FIND_LIMIT,
        after: None,
        order: None,
        ..find.clone()
    };
    let (mut nodes, _) = all_pages(db, &one);
    if let Some(order) = &find.order {
        let key = |node: &Node| {
            node.props.get(&order.key).and_then(|value| match value {
                Json::Number(n) => Some((1, json!(n.as_f64()), String::new())),
                Json::Bool(b) => Some((0, json!(b), String::new())),
                Json::String(s) => Some((2, Json::Null, s.clone())),
                _ => None,
            })
        };
        nodes.sort_by(|a, b| {
            let (ka, kb) = (key(a), key(b));
            let by_value = match (&ka, &kb) {
                (Some(_), None) => return std::cmp::Ordering::Less,
                (None, Some(_)) => return std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
                (Some(x), Some(y)) => x.0.cmp(&y.0).then_with(|| {
                    x.1.as_f64()
                        .partial_cmp(&y.1.as_f64())
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| x.1.as_bool().cmp(&y.1.as_bool()))
                        .then_with(|| x.2.cmp(&y.2))
                }),
            };
            let ordering = by_value.then(a.id.cmp(&b.id));
            if order.descending && ka.is_some() && kb.is_some() {
                ordering.reverse()
            } else if order.descending {
                // Both without a value: by id, reversed.
                a.id.cmp(&b.id).reverse().then(by_value)
            } else {
                ordering
            }
        });
    }
    nodes
}

fn ids(nodes: &[Node]) -> Vec<String> {
    nodes.iter().map(|node| node.id.to_string()).collect()
}

fn lookups() -> Vec<Find> {
    let pe = Find::label("Person").equals("country", json!("PE"));
    vec![
        Find::label("Person").limit(50),
        pe.clone().limit(33),
        pe.clone().order_by(OrderBy::asc("name")).limit(25),
        pe.clone().order_by(OrderBy::desc("name")).limit(40),
        Find::label("Person")
            .order_by(OrderBy::asc("name"))
            .limit(100),
        Find::label("Person")
            .filter("age", Cmp::Gte(json!(10)))
            .filter("age", Cmp::Lt(json!(12.5)))
            .order_by(OrderBy::desc("age"))
            .limit(7),
        Find::label("Person")
            .filter("name", Cmp::Prefix("N01".into()))
            .limit(9),
        Find::label("Person")
            .filter("country", Cmp::In(vec![json!("AR"), json!("CL")]))
            .filter("name", Cmp::Exists(false))
            .limit(5),
        Find::label("Person")
            .filter("country", Cmp::Ne(json!("PE")))
            .filter("age", Cmp::Eq(json!(7.0)))
            .order_by(OrderBy::asc("name"))
            .limit(3),
        Find::any().filter("name", Cmp::Eq(json!("Acme"))).limit(2),
        Find::label("Person")
            .filter("country", Cmp::In(Vec::new()))
            .limit(2),
    ]
}

#[test]
fn pages_put_together_are_every_match_in_order() {
    let db = people();
    let before: Vec<Vec<String>> = lookups()
        .iter()
        .map(|find| {
            let (nodes, _) = all_pages(&db, find);
            assert_eq!(ids(&nodes), ids(&expected(&db, find)), "{find:?}");
            ids(&nodes)
        })
        .collect();
    // Indexes change how, not what.
    db.write(|tx| {
        tx.create_index(NS, IndexDef::new("Person", "country"))?;
        tx.create_index(NS, IndexDef::new("Person", "name"))?;
        tx.create_index(NS, IndexDef::new("Person", "age"))?;
        tx.create_index(NS, IndexDef::composite("Person", &["country", "name"]))?;
        Ok(())
    })
    .unwrap();
    for (find, before) in lookups().iter().zip(before) {
        let (nodes, plans) = all_pages(&db, find);
        assert_eq!(ids(&nodes), before, "{find:?} {plans:?}");
    }
    let (_, plans) = all_pages(&db, &lookups()[2]);
    assert!(
        plans[0].contains("Person_country_name in order"),
        "{plans:?}"
    );
    let (_, plans) = all_pages(&db, &lookups()[1]);
    assert!(plans[0].contains("index Person_country"), "{plans:?}");
}

#[test]
fn an_ordered_page_reads_only_what_it_returns() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        tx.create_index(NS, IndexDef::composite("Person", &["country", "name"]))?;
        for i in 0..20_000i64 {
            tx.create_node(
                NS,
                NewNode::new("Person")
                    .prop("country", if i % 10 == 0 { "PE" } else { "AR" })
                    .prop("name", format!("N{:05}", (i * 7919) % 20_000)),
            )?;
        }
        Ok(())
    })
    .unwrap();
    let find = Find::label("Person")
        .equals("country", json!("PE"))
        .order_by(OrderBy::asc("name"))
        .limit(20);
    let read = db.begin_read().unwrap();
    let page = read.find_nodes(NS, &find).unwrap();
    assert_eq!(page.nodes.len(), 20);
    assert!(page.plan.contains("in order"), "{}", page.plan);
    let names: Vec<&str> = page
        .nodes
        .iter()
        .map(|node| node.props["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    // The next page starts after the last name.
    let next = read
        .find_nodes(NS, &find.clone().after(page.next.unwrap()))
        .unwrap();
    assert!(next.nodes[0].props["name"].as_str().unwrap() > names[19]);
}

#[test]
fn a_bad_cursor_or_limit_is_refused() {
    let db = people();
    let read = db.begin_read().unwrap();
    assert!(
        read.find_nodes(NS, &Find::label("Person").after("zz"))
            .is_err()
    );
    assert!(
        read.find_nodes(NS, &Find::label("Person").limit(0))
            .is_err()
    );
    assert!(
        read.find_nodes(NS, &Find::label("Person").limit(1_000_000))
            .is_err()
    );
}
