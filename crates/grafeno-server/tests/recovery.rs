//! Operational recovery, on real `grafeno serve` processes while an agent
//! keeps writing memories:
//!
//! - the server is killed without warning and started again: every memory
//!   it acknowledged is there, the file checks clean, and recall finds
//!   them;
//! - its file is damaged: the check refuses it, and the backup taken while
//!   it was writing restores into a new file with every memory acknowledged
//!   before the backup;
//! - in a cluster of three, the leader is killed while memories keep
//!   coming through the members: writes go on, and once the old leader is
//!   back, every acknowledged memory is on every member.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grafeno::NodeId;
use serde_json::{Value, json};

const TOKEN: &str = "recovery-test-token";
const NS: &str = "ops";

fn free_ports(count: usize) -> Vec<u16> {
    let listeners: Vec<TcpListener> = (0..count)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect()
}

struct Process(Option<Child>);

impl Process {
    fn kill(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into()
}

fn call(port: u16, method: &str, path: &str, body: Option<Value>) -> Option<(u16, Value)> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let auth = format!("Bearer {TOKEN}");
    let agent = agent();
    let response = if method == "GET" {
        agent.get(&url).header("Authorization", &auth).call()
    } else {
        agent
            .post(&url)
            .header("Authorization", &auth)
            .send_json(body.unwrap_or(json!({})))
    };
    let mut response = response.ok()?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().ok()?;
    Some((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

fn healthy(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if call(port, "GET", "/health", None).is_some_and(|(code, _)| code == 200) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the server on {port} did not start");
}

fn serve_file(db: &Path, port: u16, log: &Path) -> Process {
    Process(Some(
        Command::new(env!("CARGO_BIN_EXE_grafeno"))
            .args(["serve", "--db"])
            .arg(db)
            .args(["--addr", &format!("127.0.0.1:{port}")])
            .env("GRAFENO_TOKEN", TOKEN)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log).unwrap())
            .spawn()
            .unwrap(),
    ))
}

fn grafeno(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_grafeno"))
        .args(args)
        .output()
        .unwrap()
}

/// The memories an agent saved and was told were saved.
type Acked = Arc<Mutex<Vec<(String, String)>>>;

/// The last refusal the agent got, to say why it could not write.
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

/// The members' APIs, to show their view when writes stop.
static APIS: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// An agent writing memories to the members in `ports` in turn (those up
/// at the moment) until `stop`; the ones acknowledged (2xx) go in the list.
fn writer(
    ports: Arc<Mutex<Vec<u16>>>,
    stop: Arc<AtomicBool>,
) -> (Acked, std::thread::JoinHandle<()>) {
    let acked: Acked = Arc::default();
    let list = acked.clone();
    let handle = std::thread::spawn(move || {
        let mut n = 0usize;
        while !stop.load(Ordering::Relaxed) {
            let text = format!("Memoria operativa {n}: el despliegue {n} pasó la revisión");
            let port = {
                let ports = ports.lock().unwrap();
                ports[n % ports.len()]
            };
            let angle = n as f32 * 0.1;
            if let Some((code, body)) = call(
                port,
                "POST",
                &format!("/v1/{NS}/nodes"),
                Some(json!({
                    "labels": ["Memory"],
                    "text": text,
                    "embedding": [angle.cos(), angle.sin(), 1.0, 0.5],
                })),
            ) {
                match body["id"].as_str() {
                    Some(id) if (200..300).contains(&code) => {
                        list.lock().unwrap().push((id.to_owned(), text));
                    }
                    _ => *LAST_ERROR.lock().unwrap() = format!("{port}: {code} {body}"),
                }
            }
            n += 1;
        }
    });
    (acked, handle)
}

fn wait_for(acked: &Acked, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while acked.lock().unwrap().len() < count {
        if Instant::now() > deadline {
            let views: Vec<String> = APIS
                .lock()
                .unwrap()
                .iter()
                .map(|api| format!("{api}: {:?}", call(*api, "GET", "/v1/cluster", None)))
                .collect();
            panic!(
                "the agent could not write: {} of {count}; last: {}
{views:#?}",
                acked.lock().unwrap().len(),
                LAST_ERROR.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Every memory of `memories` is on the server at `port`, with its text.
fn all_there(port: u16, memories: &[(String, String)]) {
    for (id, text) in memories {
        let (code, node) = call(port, "GET", &format!("/v1/{NS}/nodes/{id}"), None)
            .unwrap_or_else(|| panic!("no answer for {id}"));
        assert_eq!(code, 200, "memory {id} is gone: {node}");
        assert_eq!(node["text"], json!(text), "{node}");
    }
}

#[test]
fn a_killed_server_keeps_what_it_acknowledged_and_a_backup_restores() {
    let dir: PathBuf = std::env::temp_dir().join(format!("grafeno-recovery-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let live = dir.join("live.redb");
    let port = free_ports(1)[0];
    let mut server = serve_file(&live, port, &dir.join("first.log"));
    healthy(port);

    let stop = Arc::new(AtomicBool::new(false));
    let (acked, handle) = writer(Arc::new(Mutex::new(vec![port])), stop.clone());
    wait_for(&acked, 100);

    // A backup while the agent writes: it holds at least what was
    // acknowledged before it was asked for.
    let before_backup = acked.lock().unwrap().clone();
    let mut response = agent()
        .get(&format!("http://127.0.0.1:{port}/v1/backup"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let bytes = response
        .body_mut()
        .with_config()
        .limit(1 << 30)
        .read_to_vec()
        .unwrap();
    let backup = dir.join("backup.redb");
    std::fs::write(&backup, bytes).unwrap();

    // More writes, then the process is killed mid-stream.
    let more = acked.lock().unwrap().len() + 100;
    wait_for(&acked, more);
    server.kill();
    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
    let acknowledged = acked.lock().unwrap().clone();

    // Started again: whole, and everything acknowledged is there.
    let check = grafeno(&["check", "--db", live.to_str().unwrap()]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let mut server = serve_file(&live, port, &dir.join("second.log"));
    healthy(port);
    all_there(port, &acknowledged);
    let (last_id, last) = acknowledged.last().unwrap();
    let (code, found) = call(
        port,
        "POST",
        &format!("/v1/{NS}/recall"),
        Some(json!({ "text": last, "expand": false, "limit": 1 })),
    )
    .unwrap();
    assert_eq!(code, 200, "{found}");
    assert_eq!(found[0]["node"]["id"], json!(last_id), "{found}");
    server.kill();

    // The file is damaged: the check refuses it.
    let mut file = std::fs::read(&live).unwrap();
    let len = file.len();
    for at in (len / 8..len).step_by(32 * 1024) {
        let end = (at + 4096).min(len);
        file[at..end].iter_mut().for_each(|b| *b = 0xA5);
    }
    std::fs::write(&live, file).unwrap();
    let check = grafeno(&["check", "--db", live.to_str().unwrap()]);
    assert!(!check.status.success(), "a damaged file passed the check");

    // The backup restores into a new file, which serves what it held.
    let restored = dir.join("restored.redb");
    let restore = grafeno(&[
        "restore",
        backup.to_str().unwrap(),
        restored.to_str().unwrap(),
    ]);
    assert!(
        restore.status.success(),
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    let check = grafeno(&["check", "--db", restored.to_str().unwrap()]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let _server = serve_file(&restored, port, &dir.join("restored.log"));
    healthy(port);
    all_there(port, &before_backup);
}

/// `(id, members' port, API port)` of each member.
fn member(dir: &Path, id: u64, cluster: &[(u64, u16, u16)]) -> Process {
    let peers: Vec<String> = cluster
        .iter()
        .map(|(id, port, api)| {
            format!("{{ id = {id}, addr = \"127.0.0.1:{port}\", api = \"127.0.0.1:{api}\" }}")
        })
        .collect();
    let (_, own, api) = *cluster.iter().find(|(peer, _, _)| *peer == id).unwrap();
    let config = format!(
        "db = \"{}\"\naddr = \"127.0.0.1:{api}\"\ntoken = \"{TOKEN}\"\n\n\
         [cluster]\nid = {id}\naddr = \"127.0.0.1:{own}\"\nsecret = \"recovery-secret\"\nvoters = [1, 2, 3]\n\
         tick_ms = 20\npeers = [{}]\n",
        dir.join(format!("member-{id}.redb"))
            .to_string_lossy()
            .replace('\\', "/"),
        peers.join(", ")
    );
    let path = dir.join(format!("member-{id}.toml"));
    std::fs::write(&path, config).unwrap();
    Process(Some(
        Command::new(env!("CARGO_BIN_EXE_grafeno"))
            .args(["serve", "--config"])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join(format!("member-{id}.log")))
                    .unwrap(),
            )
            .spawn()
            .unwrap(),
    ))
}

fn leader(apis: &[u16], alive: &[bool]) -> usize {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        for (at, api) in apis.iter().enumerate() {
            if alive[at]
                && call(*api, "GET", "/v1/cluster", None)
                    .is_some_and(|(_, s)| s["role"] == "leader")
            {
                return at;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("no leader");
}

fn memories_on(api: u16) -> i64 {
    call(
        api,
        "POST",
        &format!("/v1/{NS}/query"),
        Some(json!({ "query": "MATCH (n:Memory) RETURN count(n)" })),
    )
    .and_then(|(_, body)| body["rows"][0][0].as_i64())
    .unwrap_or(-1)
}

#[test]
fn the_leader_dies_while_memories_come_and_none_is_lost() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("grafeno-recovery-cluster-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let ports = free_ports(6);
    let cluster: Vec<(u64, u16, u16)> = (1..=3)
        .map(|id| (id, ports[id as usize - 1], ports[id as usize + 2]))
        .collect();
    let apis: Vec<u16> = cluster.iter().map(|(_, _, api)| *api).collect();
    *APIS.lock().unwrap() = apis.clone();
    let mut members: Vec<Process> = cluster
        .iter()
        .map(|(id, _, _)| member(&dir, *id, &cluster))
        .collect();
    let mut alive = vec![true; 3];
    let first = leader(&apis, &alive);

    let stop = Arc::new(AtomicBool::new(false));
    let up = Arc::new(Mutex::new(apis.clone()));
    let (acked, handle) = writer(up.clone(), stop.clone());
    wait_for(&acked, 60);

    // The leader goes; writes go on through the others.
    members[first].kill();
    alive[first] = false;
    up.lock().unwrap().retain(|api| *api != apis[first]);
    let killed_at = acked.lock().unwrap().len();
    let second = leader(&apis, &alive);
    assert_ne!(second, first);
    wait_for(&acked, killed_at + 60);

    // The old leader comes back as a follower and catches up.
    members[first] = member(&dir, cluster[first].0, &cluster);
    alive[first] = true;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !call(apis[first], "GET", "/health", None).is_some_and(|(code, _)| code == 200) {
        if Instant::now() > deadline {
            let logs: Vec<String> = (1..=3)
                .map(|id| {
                    std::fs::read_to_string(dir.join(format!("member-{id}.log")))
                        .unwrap_or_default()
                })
                .collect();
            panic!("member {} did not come back: {logs:#?}", cluster[first].0);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    up.lock().unwrap().push(apis[first]);
    let target = killed_at + 120;
    let deadline = Instant::now() + Duration::from_secs(60);
    while acked.lock().unwrap().len() < target {
        if Instant::now() > deadline {
            let views: Vec<String> = apis
                .iter()
                .map(|api| format!("{api}: {:?}", call(*api, "GET", "/v1/cluster", None)))
                .collect();
            let logs: Vec<String> = (1..=3)
                .map(|id| {
                    std::fs::read_to_string(dir.join(format!("member-{id}.log")))
                        .unwrap_or_default()
                })
                .collect();
            panic!(
                "writes stopped after the old leader came back ({} of {target}); last: {}
{views:#?}
{logs:#?}",
                acked.lock().unwrap().len(),
                LAST_ERROR.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
    let acknowledged = acked.lock().unwrap().clone();

    for (at, api) in apis.iter().enumerate() {
        let deadline = Instant::now() + Duration::from_secs(60);
        while memories_on(*api) < acknowledged.len() as i64 {
            assert!(
                Instant::now() < deadline,
                "member {at} has {} of {} memories",
                memories_on(*api),
                acknowledged.len()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        all_there(*api, &acknowledged);
    }
}
