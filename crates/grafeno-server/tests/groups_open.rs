//! A member with several groups and no static token: once the first group
//! has an account, a request to another group's empty file must not make
//! the server open again (security review, F-021).

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grafeno::NodeId;
use serde_json::{Value, json};

struct Member(Child);

impl Drop for Member {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn call(
    port: u16,
    method: &str,
    path: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> (u16, Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let url = format!("http://127.0.0.1:{port}{path}");
    let auth = bearer.map(|b| format!("Bearer {b}"));
    let response = if method == "GET" {
        let mut request = agent.get(&url);
        if let Some(auth) = &auth {
            request = request.header("Authorization", auth);
        }
        request.call()
    } else {
        let mut request = agent.post(&url);
        if let Some(auth) = &auth {
            request = request.header("Authorization", auth);
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

#[test]
fn another_groups_empty_file_does_not_open_the_server() {
    let dir = std::env::temp_dir().join(format!("grafeno-groups-open-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raft, api) = (free_port(), free_port());
    let config = format!(
        "db = \"{}\"\naddr = \"127.0.0.1:{api}\"\n\n\
         [cluster]\nid = 1\naddr = \"127.0.0.1:{raft}\"\nsecret = \"s3cret\"\nvoters = [1]\n\
         tick_ms = 20\npeers = [{{ id = 1, addr = \"127.0.0.1:{raft}\", api = \"127.0.0.1:{api}\" }}]\n\
         groups = [{{ id = 1, voters = [1] }}]\n",
        dir.join("member.redb").to_string_lossy().replace('\\', "/"),
    );
    let path = dir.join("member.toml");
    std::fs::write(&path, config).unwrap();
    let _member = Member(
        Command::new(env!("CARGO_BIN_EXE_grafeno"))
            .args(["serve", "--config"])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.join("member.log")).unwrap())
            .spawn()
            .unwrap(),
    );

    // The first account, made while the server is open; a member may need
    // a moment to elect itself.
    let deadline = Instant::now() + Duration::from_secs(30);
    let token = loop {
        let (code, body) = call(
            api,
            "POST",
            "/v1/auth/setup",
            Some(json!({ "name": "root", "password": "correct horse battery" })),
            None,
        );
        if code == 201 {
            break body["access_token"].as_str().unwrap().to_owned();
        }
        assert!(Instant::now() < deadline, "setup: {code} {body}");
        std::thread::sleep(Duration::from_millis(200));
    };
    let (code, _) = call(api, "GET", "/v1/whoami", None, None);
    assert_eq!(code, 401, "closed once an account exists");
    let (code, _) = call(api, "GET", "/v1/whoami", None, Some(&token));
    assert_eq!(code, 200);

    // A bearer that is no key, sent to group 1 after the keys' cache is
    // stale, reloads group 1's credentials from its own, empty file.
    std::thread::sleep(Duration::from_millis(300));
    let (code, _) = call(
        api,
        "GET",
        "/v1/cluster/groups/1/status",
        None,
        Some("not-a-key"),
    );
    assert_eq!(code, 401);

    // The server is still closed, in every group.
    let (code, body) = call(api, "GET", "/v1/whoami", None, None);
    assert_eq!(code, 401, "{body}");
    let (code, body) = call(api, "GET", "/v1/auth/status", None, None);
    assert_eq!(body["open"], json!(false), "{code} {body}");
    let (code, _) = call(api, "GET", "/v1/cluster/groups/1/status", None, None);
    assert_eq!(code, 401);

    let _ = std::fs::remove_dir_all(&dir);
}
