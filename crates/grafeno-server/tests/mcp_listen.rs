//! An agent's MCP server that also serves the HTTP API on the same
//! database file: an interface or a console reads what the agent writes
//! while it writes it, though only one process can open the file.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TOKEN: &str = "listen-test-token";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

fn query(port: u16, token: Option<&str>, text: &str) -> (u16, Value) {
    let mut request = agent().post(&format!("http://127.0.0.1:{port}/v1/agent/query"));
    if let Some(token) = token {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let mut response = request.send_json(json!({ "query": text })).unwrap();
    let status = response.status().as_u16();
    let body = response.body_mut().read_json().unwrap_or(Value::Null);
    (status, body)
}

#[test]
fn what_an_agent_writes_is_read_over_http_while_it_writes() {
    let dir = std::env::temp_dir().join(format!("grafeno-mcp-listen-{}", grafeno::NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("agent.redb");
    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_grafeno"))
        .args(["mcp", "--namespace", "agent", "--db"])
        .arg(&db)
        .args(["--listen", &format!("127.0.0.1:{port}"), "--token", TOKEN])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut ask = |id: u64, method: &str, params: Value| -> Value {
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    ask(1, "initialize", json!({ "protocolVersion": "2025-06-18" }));

    // The HTTP API answers, with its token.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(response) = agent()
            .get(&format!("http://127.0.0.1:{port}/health"))
            .call()
            && response.status().as_u16() == 200
        {
            break;
        }
        assert!(Instant::now() < deadline, "the HTTP API did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    let (status, _) = query(port, None, "MATCH (n) RETURN count(n)");
    assert_eq!(status, 401, "the token is needed");

    // Each memory the agent stores is there at once, over HTTP.
    for (id, text) in [
        (2, "The cache holds an hour"),
        (3, "Writes go one at a time"),
    ] {
        let stored = ask(
            id,
            "tools/call",
            json!({ "name": "remember", "arguments": { "text": text, "labels": ["Note"] } }),
        );
        assert_eq!(stored["result"]["isError"], false, "{stored}");
        let (status, body) = query(port, Some(TOKEN), "MATCH (n:Note) RETURN count(n)");
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["rows"][0][0], id - 1, "{body}");
    }
    // The change log, which an interface follows to draw new nodes as
    // they come, has the agent's writes.
    let mut changes = agent()
        .get(&format!("http://127.0.0.1:{port}/v1/agent/changes?since=0"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap();
    assert_eq!(changes.status().as_u16(), 200);
    let changes: Value = changes.body_mut().read_json().unwrap();
    let created = changes["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|change| change["op"] == "created" && change["labels"] == json!(["Note"]))
        .count();
    assert_eq!(created, 2, "{changes}");
    // And what is written over HTTP, the agent recalls.
    let (status, body) = query(
        port,
        Some(TOKEN),
        "CREATE (:Note {@text: 'Written from the console about the cache'})",
    );
    assert_eq!(status, 200, "{body}");
    let recalled = ask(
        4,
        "tools/call",
        json!({ "name": "recall", "arguments": { "query": "console cache" } }),
    );
    assert!(
        recalled.to_string().contains("Written from the console"),
        "{recalled}"
    );

    // A second process on the same file is refused, and told what to do.
    let second = Command::new(env!("CARGO_BIN_EXE_grafeno"))
        .args(["mcp", "--db"])
        .arg(&db)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!second.status.success());
    let message = String::from_utf8_lossy(&second.stderr);
    assert!(message.contains("--listen"), "{message}");
    // As is a second one on the same address.
    let taken = Command::new(env!("CARGO_BIN_EXE_grafeno"))
        .args(["mcp", "--memory", "--listen", &format!("127.0.0.1:{port}")])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!taken.status.success());
    assert!(
        String::from_utf8_lossy(&taken.stderr).contains("cannot serve the HTTP API"),
        "{}",
        String::from_utf8_lossy(&taken.stderr)
    );

    // When the agent's client closes its input, the process ends.
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the process did not end");
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
