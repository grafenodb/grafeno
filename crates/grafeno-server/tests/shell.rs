//! `grafeno shell` run as a program, without a terminal (scripts).

use std::io::Write;
use std::process::{Command, Output, Stdio};

use grafeno::NodeId;

fn shell(args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_grafeno"))
        .arg("shell")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(script) = stdin {
        input.write_all(script.as_bytes()).unwrap();
    }
    drop(input);
    child.wait_with_output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

#[test]
fn runs_statements_and_meta_commands_on_a_file() {
    let dir = std::env::temp_dir().join(format!("grafeno-shell-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("shell.redb");
    let db = db.to_str().unwrap();

    let output = shell(
        &[
            "--db",
            db,
            "-n",
            "team",
            "-c",
            "CREATE (a:Person {name: 'Ana'})-[:knows]->(b:Person {name: 'Bob'});\n\
             MATCH (a)-[:knows]->(b) RETURN a.name, b;\n\\ns",
        ],
        None,
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("2 nodes created, 1 edge created"),
        "{stdout}"
    );
    assert!(stdout.contains(" a.name | b\n"), "{stdout}");
    assert!(
        stdout.contains(" Ana    | (Bob:Person)\n(1 row)"),
        "{stdout}"
    );
    assert!(stdout.contains(" team      | 2     | 1"), "{stdout}");

    // The same file, from a script file and from standard input, as JSON.
    let script = dir.join("count.gq");
    std::fs::write(&script, "MATCH (p:Person)\nRETURN count(*) AS people;\n").unwrap();
    let output = shell(
        &[
            "--db",
            db,
            "-n",
            "team",
            "--json",
            "-f",
            script.to_str().unwrap(),
        ],
        None,
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["rows"], serde_json::json!([[2]]));

    let output = shell(
        &["--db", db, "-n", "team"],
        Some("\\check\nMATCH (n:Person) RETURN n.name ORDER BY n.name;\n"),
    );
    let stdout = text(&output.stdout);
    assert!(stdout.contains("team: 2 nodes, 1 edges"), "{stdout}");
    assert!(stdout.contains("ok\n"), "{stdout}");
    assert!(stdout.contains(" Ana\n Bob\n(2 rows)"), "{stdout}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn traces_show_conditions_and_anchors_whole() {
    let output = shell(
        &[
            "--memory",
            "-c",
            "CREATE INDEX FOR (p:Person) ON (p.name);
             CREATE (a:Person {name: 'Ana'})-[:knows]->(:Person {name: 'Bo', age: 20});
             TRACE MATCH (a:Person {name: 'Ana'})-[:knows]->(f) WHERE f.age > 30 RETURN f.name;",
        ],
        None,
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    // Ids are cut to their last eight characters; words are not.
    assert!(stdout.contains(" via index Person_name"), "{stdout}");
    assert!(stdout.contains("row: f.age > 30"), "{stdout}");
}

#[test]
fn failures_set_the_exit_code() {
    let output = shell(
        &["--memory", "-c", "RETURN 1;\nMATCH (n RETURN n;\nRETURN 2;"],
        None,
    );
    assert!(!output.status.success());
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("error: expected `)` to close the node pattern"),
        "{stderr}"
    );
    // Statements after the failing one still run.
    assert!(text(&output.stdout).contains(" 2\n(1 row)"));

    let output = shell(&["--memory", "-c", "\\nope"], None);
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("unknown command \\nope"));
}

#[test]
fn creates_and_drops_namespaces() {
    let dir = std::env::temp_dir().join(format!("grafeno-shell-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("ns.redb");
    let db = db.to_str().unwrap();
    let output = shell(
        &[
            "--db",
            db,
            "-c",
            "\\ns create demo\n\\ns create demo2\n\\ns\n\\ns drop demo\n\\ns drop demo --yes\nSHOW NAMESPACES;",
        ],
        None,
    );
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);
    assert!(
        stdout.contains("Created namespace \"demo\""),
        "{stdout}{stderr}"
    );
    assert!(stdout.contains("Created namespace \"demo2\""), "{stdout}");
    assert!(
        stderr.contains("--yes") || stdout.contains("--yes"),
        "asks to confirm: {stdout}{stderr}"
    );
    assert!(
        stdout.contains("Dropped namespace \"demo\""),
        "{stdout}{stderr}"
    );
    let listing = stdout.rsplit("Dropped namespace").next().unwrap();
    assert!(
        listing.contains("demo2") && !listing.contains("| demo "),
        "{listing}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn grafeno(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_grafeno"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(stdin.as_bytes()).unwrap();
    drop(input);
    child.wait_with_output().unwrap()
}

/// Phase 14.1: a user made with `grafeno users` signs in from the shell
/// (`--user`, the password on standard input) and works within its grants.
#[test]
fn users_sign_in_from_the_shell() {
    let dir = std::env::temp_dir().join(format!("grafeno-shell-users-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("users.redb");
    let db = db.to_str().unwrap();
    // A weak password is refused; a good one makes the user.
    let weak = grafeno(
        &[
            "users",
            "create",
            "ana",
            "-g",
            "notes:write",
            "--password-stdin",
            "--db",
            db,
        ],
        "short\n",
    );
    assert!(!weak.status.success());
    assert!(
        text(&weak.stderr).contains("at least"),
        "{}",
        text(&weak.stderr)
    );
    let made = grafeno(
        &[
            "users",
            "create",
            "ana",
            "-g",
            "notes:write",
            "--password-stdin",
            "--db",
            db,
        ],
        "her long passphrase\n",
    );
    assert!(made.status.success(), "{}", text(&made.stderr));
    let listed = grafeno(&["users", "list", "--db", db], "");
    assert!(text(&listed.stdout).contains("ana") && text(&listed.stdout).contains("notes:write"));

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut server = Command::new(env!("CARGO_BIN_EXE_grafeno"))
        .args(["serve", "--db", db, "--addr", &format!("127.0.0.1:{port}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the server did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let run = |password: &str, script: &str| {
        grafeno(
            &[
                "shell",
                "--url",
                &url,
                "--user",
                "ana",
                "--password-stdin",
                "-n",
                "notes",
                "-c",
                script,
            ],
            &format!("{password}\n"),
        )
    };
    let ok = run(
        "her long passphrase",
        "CREATE (:Note {t: 'from the shell'}); MATCH (n:Note) RETURN n.t, n.@recorded_by;",
    );
    let out = text(&ok.stdout);
    assert!(ok.status.success(), "{}", text(&ok.stderr));
    assert!(text(&ok.stderr).contains("Signed in as ana"));
    assert!(
        out.contains("from the shell") && out.contains("ana"),
        "{out}"
    );
    let wrong = run("not her passphrase", "MATCH (n) RETURN count(n);");
    assert!(!wrong.status.success());
    assert!(
        text(&wrong.stderr).contains("wrong name or password"),
        "{}",
        text(&wrong.stderr)
    );
    // Outside its grants, refused.
    let other = grafeno(
        &[
            "shell",
            "--url",
            &url,
            "--user",
            "ana",
            "--password-stdin",
            "-n",
            "secret",
            "-c",
            "MATCH (n) RETURN count(n);",
        ],
        "her long passphrase\n",
    );
    assert!(
        text(&other.stderr).contains("no `read` privilege"),
        "{}{}",
        text(&other.stdout),
        text(&other.stderr)
    );
    let _ = server.kill();
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Phase 14.3: `grafeno roles` and `grafeno grants` on a database file,
/// and access statements in a local shell (passwords through the policy).
#[test]
fn roles_and_grants_from_the_command_line() {
    let dir = std::env::temp_dir().join(format!("grafeno-shell-roles-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("roles.redb");
    let db = db.to_str().unwrap();
    let run = |args: &[&str]| {
        let mut all = args.to_vec();
        all.extend(["--db", db]);
        let out = grafeno(&all, "");
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    run(&[
        "roles",
        "create",
        "team-reader",
        "--description",
        "reads the teams",
    ]);
    run(&[
        "grants",
        "grant",
        "read,algorithms",
        "--on",
        "namespace:team-*",
        "--to",
        "team-reader",
    ]);
    run(&[
        "grants",
        "deny",
        "read",
        "--on",
        "namespace:team-secret",
        "--to",
        "team-reader",
    ]);
    run(&[
        "grants",
        "grant",
        "read",
        "--on",
        "property:Person.email@crm",
        "--to",
        "team-reader",
    ]);
    let shown = run(&["roles", "show", "team-reader"]);
    assert!(shown.contains("deny"), "{shown}");
    assert!(shown.contains("namespace team-secret"), "{shown}");
    assert!(shown.contains("property Person.email in crm"), "{shown}");
    let revoked = run(&[
        "grants",
        "revoke",
        "read",
        "--on",
        "namespace:team-secret",
        "--from",
        "team-reader",
        "--denials",
    ]);
    assert!(revoked.contains("revoked 1 privilege"), "{revoked}");
    let listed = run(&["roles", "list"]);
    assert!(
        listed.contains("team-reader") && listed.contains("reads the teams"),
        "{listed}"
    );
    assert!(run(&["grants", "catalog"]).contains("manage_roles"));

    // A local shell sets passwords through the policy and Argon2id.
    let weak = shell(
        &["--db", db, "-c", "CREATE USER ana SET PASSWORD 'short';"],
        None,
    );
    assert!(!weak.status.success());
    let good = shell(
        &[
            "--db",
            db,
            "-c",
            "CREATE USER ana SET PASSWORD 'correct horse battery';",
        ],
        None,
    );
    assert!(good.status.success(), "{}", text(&good.stderr));
    run(&["grants", "add-role", "team-reader", "--to", "ana"]);
    let users = run(&["users", "list"]);
    assert!(users.contains("ana"), "{users}");
    let shown = run(&["roles", "show", "ana"]);
    assert!(shown.contains("team-reader"), "{shown}");
    run(&["grants", "remove-role", "team-reader", "--from", "ana"]);
    run(&["roles", "delete", "team-reader"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn namespaces_from_scripts_and_the_command_line() {
    let dir = std::env::temp_dir().join(format!("grafeno-ns-script-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("ns.redb");
    let db = db.to_str().unwrap();
    let script = dir.join("setup.gq");
    std::fs::write(
        &script,
        "CREATE NAMESPACE crm IF NOT EXISTS COMMENT 'customers';\n\
         CREATE NAMESPACE crm IF NOT EXISTS;\n\
         \\use crm\n\
         CREATE INDEX FOR (c:Customer) ON (c.email);\n\
         CREATE (:Customer {email: 'a@example.org'});\n",
    )
    .unwrap();
    let output = shell(&["--db", db, "-f", script.to_str().unwrap()], None);
    assert!(output.status.success(), "{}", text(&output.stderr));

    // Stops at the first failure only when asked.
    let bad = "CREATE NAMESPACE crm;\nCREATE NAMESPACE later;";
    let output = shell(&["--db", db, "-c", bad], None);
    assert!(!output.status.success());
    let output = shell(&["--db", db, "-c", "SHOW NAMESPACES;"], None);
    assert!(text(&output.stdout).contains("later"), "the rest ran");
    let output = shell(
        &[
            "--db",
            db,
            "--stop-on-error",
            "-c",
            "CREATE NAMESPACE crm;\nCREATE NAMESPACE never;",
        ],
        None,
    );
    assert!(!output.status.success());

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_grafeno"))
            .args(args)
            .output()
            .unwrap()
    };
    let listing = run(&["namespaces", "--db", db]);
    let listing = text(&listing.stdout);
    assert!(
        listing.contains("crm") && !listing.contains("never"),
        "{listing}"
    );
    assert!(
        run(&[
            "namespaces",
            "--db",
            db,
            "create",
            "logs",
            "--description",
            "app logs"
        ])
        .status
        .success()
    );
    assert!(
        !run(&["namespaces", "--db", db, "create", "logs"])
            .status
            .success()
    );
    assert!(
        run(&[
            "namespaces",
            "--db",
            db,
            "create",
            "logs",
            "--if-not-exists"
        ])
        .status
        .success()
    );
    assert!(
        !run(&["namespaces", "--db", db, "drop", "logs"])
            .status
            .success(),
        "needs --yes"
    );
    let dropped = run(&["namespaces", "--db", db, "drop", "crm", "--yes"]);
    assert!(
        text(&dropped.stdout).contains("1 nodes"),
        "{}",
        text(&dropped.stdout)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transactions_on_a_file_and_on_a_server() {
    let dir = std::env::temp_dir().join(format!("grafeno-shell-tx-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("tx.redb");
    let db = db.to_str().unwrap();
    let script = "BEGIN; CREATE (:Note {n: 1}); ROLLBACK;\n\
                  BEGIN; CREATE (:Note {n: 2}); COMMIT;\n\
                  BEGIN; CREATE (:Note {n: 3}); RETURN 1 / 0; COMMIT;\n\
                  MATCH (n:Note) RETURN n.n;";
    let output = shell(&["--db", db, "-n", "k", "-c", script], None);
    let stdout = text(&output.stdout);
    assert!(
        !output.status.success(),
        "the failed transaction is an error"
    );
    assert!(
        stdout.contains(" 2\n(1 row)"),
        "{stdout}{}",
        text(&output.stderr)
    );

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // Killed when the test ends, failing or not (a child left running
    // would hold the test's output open).
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_grafeno"))
            .args(["serve", "--db", db, "--addr", &format!("127.0.0.1:{port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let url = format!("http://127.0.0.1:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the server did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let output = shell(&["--url", &url, "-n", "remote", "-c", script], None);
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains(" 2\n(1 row)"),
        "{stdout}{}",
        text(&output.stderr)
    );
    // Read only: a snapshot, no writes.
    let output = shell(
        &[
            "--url",
            &url,
            "-n",
            "k",
            "-c",
            "BEGIN READ ONLY; MATCH (n) RETURN count(n); CREATE (:Note); COMMIT;",
        ],
        None,
    );
    assert!(
        text(&output.stderr).contains("only reads"),
        "{}",
        text(&output.stderr)
    );
    drop(_server);
    let _ = std::fs::remove_dir_all(&dir);
}
