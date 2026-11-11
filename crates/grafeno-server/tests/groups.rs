//! Phase 13.5 end to end: three `grafeno serve` members running group 0
//! and two more Raft groups. Namespaces placed on different groups take
//! requests through any member; a namespace moved to another group while
//! clients keep writing to it ends with every acknowledged write exactly
//! once, its change log going on from the same sequence number.

use std::collections::HashSet;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grafeno::NodeId;
use serde_json::{Value, json};

const TOKEN: &str = "groups-test-token";

fn free_ports(count: usize) -> Vec<u16> {
    let listeners: Vec<TcpListener> = (0..count)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect()
}

struct Members(Vec<Child>);

impl Drop for Members {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn start(dir: &Path, id: u64, cluster: &[(u64, u16, u16)]) -> Child {
    let peers: Vec<String> = cluster
        .iter()
        .map(|(id, port, api)| {
            format!("{{ id = {id}, addr = \"127.0.0.1:{port}\", api = \"127.0.0.1:{api}\" }}")
        })
        .collect();
    let (_, own, api) = *cluster.iter().find(|(peer, _, _)| *peer == id).unwrap();
    let config = format!(
        "db = \"{}\"\naddr = \"127.0.0.1:{api}\"\ntoken = \"{TOKEN}\"\n\n\
         [cluster]\nid = {id}\naddr = \"127.0.0.1:{own}\"\nsecret = \"s3cret\"\nvoters = [1, 2, 3]\n\
         tick_ms = 20\npeers = [{}]\ngroups = [{{ id = 1, voters = [1, 2, 3] }}, {{ id = 2, voters = [1, 2, 3] }}]\n",
        dir.join(format!("member-{id}.redb"))
            .to_string_lossy()
            .replace('\\', "/"),
        peers.join(", ")
    );
    let path = dir.join(format!("member-{id}.toml"));
    std::fs::write(&path, config).unwrap();
    Command::new(env!("CARGO_BIN_EXE_grafeno"))
        .args(["serve", "--config"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(dir.join(format!("member-{id}.log"))).unwrap())
        .spawn()
        .unwrap()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

fn call(
    port: u16,
    method: &str,
    path: &str,
    body: Option<Value>,
    key: Option<&str>,
) -> (u16, Value) {
    let url = format!("http://127.0.0.1:{port}{path}");
    let auth = format!("Bearer {TOKEN}");
    let response = if method == "GET" {
        agent().get(&url).header("Authorization", &auth).call()
    } else {
        let mut request = agent().post(&url).header("Authorization", &auth);
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        request.send_json(body.unwrap_or(json!({})))
    };
    match response {
        Ok(mut response) => {
            let status = response.status().as_u16();
            let text = response.body_mut().read_to_string().unwrap_or_default();
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }
        Err(_) => (0, Value::Null),
    }
}

fn query(port: u16, ns: &str, text: &str) -> (u16, Value) {
    call(
        port,
        "POST",
        &format!("/v1/{ns}/query"),
        Some(json!({ "query": text })),
        None,
    )
}

fn count(port: u16, ns: &str) -> i64 {
    query(port, ns, "MATCH (n) RETURN count(n)").1["rows"][0][0]
        .as_i64()
        .unwrap_or(-1)
}

#[test]
fn namespaces_live_on_groups_and_move_under_load() {
    let dir = std::env::temp_dir().join(format!("grafeno-groups-e2e-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let ports = free_ports(6);
    let cluster: Vec<(u64, u16, u16)> = (1..=3)
        .map(|id| (id, ports[id as usize - 1], ports[id as usize + 2]))
        .collect();
    let apis: Vec<u16> = cluster.iter().map(|(_, _, api)| *api).collect();
    let members = Members(
        cluster
            .iter()
            .map(|(id, _, _)| start(&dir, *id, &cluster))
            .collect(),
    );
    // Every group elects a leader.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (code, groups) = call(apis[0], "GET", "/v1/cluster/groups", None, None);
        let leaders = |id: u64| {
            apis.iter()
                .filter(|api| {
                    let (_, groups) = call(**api, "GET", "/v1/cluster/groups", None, None);
                    groups
                        .as_array()
                        .and_then(|g| g.iter().find(|g| g["id"] == id))
                        .is_some_and(|g| g["status"]["role"] == "leader")
                })
                .count()
        };
        if code == 200
            && groups.as_array().is_some_and(|g| g.len() == 3)
            && (0..3).all(|id| leaders(id) == 1)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "groups did not elect leaders: {groups}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // Place namespaces on groups, through any member.
    for (ns, group, through) in [("alpha", 1, 0), ("gamma", 2, 1), ("moving", 1, 2)] {
        let (code, body) = call(
            apis[through],
            "POST",
            "/v1/cluster/placement",
            Some(json!({ "namespace": ns, "group": group })),
            None,
        );
        assert_eq!(code, 200, "{ns}: {body}");
    }
    let (_, placement) = call(apis[1], "GET", "/v1/cluster/placement", None, None);
    assert_eq!(placement.as_array().unwrap().len(), 3, "{placement}");
    // Writes to each, through different members; reads anywhere.
    for (i, ns) in ["alpha", "beta", "gamma"].iter().enumerate() {
        for k in 0..4 {
            let (code, body) = query(apis[(i + k) % 3], ns, &format!("CREATE (:Note {{k: {k}}})"));
            assert_eq!(code, 200, "{ns}: {body}");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while apis.iter().any(|api| {
        ["alpha", "beta", "gamma"]
            .iter()
            .any(|ns| count(*api, ns) != 4)
    }) {
        assert!(Instant::now() < deadline, "members did not catch up");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Each group's own log holds its namespace's writes.
    let (_, groups) = call(apis[0], "GET", "/v1/cluster/groups", None, None);
    for group in groups.as_array().unwrap() {
        assert!(group["status"]["applied"].as_u64().unwrap() > 1, "{group}");
    }
    let (_, listed) = call(apis[2], "GET", "/v1/namespaces", None, None);
    let names: HashSet<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["name"].as_str())
        .collect();
    assert_eq!(
        names,
        HashSet::from(["alpha", "beta", "gamma", "moving"]),
        "{listed}"
    );

    // A namespace moves to another group while clients write to it.
    let running = Arc::new(AtomicBool::new(true));
    let acknowledged = Arc::new(Mutex::new(Vec::<i64>::new()));
    let busy = Arc::new(Mutex::new(0usize));
    let writers: Vec<_> = (0..3)
        .map(|w| {
            let (running, acknowledged, busy) =
                (running.clone(), acknowledged.clone(), busy.clone());
            let api = apis[w];
            std::thread::spawn(move || {
                let mut n = 0i64;
                while running.load(Ordering::Relaxed) {
                    n += 1;
                    let v = w as i64 * 1_000_000 + n;
                    let key = format!("w{v}");
                    // Retried until it is acknowledged, with the same key.
                    loop {
                        let (code, _) = call(
                            api,
                            "POST",
                            "/v1/moving/query",
                            Some(
                                json!({ "query": "CREATE (:Item {v: $v})", "params": { "v": v } }),
                            ),
                            Some(&key),
                        );
                        if code == 200 {
                            acknowledged.lock().unwrap().push(v);
                            break;
                        }
                        *busy.lock().unwrap() += 1;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(1_500));
    let (code, moved) = call(
        apis[1],
        "POST",
        "/v1/cluster/placement",
        Some(json!({ "namespace": "moving", "group": 2 })),
        None,
    );
    assert_eq!(code, 200, "{moved}");
    assert_eq!(moved["moved"], true, "{moved}");
    std::thread::sleep(Duration::from_millis(1_500));
    running.store(false, Ordering::Relaxed);
    for writer in writers {
        writer.join().unwrap();
    }
    let acknowledged = acknowledged.lock().unwrap().clone();
    assert!(
        acknowledged.len() > 20,
        "only {} writes",
        acknowledged.len()
    );
    let (_, placement) = call(apis[0], "GET", "/v1/cluster/placement", None, None);
    let moving = placement
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["namespace"] == "moving")
        .cloned()
        .unwrap();
    assert_eq!(moving["group"], 2, "{placement}");
    // Every acknowledged write is there once, on every member.
    let expected: HashSet<i64> = acknowledged.iter().copied().collect();
    assert_eq!(expected.len(), acknowledged.len());
    let deadline = Instant::now() + Duration::from_secs(10);
    for api in &apis {
        loop {
            let (code, body) = query(*api, "moving", "MATCH (i:Item) RETURN i.v ORDER BY i.v");
            assert_eq!(code, 200, "{body}");
            let found: Vec<i64> = body["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row[0].as_i64().unwrap())
                .collect();
            let unique: HashSet<i64> = found.iter().copied().collect();
            if unique == expected && found.len() == unique.len() {
                break;
            }
            if Instant::now() >= deadline {
                // What every member sees, routes to and has applied.
                let mut seen = Vec::new();
                for other in &apis {
                    let (_, placement) = call(*other, "GET", "/v1/cluster/placement", None, None);
                    let (_, groups) = call(*other, "GET", "/v1/cluster/groups", None, None);
                    let applied: Vec<String> = groups
                        .as_array()
                        .map(|groups| {
                            groups
                                .iter()
                                .map(|g| format!("g{}={}", g["id"], g["status"]["applied"]))
                                .collect()
                        })
                        .unwrap_or_default();
                    seen.push(format!(
                        "{other}: {} items, placement {}, applied {}",
                        count(*other, "moving"),
                        placement,
                        applied.join(" ")
                    ));
                }
                for id in 1..=3 {
                    let log = std::fs::read_to_string(dir.join(format!("member-{id}.log")))
                        .unwrap_or_default();
                    let tail: Vec<&str> = log.lines().rev().take(15).collect();
                    seen.push(format!("member {id} log (last lines first): {tail:?}"));
                }
                panic!(
                    "member {api}: {} rows, {} distinct, {} acknowledged, missing {:?}, last acknowledged {:?}
{}",
                    found.len(),
                    unique.len(),
                    expected.len(),
                    expected.difference(&unique).take(5).collect::<Vec<_>>(),
                    acknowledged.iter().rev().take(6).collect::<Vec<_>>(),
                    seen.join("
")
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    // The change log goes on: one entry per write, numbered without a gap.
    let (_, changes) = call(
        apis[2],
        "GET",
        "/v1/moving/changes?limit=100000",
        None,
        None,
    );
    assert_eq!(changes["last"], expected.len(), "{}", changes["last"]);
    eprintln!(
        "moved with {} writes acknowledged, {} busy answers retried: {moved}",
        expected.len(),
        busy.lock().unwrap()
    );
    drop(members);
    let _ = std::fs::remove_dir_all(&dir);
}
