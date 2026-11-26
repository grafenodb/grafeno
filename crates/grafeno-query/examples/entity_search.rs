//! Looking up entities by their properties, as in a relational database:
//! filters, sorting and paging over 1,000,000 `Person` nodes on a database
//! file, with and without property indexes.
//!
//! `cargo run --release -p grafeno-query --example entity_search -- 1000000`
//! (`CSV=path` also writes the rows as CSV)

use std::io::Write as _;
use std::time::Instant;

use grafeno::{Find, Grafeno, NewNode, OrderBy};
use grafeno_query::{Params, execute};
use serde_json::json;

const COUNTRIES: [&str; 20] = [
    "PE", "AR", "BR", "CL", "CO", "MX", "ES", "US", "FR", "DE", "IT", "PT", "UY", "PY", "BO", "EC",
    "VE", "CA", "GB", "JP",
];

fn median_ms(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let started = Instant::now();
            f();
            started.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[runs / 2]
}

fn query_ms(db: &Grafeno, query: &str) -> String {
    // Within the server's limits: a query over one (max_rows…) fails.
    let run = || {
        grafeno_query::execute_with(
            db,
            "people",
            query,
            &Params::new(),
            &grafeno_query::Limits::standard(),
        )
    };
    if let Err(error) = run() {
        return format!("fails: {}", error.message.replace(',', ";"));
    }
    format!(
        "{:.2}",
        median_ms(5, || {
            run().unwrap();
        })
    )
}

fn main() {
    let nodes: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(1_000_000);
    let path = std::env::temp_dir().join(format!("grafeno-entities-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Grafeno::open(&path).unwrap();
    let started = Instant::now();
    for chunk in 0..nodes.div_ceil(50_000) {
        db.write(|tx| {
            for i in chunk * 50_000..((chunk + 1) * 50_000).min(nodes) {
                // A skewed spread: PE is 1 person in 16.
                let country = if i % 16 == 0 {
                    "PE"
                } else {
                    COUNTRIES[1 + (i * 7919) % (COUNTRIES.len() - 1)]
                };
                let name = format!("N{:07}", (i * 7_368_787) % 10_000_019);
                tx.create_node(
                    "people",
                    NewNode::new("Person")
                        .prop("name", name)
                        .prop("country", country)
                        .prop("age", (i % 90) as i64),
                )?;
            }
            Ok(())
        })
        .unwrap();
    }
    println!(
        "{nodes} :Person nodes written in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    std::io::stdout().flush().unwrap();

    let queries = [
        (
            "PE by name, first 20",
            "MATCH (p:Person) WHERE p.country = 'PE' RETURN p.name ORDER BY p.name LIMIT 20",
        ),
        (
            "PE by name, rows 10,001 to 10,020",
            "MATCH (p:Person) WHERE p.country = 'PE' RETURN p.name ORDER BY p.name SKIP 10000 LIMIT 20",
        ),
        (
            "everyone by name, first 20",
            "MATCH (p:Person) RETURN p.name ORDER BY p.name LIMIT 20",
        ),
        (
            "everyone by name descending, first 20",
            "MATCH (p:Person) RETURN p.name ORDER BY p.name DESC LIMIT 20",
        ),
        (
            "PE aged 30, by name",
            "MATCH (p:Person {country: 'PE'}) WHERE p.age = 30 RETURN p.name ORDER BY p.name LIMIT 20",
        ),
        (
            "count of PE",
            "MATCH (p:Person {country: 'PE'}) RETURN count(p)",
        ),
    ];
    let mut rows: Vec<(String, String, String)> = Vec::new();
    let mut run = |setup: &str| {
        for (name, query) in &queries {
            let ms = query_ms(&db, query);
            println!("| {name} | {setup} | {ms} ms |");
            std::io::stdout().flush().unwrap();
            rows.push((name.to_string(), setup.to_owned(), ms));
        }
    };
    run("no index");
    execute(
        &db,
        "people",
        "CREATE INDEX FOR (p:Person) ON (p.country)",
        &Params::new(),
    )
    .unwrap();
    run("index on country");
    execute(
        &db,
        "people",
        "CREATE INDEX FOR (p:Person) ON (p.name)",
        &Params::new(),
    )
    .unwrap();
    run("indexes on country and name");
    execute(
        &db,
        "people",
        "CREATE INDEX FOR (p:Person) ON (p.country, p.name)",
        &Params::new(),
    )
    .unwrap();
    run("and on (country, name)");

    // Keyset paging through every PE person, 100 at a time, with the API.
    let mut find = Find::label("Person")
        .equals("country", json!("PE"))
        .order_by(OrderBy::asc("name"))
        .limit(100);
    let started = Instant::now();
    let mut pages = 0;
    let mut seen = 0;
    loop {
        let page = db
            .begin_read()
            .unwrap()
            .find_nodes("people", &find)
            .unwrap();
        pages += 1;
        seen += page.nodes.len();
        match page.next {
            Some(next) => find = find.after(next),
            None => break,
        }
    }
    let per_page = started.elapsed().as_secs_f64() * 1000.0 / pages as f64;
    println!(
        "| find_nodes: every PE person, 100 a page | {pages} pages, {seen} nodes | {per_page:.2} ms a page |"
    );
    rows.push((
        "find_nodes: every PE person, 100 a page".into(),
        format!("{pages} pages"),
        format!("{per_page:.2}"),
    ));

    if let Ok(csv) = std::env::var("CSV") {
        let mut out = String::from("query,indexes,median_ms\n");
        for (name, setup, ms) in rows {
            out.push_str(&format!("\"{name}\",\"{setup}\",{ms}\n"));
        }
        std::fs::write(csv, out).unwrap();
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}
