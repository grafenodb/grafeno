//! Phases 13.2 and 13.3 end to end: three `grafeno serve` processes with
//! `[cluster]` settings on this machine. Writes reach every member;
//! followers forward writes and linearizable reads to the leader; a read
//! waits for a write's position; a keyed write runs once; when the leader's
//! process is killed, the others elect a new one and writes go on through
//! any member.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grafeno::NodeId;
use serde_json::{Value, json};

const TOKEN: &str = "cluster-test-token";

/// Distinct free ports: all bound at once, then released.
fn free_ports(count: usize) -> Vec<u16> {
    let listeners: Vec<TcpListener> = (0..count)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect()
}

struct Member {
    id: u64,
    api: u16,
    child: Option<Child>,
}

impl Drop for Member {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into()
}

/// A request with extra headers; returns the status, the body and the
/// member's position header.
fn call_with(
    port: u16,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, String)],
) -> Option<(u16, Value, Option<u64>)> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let auth = format!("Bearer {TOKEN}");
    let agent = agent();
    let response = if method == "GET" {
        let mut request = agent.get(&url).header("Authorization", &auth);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        request.call()
    } else {
        let mut request = agent.post(&url).header("Authorization", &auth);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        request.send_json(body.unwrap_or(json!({})))
    };
    let mut response = response.ok()?;
    let status = response.status().as_u16();
    let position = response
        .headers()
        .get("x-grafeno-position")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let text = response.body_mut().read_to_string().ok()?;
    Some((
        status,
        serde_json::from_str(&text).unwrap_or(Value::Null),
        position,
    ))
}

fn call(port: u16, method: &str, path: &str, body: Option<Value>) -> Option<(u16, Value)> {
    call_with(port, method, path, body, &[]).map(|(status, body, _)| (status, body))
}

fn status(member: &Member) -> Option<Value> {
    call(member.api, "GET", "/v1/cluster", None).map(|(_, body)| body)
}

fn leader(members: &[Member]) -> usize {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        for (at, member) in members.iter().enumerate() {
            if member.child.is_some() && status(member).is_some_and(|s| s["role"] == "leader") {
                return at;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let views: Vec<String> = members
        .iter()
        .map(|m| format!("member {} ({}): {:?}", m.id, m.api, status(m)))
        .collect();
    panic!("no leader: {views:#?}");
}

fn query(port: u16, text: &str) -> (u16, Value) {
    call(port, "POST", "/v1/c/query", Some(json!({ "query": text }))).expect("an answer")
}

fn count(port: u16) -> i64 {
    query(port, "MATCH (n:Note) RETURN count(n)").1["rows"][0][0]
        .as_i64()
        .unwrap_or(-1)
}

/// `(id, members' port, API port)` of each member.
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
         tick_ms = 20\npeers = [{}]\n",
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

#[test]
fn three_processes_replicate_route_and_fail_over() {
    let dir: PathBuf = std::env::temp_dir().join(format!("grafeno-cluster-e2e-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let ports = free_ports(6);
    let cluster: Vec<(u64, u16, u16)> = (1..=3)
        .map(|id| (id, ports[id as usize - 1], ports[id as usize + 2]))
        .collect();
    let mut members: Vec<Member> = cluster
        .iter()
        .map(|(id, _, api)| Member {
            id: *id,
            api: *api,
            child: Some(start(&dir, *id, &cluster)),
        })
        .collect();

    let first = leader(&members);
    for i in 0..5 {
        let (code, body) = query(members[first].api, &format!("CREATE (:Note {{i: {i}}})"));
        assert_eq!(code, 200, "{body}");
    }
    // A follower forwards a write to the leader; the answer carries the
    // position to read it at.
    let follower = (first + 1) % 3;
    let other = (first + 2) % 3;
    let (code, body, position) = call_with(
        members[follower].api,
        "POST",
        "/v1/c/query",
        Some(json!({ "query": "CREATE (:Note {i: 99})" })),
        &[],
    )
    .unwrap();
    assert_eq!(code, 200, "{body}");
    let position = position.expect("a position");
    // Read your writes on the third member: it waits for that position.
    let (code, body, _) = call_with(
        members[other].api,
        "POST",
        "/v1/c/query",
        Some(json!({ "query": "MATCH (n:Note) RETURN count(n)" })),
        &[("x-grafeno-after", position.to_string())],
    )
    .unwrap();
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["rows"][0][0], 6);
    // A linearizable read on a follower goes through the leader.
    let (code, body, _) = call_with(
        members[other].api,
        "POST",
        "/v1/c/query",
        Some(json!({ "query": "MATCH (n:Note) RETURN count(n)" })),
        &[("x-grafeno-consistency", "linearizable".into())],
    )
    .unwrap();
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["rows"][0][0], 6);
    // A keyed write runs once, however many times it is sent.
    for _ in 0..3 {
        let (code, body) = call(
            members[follower].api,
            "POST",
            "/v1/c/query",
            Some(
                json!({ "query": "CREATE (:Note {i: 100}) RETURN 1", "idempotency_key": "k-100" }),
            ),
        )
        .unwrap();
        assert_eq!(code, 200, "{body}");
    }
    // Every member reads the same notes (followers apply shortly after).
    let deadline = Instant::now() + Duration::from_secs(10);
    while members.iter().any(|m| count(m.api) != 7) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    for member in &members {
        assert_eq!(count(member.api), 7, "member {}", member.id);
    }

    // Accounts are replicated (14.1): a key made through the leader works
    // on the followers at once, and a user who signs in on one follower
    // uses the session on the other and refreshes it on the leader.
    let (code, body) = call(
        members[first].api,
        "POST",
        "/v1/keys",
        Some(json!({ "name": "reader", "grants": [{ "namespace": "c", "role": "read" }] })),
    )
    .unwrap();
    assert_eq!(code, 201, "{body}");
    let secret = body["secret"].as_str().unwrap().to_owned();
    let with = |port: u16, bearer: &str, method: &str, path: &str, body: Option<Value>| {
        let url = format!("http://127.0.0.1:{port}{path}");
        let auth = format!("Bearer {bearer}");
        let agent = agent();
        let mut response = if method == "GET" {
            agent.get(&url).header("Authorization", &auth).call()
        } else {
            agent
                .post(&url)
                .header("Authorization", &auth)
                .send_json(body.unwrap_or(json!({})))
        }
        .unwrap();
        let code = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap();
        (
            code,
            serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
        )
    };
    for member in [follower, other] {
        let (code, body) = with(members[member].api, &secret, "GET", "/v1/c/stats", None);
        assert_eq!(
            code, 200,
            "a new key on member {}: {body}",
            members[member].id
        );
    }
    let (code, body) = call(
        members[follower].api,
        "POST",
        "/v1/accounts",
        Some(
            json!({ "name": "ana", "kind": "user", "password": "her long passphrase",
                     "grants": [{ "namespace": "c", "role": "write" }] }),
        ),
    )
    .unwrap();
    assert_eq!(code, 201, "{body}");
    let (code, session) = with(
        members[follower].api,
        "none",
        "POST",
        "/v1/auth/signin",
        Some(json!({ "name": "ana", "password": "her long passphrase" })),
    );
    assert_eq!(code, 200, "{session}");
    let access = session["access_token"].as_str().unwrap().to_owned();
    let (code, body) = with(members[other].api, &access, "GET", "/v1/c/stats", None);
    assert_eq!(code, 200, "the session on another member: {body}");
    let (code, renewed) = with(
        members[first].api,
        "none",
        "POST",
        "/v1/auth/refresh",
        Some(json!({ "refresh_token": session["refresh_token"] })),
    );
    assert_eq!(code, 200, "{renewed}");
    let (code, body) = with(
        members[follower].api,
        renewed["access_token"].as_str().unwrap(),
        "GET",
        "/v1/c/stats",
        None,
    );
    assert_eq!(code, 200, "{body}");

    // The audit log (14.5): a request refused on a follower is recorded
    // once, in the replicated log, and the log outlives its leader.
    let (code, _) = with(
        members[follower].api,
        "gfk_wrong",
        "GET",
        "/v1/c/stats",
        None,
    );
    assert_eq!(code, 401);
    let audit = |port: u16| -> Vec<Value> {
        call(port, "GET", "/v1/audit?limit=10000", None)
            .map(|(_, page)| page["events"].as_array().cloned().unwrap_or_default())
            .unwrap_or_default()
    };
    let refused = |events: &[Value]| {
        events
            .iter()
            .filter(|e| e["kind"] == "refused" && e["target"] == "GET /v1/c/stats")
            .count()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut before = audit(members[first].api);
    while refused(&before) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        before = audit(members[first].api);
    }
    assert_eq!(refused(&before), 1, "{before:#?}");
    assert!(
        before
            .iter()
            .any(|e| e["kind"] == "account_created" && e["target"] == "ana")
    );

    // The leader's process dies: the others go on.
    let killed = members[first].id;
    if let Some(mut child) = members[first].child.take() {
        // Dropping a Child does not end the process: kill it.
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let second = leader(&members);
    assert_ne!(members[second].id, killed);
    let (code, body) = query(members[second].api, "CREATE (:Note {i: 5})");
    assert_eq!(code, 200, "{body}");
    // Writes keep working on any surviving member.
    let survivor = members
        .iter()
        .find(|m| m.child.is_some() && m.id != members[second].id)
        .unwrap();
    let (code, body) = query(survivor.api, "CREATE (:Note {i: 6})");
    assert_eq!(code, 200, "{body}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while count(survivor.api) != 9 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(count(survivor.api), 9);
    // The new leader holds every event of the old one, and goes on from it.
    let after = audit(members[second].api);
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "the audit log after failover"
    );
    assert_eq!(refused(&after), 1);
    let (code, _) = with(members[second].api, "gfk_wrong", "GET", "/v1/c/stats", None);
    assert_eq!(code, 401);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut later = audit(members[second].api);
    while refused(&later) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        later = audit(members[second].api);
    }
    assert_eq!(refused(&later), 2);
    for (i, event) in later.iter().enumerate() {
        assert_eq!(
            event["seq"],
            i as u64 + 1,
            "numbered without gaps across leaders"
        );
    }
    assert_eq!(
        status(&members[second]).unwrap()["voters"],
        json!([1, 2, 3])
    );
    drop(members);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What another member's port answers without the cluster secret.
fn raft_call(port: u16, method: &str, path: &str, secret: Option<&str>, body: Vec<u8>) -> u16 {
    let url = format!("http://127.0.0.1:{port}{path}");
    let agent = agent();
    let response = if method == "GET" {
        let mut request = agent.get(&url);
        if let Some(secret) = secret {
            request = request.header("x-grafeno-cluster", secret);
        }
        request.call()
    } else {
        let mut request = agent.post(&url);
        if let Some(secret) = secret {
            request = request.header("x-grafeno-cluster", secret);
        }
        request.send(&body[..])
    };
    response.map(|r| r.status().as_u16()).unwrap_or(0)
}

/// Found by an outside review: a client could mark its own request as one
/// another member forwarded, and a member that lost its quorum served it a
/// "linearizable" read it could not vouch for; the members' port answered
/// its status to anyone, and read a body before checking the secret; and a
/// member with an empty secret let anyone in.
#[test]
fn only_members_speak_for_members() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("grafeno-cluster-trust-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let ports = free_ports(6);
    let cluster: Vec<(u64, u16, u16)> = (1..=3)
        .map(|id| (id, ports[id as usize - 1], ports[id as usize + 2]))
        .collect();
    let mut members: Vec<Member> = cluster
        .iter()
        .map(|(id, _, api)| Member {
            id: *id,
            api: *api,
            child: Some(start(&dir, *id, &cluster)),
        })
        .collect();
    let first = leader(&members);
    let (code, body) = query(members[first].api, "CREATE (:Note {i: 1})");
    assert_eq!(code, 200, "{body}");

    // The members' port: its status and its messages need the secret, and
    // a large body without it is refused before it is read.
    let raft_port = cluster[first].1;
    assert_eq!(
        raft_call(raft_port, "GET", "/status", None, Vec::new()),
        401
    );
    assert_eq!(
        raft_call(raft_port, "GET", "/status", Some("wrong"), Vec::new()),
        401
    );
    assert_eq!(
        raft_call(raft_port, "GET", "/status", Some("s3cret"), Vec::new()),
        200
    );
    assert_eq!(
        raft_call(raft_port, "POST", "/raft", None, b"{}".to_vec()),
        401
    );
    // A large one is refused before it is read: the answer comes, or the
    // connection is closed while the client is still sending.
    let large = raft_call(raft_port, "POST", "/raft", None, vec![b'x'; 8 << 20]);
    assert!(matches!(large, 401 | 0), "{large}");

    // Two members go: the one left has no quorum.
    let survivor = (first + 1) % 3;
    for at in [first, (first + 2) % 3] {
        if let Some(mut child) = members[at].child.take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
    let read = |headers: &[(&str, String)]| {
        call_with(
            members[survivor].api,
            "POST",
            "/v1/c/query",
            Some(json!({ "query": "MATCH (n:Note) RETURN count(n)" })),
            headers,
        )
        .map(|(code, body, _)| (code, body))
        .expect("an answer")
    };
    let (code, body) = read(&[("x-grafeno-consistency", "linearizable".into())]);
    assert_eq!(code, 503, "{body}");
    // Marked as forwarded by the client itself: refused all the same.
    let (code, body) = read(&[
        ("x-grafeno-consistency", "linearizable".into()),
        ("x-grafeno-forwarded", "1".into()),
    ]);
    assert_eq!(
        code, 503,
        "a client's own forwarded mark was trusted: {body}"
    );
    // A local read is what the member has, as before.
    let (code, body) = read(&[]);
    assert_eq!(code, 200, "{body}");
    drop(members);

    // A member with an empty secret does not start.
    let ports = free_ports(2);
    let config = format!(
        "db = \"{}\"\naddr = \"127.0.0.1:{}\"\ntoken = \"{TOKEN}\"\n\n\
         [cluster]\nid = 1\naddr = \"127.0.0.1:{}\"\nsecret = \"\"\nvoters = [1]\n\
         peers = [{{ id = 1, addr = \"127.0.0.1:{}\", api = \"127.0.0.1:{}\" }}]\n",
        dir.join("empty.redb").to_string_lossy().replace('\\', "/"),
        ports[0],
        ports[1],
        ports[1],
        ports[0],
    );
    let path = dir.join("empty.toml");
    std::fs::write(&path, config).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_grafeno"))
        .args(["serve", "--config"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(dir.join("empty.log")).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let exited = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if exited.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let log = std::fs::read_to_string(dir.join("empty.log")).unwrap_or_default();
    let status = exited.expect("a member with an empty secret kept running");
    assert!(!status.success(), "{log}");
    assert!(log.contains("secret is empty"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}
