//! Phase 5 of the Mental Palace plan: several agents, each its own
//! `grafeno mcp --url` process with its own key, share one memory through
//! one server that owns the file. One may write the shared palace and the
//! other only read it; each has a namespace of its own the other cannot
//! reach; their writes, at the same time, all land, each with who made it.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const OWNER: &str = "agents-test-owner";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into()
}

fn post(port: u16, token: &str, path: &str, body: Value) -> (u16, Value) {
    let mut response = http()
        .post(&format!("http://127.0.0.1:{port}{path}"))
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(body)
        .unwrap();
    let status = response.status().as_u16();
    (
        status,
        response.body_mut().read_json().unwrap_or(Value::Null),
    )
}

fn get(port: u16, token: &str, path: &str) -> (u16, Value) {
    let mut response = http()
        .get(&format!("http://127.0.0.1:{port}{path}"))
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .unwrap();
    let status = response.status().as_u16();
    (
        status,
        response.body_mut().read_json().unwrap_or(Value::Null),
    )
}

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// An agent: its MCP process, talking to the server with its key.
struct Agent {
    _process: Process,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    ids: AtomicU64,
}

impl Agent {
    fn start(port: u16, key: &str, namespace: &str) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_grafeno"))
            .args(["mcp", "--url", &format!("http://127.0.0.1:{port}")])
            .args(["--token", key, "--namespace", namespace])
            .env_remove("GRAFENO_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut agent = Agent {
            _process: Process(child),
            input,
            output,
            ids: AtomicU64::new(1),
        };
        agent.ask("initialize", json!({ "protocolVersion": "2025-06-18" }));
        agent
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id, "{response}");
        response["result"].clone()
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.ask(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    /// A tool that must succeed: its structured result.
    fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.tool(name, arguments);
        assert_eq!(result["isError"], false, "{name}: {result}");
        result["structuredContent"].clone()
    }

    /// A tool that must fail: its message.
    fn refused(&mut self, name: &str, arguments: Value) -> String {
        let result = self.tool(name, arguments);
        assert_eq!(result["isError"], true, "{name}: {result}");
        result["content"][0]["text"].as_str().unwrap().to_owned()
    }
}

/// An account with these grants and a key for it; the key's secret.
fn account(port: u16, name: &str, grants: Value) -> String {
    let (status, body) = post(
        port,
        OWNER,
        "/v1/accounts",
        json!({ "name": name, "kind": "service", "grants": grants }),
    );
    assert_eq!(status, 201, "{body}");
    let (status, body) = post(
        port,
        OWNER,
        "/v1/keys",
        json!({ "name": format!("{name}-key"), "account": name }),
    );
    assert_eq!(status, 201, "{body}");
    body["secret"].as_str().unwrap().to_owned()
}

#[test]
fn two_agents_share_a_palace_through_one_server() {
    let port = free_port();
    let _server = Process(
        Command::new(env!("CARGO_BIN_EXE_grafeno"))
            .args(["serve", "--memory", "--addr", &format!("127.0.0.1:{port}")])
            .args(["--token", OWNER])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while http()
        .get(&format!("http://127.0.0.1:{port}/health"))
        .call()
        .map(|r| r.status().as_u16())
        .unwrap_or(0)
        != 200
    {
        assert!(Instant::now() < deadline, "the server did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    // One writes the shared palace, the other only reads it; each has a
    // namespace of its own.
    let key_a = account(
        port,
        "agent-a",
        json!([{ "namespace": "shared", "role": "write" }, { "namespace": "private-a", "role": "write" }]),
    );
    let key_b = account(
        port,
        "agent-b",
        json!([{ "namespace": "shared", "role": "read" }, { "namespace": "private-b", "role": "write" }]),
    );
    let mut a = Agent::start(port, &key_a, "shared");
    let mut b = Agent::start(port, &key_b, "shared");

    // A builds the shared palace.
    a.ok("palace_create", json!({ "name": "Team" }));
    a.ok(
        "palace_create_room",
        json!({ "palace": "Team", "name": "Billing", "topic": "billing and invoices" }),
    );
    let note = a.ok(
        "remember",
        json!({ "text": "Invoices go out at night", "labels": ["Fact"] }),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    a.ok(
        "palace_anchor",
        json!({ "memory": note, "room": "Billing" }),
    );

    // B reads it at once, through the same server.
    let map = b.ok("palace_map", json!({ "room": "Billing" }));
    assert_eq!(map["room"]["loci"][0]["memories"][0]["id"], note.as_str());
    let walk = b.ok("palace_walk", json!({ "topic": "billing and invoices" }));
    assert_eq!(walk["memories"][0]["id"], note.as_str());
    let entered = b.ok("palace_enter", json!({ "room": "Billing" }));
    assert!(
        entered["context"]
            .as_str()
            .unwrap()
            .contains("Invoices go out")
    );
    let rows = b.ok(
        "query",
        json!({ "query": "MATCH (n:Fact) RETURN count(n)" }),
    );
    assert_eq!(rows["rows"][0][0], 1, "{rows}");

    // B may not write the shared palace, nor reach A's namespace; A may
    // not reach B's.
    let message = b.refused(
        "palace_anchor",
        json!({ "memory": note, "room": "Billing", "remove": true }),
    );
    assert!(message.contains("read-only"), "{message}");
    let message = b.refused("remember", json!({ "text": "Not mine to write" }));
    assert!(message.contains("read-only"), "{message}");
    let message = b.refused(
        "recall",
        json!({ "query": "anything", "namespace": "private-a" }),
    );
    assert!(message.contains("403"), "{message}");
    let message = a.refused("palace_map", json!({ "namespace": "private-b" }));
    assert!(message.contains("403"), "{message}");
    let message = b.refused(
        "query",
        json!({ "query": "MATCH (n) RETURN count(n)", "namespace": "private-a" }),
    );
    assert!(message.contains("403"), "{message}");
    // B writes in its own.
    b.ok(
        "remember",
        json!({ "text": "My own note", "namespace": "private-b" }),
    );

    // Both write at once: A in the shared namespace, B in its own, while
    // each also reads the shared palace. Every write lands.
    const EACH: usize = 40;
    let writer_a = std::thread::spawn(move || {
        for i in 0..EACH {
            a.ok(
                "remember",
                json!({ "text": format!("Shared note {i} from a") }),
            );
            a.ok("palace_map", json!({}));
        }
        a
    });
    let writer_b = std::thread::spawn(move || {
        for i in 0..EACH {
            b.ok(
                "remember",
                json!({ "text": format!("Own note {i} from b"), "namespace": "private-b" }),
            );
            b.ok("palace_walk", json!({ "topic": "billing" }));
        }
        b
    });
    let mut a = writer_a.join().unwrap();
    let _b = writer_b.join().unwrap();
    let count = |ns: &str| {
        let (status, body) = post(
            port,
            OWNER,
            &format!("/v1/{ns}/query"),
            json!({ "query": "MATCH (n:Memory) RETURN count(n)" }),
        );
        assert_eq!(status, 200, "{body}");
        body["rows"][0][0].as_u64().unwrap()
    };
    assert_eq!(count("shared"), EACH as u64);
    assert_eq!(count("private-b"), EACH as u64 + 1);
    for ns in ["shared", "private-b"] {
        let (status, report) = get(port, OWNER, &format!("/v1/{ns}/check"));
        assert_eq!(status, 200);
        assert_eq!(report["problems"], json!([]), "{ns}: {report}");
    }

    // Each write says who made it.
    let (_, who) = post(
        port,
        OWNER,
        "/v1/shared/query",
        json!({ "query": "MATCH (n:Memory) RETURN DISTINCT n.@recorded_by" }),
    );
    assert_eq!(who["rows"], json!([["agent-a"]]), "{who}");
    let (_, who) = post(
        port,
        OWNER,
        "/v1/private-b/query",
        json!({ "query": "MATCH (n:Memory) RETURN DISTINCT n.@recorded_by" }),
    );
    assert_eq!(who["rows"], json!([["agent-b"]]), "{who}");
    // And the refusals are in the audit log, with whom they refused.
    let (_, audit) = get(port, OWNER, "/v1/audit?limit=1000");
    let refused: Vec<&Value> = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "refused")
        .collect();
    assert!(refused.iter().any(|e| e["account"] == "agent-b"), "{audit}");
    assert!(refused.iter().any(|e| e["account"] == "agent-a"), "{audit}");

    // The route the agents use, over HTTP.
    let (status, tools) = get(port, &key_b, "/v1/shared/tools");
    assert_eq!(status, 200);
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"palace_map") && !names.contains(&"remember"));
    let (status, body) = post(port, &key_a, "/v1/shared/tools/nope", json!({}));
    assert_eq!(status, 404, "{body}");
    let (status, body) = post(
        port,
        &key_a,
        "/v1/shared/tools/recall",
        json!({ "query": "x", "namespace": "private-a" }),
    );
    assert_eq!(status, 400, "{body}");
    let (status, _) = post(
        port,
        &key_a,
        "/v1/shared/tools/query",
        json!({ "query": "RETURN 1" }),
    );
    assert_eq!(status, 400);
    let _ = a.ok("palace_map", json!({}));
}
