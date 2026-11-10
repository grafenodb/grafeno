//! Runs `conformance/scenarios.txt`: GQ's conformance scenarios (a graph, a
//! query, the expected result), written by hand. Every
//! scenario runs on a new database; all failures are listed together.

mod common;

use grafeno::Grafeno;
use grafeno_query::{Params, execute};
use serde_json::Value as Json;

const NS: &str = "conformance";
const SCENARIOS: &str = include_str!("conformance/scenarios.txt");

#[derive(Default)]
struct Scenario {
    name: String,
    line: usize,
    given: Vec<String>,
    when: String,
    expect: Option<Expect>,
}

enum Expect {
    Ordered(Json),
    AnyOrder(Json),
    Error(String),
}

/// What the line after a keyword started; a line without a keyword goes on.
#[derive(PartialEq)]
enum Open {
    None,
    Given,
    When,
}

fn scenarios() -> Vec<Scenario> {
    let mut out: Vec<Scenario> = Vec::new();
    let mut open = Open::None;
    for (index, line) in SCENARIOS.lines().enumerate() {
        let number = index + 1;
        let text = line.trim_end();
        if text.starts_with('#') || text.trim().is_empty() {
            open = Open::None;
            continue;
        }
        if let Some(name) = text.strip_prefix("=== ") {
            out.push(Scenario {
                name: name.to_owned(),
                line: number,
                ..Scenario::default()
            });
            open = Open::None;
            continue;
        }
        let current = out
            .last_mut()
            .unwrap_or_else(|| panic!("line {number}: outside a scenario"));
        let parse = |json: &str| -> Json {
            serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("line {number}: invalid JSON: {e}"))
        };
        if let Some(query) = text.strip_prefix("given:") {
            current.given.push(query.trim().to_owned());
            open = Open::Given;
        } else if let Some(query) = text.strip_prefix("when:") {
            current.when = query.trim().to_owned();
            open = Open::When;
        } else if let Some(json) = text.strip_prefix("then any order:") {
            current.expect = Some(Expect::AnyOrder(parse(json)));
            open = Open::None;
        } else if let Some(needle) = text.strip_prefix("then error:") {
            current.expect = Some(Expect::Error(needle.trim().to_owned()));
            open = Open::None;
        } else if let Some(json) = text.strip_prefix("then:") {
            current.expect = Some(Expect::Ordered(parse(json)));
            open = Open::None;
        } else if open == Open::When {
            current.when.push('\n');
            current.when.push_str(text);
        } else if open == Open::Given {
            let last = current.given.last_mut().expect("opened by given:");
            last.push('\n');
            last.push_str(text);
        } else {
            panic!("line {number}: cannot read {text:?}");
        }
    }
    out
}

/// A float that is a whole number and an integer are told apart by JSON:
/// compares them as written, so `2.0` and `2` differ.
fn sorted(rows: &Json) -> Vec<String> {
    let mut rows: Vec<String> = rows
        .as_array()
        .expect("rows are a list")
        .iter()
        .map(Json::to_string)
        .collect();
    rows.sort();
    rows
}

/// `got` with every value that `expected` writes as `"*"` replaced by
/// `"*"`, so that it compares equal.
fn masked(got: &Json, expected: &Json) -> Json {
    match (got, expected) {
        (_, Json::String(star)) if star == "*" => Json::String("*".into()),
        (Json::Array(got), Json::Array(expected)) if got.len() == expected.len() => Json::Array(
            got.iter()
                .zip(expected)
                .map(|(got, expected)| masked(got, expected))
                .collect(),
        ),
        _ => got.clone(),
    }
}

fn run(scenario: &Scenario) -> Result<(), String> {
    let db = Grafeno::in_memory().map_err(|e| e.to_string())?;
    let params = Params::new();
    for given in &scenario.given {
        execute(&db, NS, given, &params).map_err(|e| format!("given {given:?}: {e}"))?;
    }
    let result = execute(&db, NS, &scenario.when, &params);
    // Split across members, a read gives the same rows (13.4).
    common::check_split(&db, NS, &scenario.when, &params);
    let expect = scenario
        .expect
        .as_ref()
        .ok_or_else(|| "no `then`".to_owned())?;
    match (expect, result) {
        (Expect::Error(needle), Err(error)) => {
            if error.to_string().contains(needle.as_str()) {
                Ok(())
            } else {
                Err(format!("error {error:?} does not say {needle:?}"))
            }
        }
        (Expect::Error(needle), Ok(result)) => Err(format!(
            "expected an error saying {needle:?}, got {}",
            result.to_json()["rows"]
        )),
        (_, Err(error)) => Err(format!("failed: {error}")),
        (Expect::Ordered(rows), Ok(result)) => {
            let got = masked(&result.to_json()["rows"], rows);
            if &got == rows {
                Ok(())
            } else {
                Err(format!("expected {rows}, got {got}"))
            }
        }
        (Expect::AnyOrder(rows), Ok(result)) => {
            let got = result.to_json()["rows"].clone();
            if sorted(&got) == sorted(rows) {
                Ok(())
            } else {
                Err(format!("expected in any order {rows}, got {got}"))
            }
        }
    }
}

#[test]
fn every_scenario_holds() {
    let all = scenarios();
    assert!(all.len() > 100, "{} scenarios", all.len());
    let failures: Vec<String> = all
        .iter()
        .filter_map(|scenario| {
            run(scenario).err().map(|why| {
                format!(
                    "line {}: {}\n    {}\n    {why}",
                    scenario.line, scenario.name, scenario.when
                )
            })
        })
        .collect();
    println!(
        "{} of {} scenarios hold; {} split in 2, 3 and 5 parts",
        all.len() - failures.len(),
        all.len(),
        common::SPLIT.load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(
        failures.is_empty(),
        "{} of {} scenarios failed:\n{}",
        failures.len(),
        all.len(),
        failures.join("\n")
    );
}
