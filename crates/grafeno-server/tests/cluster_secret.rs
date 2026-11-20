//! The members' secret of the example settings is refused, and the secret
//! can come from the environment instead of the settings file (security
//! review, F-008).

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use grafeno::NodeId;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn health(port: u16) -> u16 {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    agent
        .get(&format!("http://127.0.0.1:{port}/health"))
        .call()
        .map(|r| r.status().as_u16())
        .unwrap_or(0)
}

#[test]
fn the_example_secret_is_refused_and_the_environment_gives_one() {
    let dir = std::env::temp_dir().join(format!("grafeno-cluster-secret-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let (raft, api) = (free_port(), free_port());
    let config = format!(
        "db = \"{}\"\naddr = \"127.0.0.1:{api}\"\n\n\
         [cluster]\nid = 1\naddr = \"127.0.0.1:{raft}\"\nsecret = \"change-this-shared-secret\"\n\
         voters = [1]\ntick_ms = 20\n\
         peers = [{{ id = 1, addr = \"127.0.0.1:{raft}\", api = \"127.0.0.1:{api}\" }}]\n",
        dir.join("member.redb").to_string_lossy().replace('\\', "/"),
    );
    let path = dir.join("member.toml");
    std::fs::write(&path, config).unwrap();
    let serve = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_grafeno"));
        command
            .args(["serve", "--config"])
            .arg(&path)
            .env("GRAFENO_TOKEN", "a-token-of-the-test")
            .env_remove("GRAFENO_CLUSTER_SECRET")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    };

    // The placeholder: the member does not start.
    let mut member = serve().spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = member.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = member.kill();
            panic!("a member with the example secret kept running");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut said = String::new();
    std::io::Read::read_to_string(&mut member.stderr.take().unwrap(), &mut said).unwrap();
    assert!(!status.success(), "{said}");
    assert!(said.contains("placeholder"), "{said}");

    // The same file, with the secret from the environment: it starts.
    let mut member = serve()
        .env("GRAFENO_CLUSTER_SECRET", "a-secret-of-this-test")
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let started = loop {
        if health(api) == 200 {
            break true;
        }
        if member.try_wait().unwrap().is_some() || Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = member.kill();
    let _ = member.wait();
    assert!(
        started,
        "a member with GRAFENO_CLUSTER_SECRET did not start"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
