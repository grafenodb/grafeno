//! `grafeno shell`: an interactive console for GQ queries and administration,
//! on a database file or on a running server.

use std::cell::{Cell, RefCell};
use std::io::{BufRead, IsTerminal, Read};
use std::time::Instant;

use grafeno::Grafeno;
use grafeno_memory::{ContextOptions, RecallQuery, context_for, recall};
use serde_json::{Value, json};

const HELP: &str = "\
Statements end with `;` and may span lines. Meta-commands start with `\\`:

  \\?                 this help
  \\q                 quit
  \\ns                list namespaces (empty ones too)
  \\ns create NAME    create an empty namespace
  \\ns drop NAME --yes  delete a namespace and everything in it
  \\use NAME          switch to another namespace
  \\stats             statistics of the namespace
  \\check             check indexes and counters
  \\rebuild           rebuild indexes and counters from the records
  \\search TEXT       full-text search
  \\recall TEXT       hybrid recall (text and graph)
  \\context TEXT      the context an agent would get (1500 tokens)
  \\export FILE       export the namespace (\\export FILE * for all)
  \\import FILE       import an export
  \\format table|json how results are shown
  \\timing on|off     show how long each statement takes
  \\i FILE            run the statements in a file

Example: MATCH (p:Person)-[:works_at]->(c) RETURN p.name, c.name LIMIT 10;";

/// Where the shell sends its work: a database file opened directly, or a
/// server over HTTP. Both answer with the HTTP API's JSON shapes.
///
/// A remote backend may know several members of a cluster: it talks to
/// one, moves to the next when that one does not answer, and sends the
/// last position it saw (`x-grafeno-after`) so it reads its own writes on
/// whichever member it lands.
pub enum Backend {
    Local(Grafeno),
    Remote {
        agent: ureq::Agent,
        urls: Vec<String>,
        current: Cell<usize>,
        position: Cell<u64>,
        token: RefCell<Option<String>>,
        /// A signed-in user's refresh token (14.1).
        refresh: RefCell<Option<String>>,
    },
}

impl Backend {
    /// `urls` is one address or several separated by commas.
    pub fn remote(urls: &str, token: Option<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let mut list: Vec<String> = urls
            .split(',')
            .map(|url| url.trim().trim_end_matches('/').to_owned())
            .filter(|url| !url.is_empty())
            .collect();
        if list.is_empty() {
            list.push(urls.trim().to_owned());
        }
        Backend::Remote {
            agent,
            urls: list,
            current: Cell::new(0),
            position: Cell::new(0),
            token: RefCell::new(token),
            refresh: RefCell::new(None),
        }
    }

    /// Signs in as a user: the session's tokens replace any token given,
    /// and the access token is renewed when it expires.
    pub fn sign_in(&self, name: &str, password: &str) -> Result<String, String> {
        let Backend::Remote { token, refresh, .. } = self else {
            return Err("signing in needs a server (--url)".into());
        };
        token.replace(None);
        let answer = self.call(
            "POST",
            "/v1/auth/signin",
            Some(json!({ "name": name, "password": password })),
        )?;
        token.replace(answer["access_token"].as_str().map(str::to_owned));
        refresh.replace(answer["refresh_token"].as_str().map(str::to_owned));
        Ok(answer["account"]["name"]
            .as_str()
            .unwrap_or(name)
            .to_owned())
    }

    /// New tokens for an expired access token; whether it worked.
    fn renew(&self) -> bool {
        let Backend::Remote { token, refresh, .. } = self else {
            return false;
        };
        let Some(current) = refresh.borrow().clone() else {
            return false;
        };
        token.replace(None);
        let body = json!({ "refresh_token": current }).to_string();
        match self.send_once("POST", "/v1/auth/refresh", Some(body), true) {
            Ok((200, text)) => {
                let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                token.replace(answer["access_token"].as_str().map(str::to_owned));
                refresh.replace(answer["refresh_token"].as_str().map(str::to_owned));
                true
            }
            _ => false,
        }
    }

    fn describe(&self) -> String {
        match self {
            Backend::Local(_) => "a local database file".into(),
            Backend::Remote { urls, current, .. } => urls[current.get()].clone(),
        }
    }

    /// Sends a request to the server and returns its JSON (or text) body;
    /// an expired session is renewed once.
    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<String>,
        json_body: bool,
    ) -> Result<String, String> {
        let (mut status, mut text) = self.send_once(method, path, body.clone(), json_body)?;
        let code = |text: &str| {
            serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|body| body["error"]["code"].as_str().map(str::to_owned))
        };
        if status == 401 && code(&text).as_deref() == Some("session_expired") && self.renew() {
            (status, text) = self.send_once(method, path, body, json_body)?;
        }
        if (200..300).contains(&status) {
            return Ok(text);
        }
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|body| body["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or(text);
        Err(format!("{message} (HTTP {status})"))
    }

    /// One request: its status and body.
    fn send_once(
        &self,
        method: &str,
        path: &str,
        body: Option<String>,
        json_body: bool,
    ) -> Result<(u16, String), String> {
        let Backend::Remote {
            agent,
            urls,
            current,
            position,
            token,
            ..
        } = self
        else {
            unreachable!("only remote backends send requests")
        };
        let content_type = if json_body {
            "application/json"
        } else {
            "application/x-ndjson"
        };
        // The member in use first, then the others in turn.
        let mut failures = Vec::new();
        let mut answered = None;
        for step in 0..urls.len() {
            let at = (current.get() + step) % urls.len();
            let url = &urls[at];
            let uri = format!("{url}{path}");
            let mut request = match method {
                "GET" => agent.get(&uri).force_send_body(),
                _ => agent.post(&uri),
            };
            if let Some(token) = token.borrow().as_deref() {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            if position.get() > 0 {
                request = request.header("x-grafeno-after", position.get().to_string());
            }
            let result = match &body {
                Some(body) => request
                    .header("content-type", content_type)
                    .send(body.as_str()),
                None => request.send_empty(),
            };
            match result {
                Ok(response) => {
                    current.set(at);
                    answered = Some(response);
                    break;
                }
                Err(error) => failures.push(format!("cannot reach {url}: {error}")),
            }
        }
        let Some(mut response) = answered else {
            return Err(failures.join("; "));
        };
        if let Some(seen) = response
            .headers()
            .get("x-grafeno-position")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            position.set(position.get().max(seen));
        }
        let status = response.status();
        let text = response
            .body_mut()
            .with_config()
            .limit(1 << 30)
            .read_to_string()
            .map_err(|error| format!("cannot read the response: {error}"))?;
        Ok((status.as_u16(), text))
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let text = self.send(method, path, body.map(|b| b.to_string()), true)?;
        serde_json::from_str(&text).map_err(|error| format!("invalid response: {error}"))
    }

    fn query(&self, ns: &str, query: &str) -> Result<Value, String> {
        match self {
            Backend::Local(db) => grafeno_query::execute_with(
                db,
                ns,
                query,
                &Default::default(),
                &crate::access_cli::local_limits(),
            )
            .map(|result| result.to_json())
            .map_err(|error| error.to_string()),
            Backend::Remote { .. } => {
                self.call("POST", &path(ns, "query"), Some(json!({ "query": query })))
            }
        }
    }

    fn local<T: serde::Serialize>(
        db: &Grafeno,
        work: impl FnOnce(&Grafeno) -> grafeno::Result<T>,
    ) -> Result<Value, String> {
        let value = work(db).map_err(|error| error.to_string())?;
        serde_json::to_value(value).map_err(|error| error.to_string())
    }

    fn namespaces(&self) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| db.begin_read()?.namespaces()),
            Backend::Remote { .. } => self.call("GET", "/v1/namespaces", None),
        }
    }

    fn create_namespace(&self, ns: &str) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| {
                db.write(|tx| {
                    tx.create_namespace(ns, None)?;
                    tx.namespace(ns)
                })
            }),
            Backend::Remote { .. } => {
                self.call("POST", "/v1/namespaces", Some(json!({ "name": ns })))
            }
        }
    }

    fn drop_namespace(&self, ns: &str) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| db.write(|tx| tx.drop_namespace(ns))),
            Backend::Remote { .. } => {
                self.call("DELETE", &format!("/v1/namespaces/{}", encode(ns)), None)
            }
        }
    }

    fn namespace_exists(&self, ns: &str) -> Result<bool, String> {
        Ok(self
            .namespaces()?
            .as_array()
            .is_some_and(|all| all.iter().any(|info| info["name"] == ns)))
    }

    fn stats(&self, ns: &str) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| db.begin_read()?.stats(ns)),
            Backend::Remote { .. } => self.call("GET", &path(ns, "stats"), None),
        }
    }

    fn check(&self, ns: &str) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| db.begin_read()?.check(ns)),
            Backend::Remote { .. } => self.call("GET", &path(ns, "check"), None),
        }
    }

    fn rebuild(&self, ns: &str) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| db.write(|tx| tx.rebuild_indexes(ns))),
            Backend::Remote { .. } => self.call("POST", &path(ns, "rebuild"), None),
        }
    }

    fn search(&self, ns: &str, text: &str) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| db.begin_read()?.search_text(ns, text, 20)),
            Backend::Remote { .. } => self.call(
                "POST",
                &path(ns, "search/text"),
                Some(json!({ "query": text, "limit": 20 })),
            ),
        }
    }

    fn recall(&self, ns: &str, text: &str) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| {
                recall(
                    &db.begin_read()?,
                    ns,
                    &RecallQuery::new().text(text).limit(15),
                )
            }),
            Backend::Remote { .. } => self.call(
                "POST",
                &path(ns, "recall"),
                Some(json!({ "text": text, "limit": 15 })),
            ),
        }
    }

    fn context(&self, ns: &str, text: &str) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| {
                let query = RecallQuery::new().text(text).limit(20);
                context_for(
                    &db.begin_read()?,
                    ns,
                    &query,
                    &ContextOptions::with_budget(1_500),
                )
            }),
            Backend::Remote { .. } => self.call(
                "POST",
                &path(ns, "context"),
                Some(json!({ "text": text, "limit": 20, "budget": 1500 })),
            ),
        }
    }

    fn export(&self, namespaces: &[String]) -> Result<String, String> {
        match self {
            Backend::Local(db) => {
                let mut out = Vec::new();
                db.begin_read()
                    .and_then(|read| read.export(namespaces, &mut out))
                    .map_err(|error| error.to_string())?;
                String::from_utf8(out).map_err(|error| error.to_string())
            }
            Backend::Remote { .. } => {
                let query = if namespaces.is_empty() {
                    String::new()
                } else {
                    format!("?ns={}", encode(&namespaces.join(",")))
                };
                self.send("GET", &format!("/v1/export{query}"), None, false)
            }
        }
    }

    fn import(&self, dump: String) -> Result<Value, String> {
        match self {
            Backend::Local(db) => Self::local(db, |db| db.import(dump.as_bytes())),
            Backend::Remote { .. } => {
                let text = self.send("POST", "/v1/import", Some(dump), false)?;
                serde_json::from_str(&text).map_err(|error| error.to_string())
            }
        }
    }
}

/// Percent-encodes a path segment or query value.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn path(ns: &str, rest: &str) -> String {
    format!("/v1/{}/{rest}", encode(ns))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Table,
    Json,
}

pub struct Shell {
    backend: Backend,
    namespace: String,
    format: Format,
    timing: bool,
    /// Whether anything failed (the exit code of scripts).
    pub failed: bool,
    /// Scripts stop at the first statement or meta-command that fails.
    pub stop_on_error: bool,
}

impl Shell {
    pub fn new(backend: Backend, namespace: String, json: bool) -> Self {
        Self {
            backend,
            namespace,
            format: if json { Format::Json } else { Format::Table },
            timing: false,
            failed: false,
            stop_on_error: false,
        }
    }

    /// Runs a script: statements separated by `;` and meta-command lines.
    pub fn run_script(&mut self, script: &str) {
        let stop = |shell: &Self| shell.stop_on_error && shell.failed;
        let mut pending = String::new();
        for line in script.lines() {
            if pending.trim().is_empty() && line.trim_start().starts_with('\\') {
                if !self.meta(line.trim()) || stop(self) {
                    return;
                }
                continue;
            }
            pending.push_str(line);
            pending.push('\n');
            let (statements, rest) = split_statements(&pending);
            for statement in statements {
                self.statement(&statement);
                if stop(self) {
                    return;
                }
            }
            pending = rest;
        }
        if !pending.trim().is_empty() {
            self.statement(&pending);
        }
    }

    /// The interactive loop, with line editing and history.
    pub fn interactive(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        use rustyline::error::ReadlineError;

        println!(
            "grafeno {} shell on {}. Type \\? for help, \\q to quit.",
            env!("CARGO_PKG_VERSION"),
            self.backend.describe()
        );
        let mut editor = rustyline::DefaultEditor::new()?;
        let history = history_file();
        if let Some(history) = &history {
            let _ = editor.load_history(history);
        }
        let mut pending = String::new();
        loop {
            let prompt = if pending.is_empty() {
                format!("{}=> ", self.namespace)
            } else {
                format!("{}-> ", self.namespace)
            };
            match editor.readline(&prompt) {
                Ok(line) => {
                    if pending.trim().is_empty() && line.trim_start().starts_with('\\') {
                        let _ = editor.add_history_entry(line.trim());
                        if !self.meta(line.trim()) {
                            break;
                        }
                        continue;
                    }
                    pending.push_str(&line);
                    pending.push('\n');
                    let (statements, rest) = split_statements(&pending);
                    if !statements.is_empty() {
                        let entered = pending[..pending.len() - rest.len()].trim();
                        let _ = editor.add_history_entry(entered.replace('\n', " "));
                    }
                    for statement in statements {
                        self.statement(&statement);
                    }
                    pending = rest;
                    if pending.trim().is_empty() {
                        pending.clear();
                    }
                }
                Err(ReadlineError::Interrupted) => pending.clear(),
                Err(ReadlineError::Eof) => break,
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(history) = &history {
            let _ = editor.save_history(history);
        }
        Ok(())
    }

    fn report(&mut self, result: Result<(), String>) {
        if let Err(message) = result {
            self.failed = true;
            eprintln!("error: {message}");
        }
    }

    fn statement(&mut self, statement: &str) {
        let statement = statement.trim();
        if statement.is_empty() {
            return;
        }
        let started = Instant::now();
        let result = self.backend.query(&self.namespace, statement);
        let elapsed = started.elapsed();
        let result = result.map(|output| {
            match self.format {
                Format::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&output).unwrap_or_default()
                ),
                Format::Table => print_result(&output),
            }
            if self.timing {
                println!("Time: {:.3} ms", elapsed.as_secs_f64() * 1e3);
            }
        });
        self.report(result);
    }

    /// Runs a meta-command. Returns `false` to quit.
    fn meta(&mut self, line: &str) -> bool {
        let (command, argument) = match line.split_once(char::is_whitespace) {
            Some((command, argument)) => (command, argument.trim()),
            None => (line, ""),
        };
        let ns = self.namespace.clone();
        let result: Result<(), String> = match command {
            "\\q" | "\\quit" => return false,
            "\\?" | "\\help" | "\\h" => {
                println!("{HELP}");
                Ok(())
            }
            "\\ns" if argument.starts_with("create ") => {
                let name = argument["create ".len()..].trim();
                self.backend.create_namespace(name).map(|_| {
                    self.namespace = name.to_owned();
                    println!("Created namespace {name:?}; now on it.");
                })
            }
            "\\ns" if argument.starts_with("drop ") => {
                let rest = argument["drop ".len()..].trim();
                match rest.strip_suffix("--yes").map(str::trim) {
                    None => Err(format!(
                        "this deletes {rest:?} and everything in it; to confirm: \\ns drop {rest} --yes"
                    )),
                    Some(name) => self.backend.drop_namespace(name).map(|dropped| {
                        println!(
                            "Dropped namespace {name:?} ({} nodes, {} edges).",
                            dropped["nodes"], dropped["edges"]
                        );
                    }),
                }
            }
            "\\ns" if !argument.is_empty() => Err(format!(
                "unknown \\ns command {argument:?} (\\ns, \\ns create NAME, \\ns drop NAME --yes)"
            )),
            "\\ns" => self.backend.namespaces().map(|namespaces| {
                let rows: Vec<Vec<Value>> = namespaces
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|info| {
                        vec![
                            info["name"].clone(),
                            info["nodes"].clone(),
                            info["edges"].clone(),
                        ]
                    })
                    .collect();
                print_table(&["namespace", "nodes", "edges"], &rows);
            }),
            "\\use" | "\\c" if !argument.is_empty() => {
                self.namespace = argument.to_owned();
                match self.backend.namespace_exists(argument) {
                    Ok(true) => println!("Now on namespace {argument:?}."),
                    _ => println!(
                        "Now on namespace {argument:?}, which does not exist yet: the first write \
                         creates it (or \\ns create {argument})."
                    ),
                }
                Ok(())
            }
            "\\stats" => self.backend.stats(&ns).map(|stats| print_json(&stats)),
            "\\check" => self.backend.check(&ns).map(|report| {
                let problems = report["problems"].as_array().cloned().unwrap_or_default();
                println!(
                    "{}: {} nodes, {} edges, {} texts, {} vectors, {} dangling links",
                    ns,
                    report["nodes"],
                    report["edges"],
                    report["texts"],
                    report["vectors"],
                    report["dangling_links"]
                );
                if problems.is_empty() {
                    println!("ok");
                } else {
                    for problem in &problems {
                        println!("  - {}", problem.as_str().unwrap_or_default());
                    }
                }
            }),
            "\\rebuild" => self.backend.rebuild(&ns).map(|report| print_json(&report)),
            "\\search" if !argument.is_empty() => self.backend.search(&ns, argument).map(|hits| {
                let rows: Vec<Vec<Value>> = hits
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|hit| vec![hit["node"].clone(), hit["score"].clone()])
                    .collect();
                print_table(&["node", "score"], &rows);
            }),
            "\\recall" if !argument.is_empty() => {
                self.backend.recall(&ns, argument).map(|results| {
                    let rows: Vec<Vec<Value>> = results
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|result| {
                            let why: Vec<String> = result["signals"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|signal| {
                                    signal["kind"].as_str().unwrap_or_default().to_owned()
                                })
                                .collect();
                            vec![
                                result["node"].clone(),
                                result["score"].clone(),
                                json!(why.join(", ")),
                            ]
                        })
                        .collect();
                    print_table(&["node", "score", "why"], &rows);
                })
            }
            "\\context" if !argument.is_empty() => {
                self.backend.context(&ns, argument).map(|context| {
                    print!("{}", context["rendered"].as_str().unwrap_or_default());
                    println!("({} tokens)", context["tokens"]);
                })
            }
            "\\export" if !argument.is_empty() => {
                let mut parts = argument.split_whitespace();
                let file = parts.next().unwrap_or_default().to_owned();
                let rest: Vec<String> = parts.map(str::to_owned).collect();
                let namespaces = match rest.as_slice() {
                    [] => vec![ns.clone()],
                    [all] if all == "*" => Vec::new(),
                    names => names.to_vec(),
                };
                self.backend.export(&namespaces).and_then(|dump| {
                    let lines = dump.lines().count().saturating_sub(1);
                    std::fs::write(&file, dump)
                        .map_err(|error| format!("cannot write {file}: {error}"))?;
                    println!("Wrote {lines} records to {file}.");
                    Ok(())
                })
            }
            "\\import" if !argument.is_empty() => std::fs::read_to_string(argument)
                .map_err(|error| format!("cannot read {argument}: {error}"))
                .and_then(|dump| self.backend.import(dump))
                .map(|stats| {
                    println!(
                        "Imported {} namespaces, {} nodes, {} edges.",
                        stats["namespaces"], stats["nodes"], stats["edges"]
                    );
                }),
            "\\format" => match argument {
                "table" => {
                    self.format = Format::Table;
                    Ok(())
                }
                "json" => {
                    self.format = Format::Json;
                    Ok(())
                }
                _ => Err("use \\format table or \\format json".into()),
            },
            "\\timing" => {
                self.timing = match argument {
                    "on" => true,
                    "off" => false,
                    _ => !self.timing,
                };
                println!("Timing is {}.", if self.timing { "on" } else { "off" });
                Ok(())
            }
            "\\i" if !argument.is_empty() => match std::fs::read_to_string(argument) {
                Ok(script) => {
                    self.run_script(&script);
                    Ok(())
                }
                Err(error) => Err(format!("cannot read {argument}: {error}")),
            },
            "\\use" | "\\c" | "\\search" | "\\recall" | "\\context" | "\\export" | "\\import"
            | "\\i" => Err(format!("{command} needs an argument; see \\?")),
            other => Err(format!("unknown command {other}; see \\?")),
        };
        self.report(result);
        true
    }
}

/// Reads a whole script from standard input when it is not a terminal.
pub fn stdin_script() -> Option<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut script = String::new();
    stdin.lock().read_to_string(&mut script).ok()?;
    Some(script)
}

/// Reads a script file, or standard input for `-`.
pub fn read_script(path: &str) -> std::io::Result<String> {
    if path == "-" {
        let mut script = String::new();
        for line in std::io::stdin().lock().lines() {
            script.push_str(&line?);
            script.push('\n');
        }
        return Ok(script);
    }
    std::fs::read_to_string(path)
}

fn history_file() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(std::path::PathBuf::from(home).join(".grafeno_history"))
}

/// Splits `text` at `;` outside strings, backquoted names and `//`
/// comments. Returns the complete statements and what is left after them.
pub fn split_statements(text: &str) -> (Vec<String>, String) {
    let mut statements = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && q != '`' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '/' if chars.peek().is_some_and(|&(_, next)| next == '/') => {
                    while chars.peek().is_some_and(|&(_, next)| next != '\n') {
                        chars.next();
                    }
                }
                ';' => {
                    let statement = text[start..at].trim();
                    if !statement.is_empty() {
                        statements.push(statement.to_owned());
                    }
                    start = at + 1;
                }
                _ => {}
            },
        }
    }
    (statements, text[start..].to_owned())
}

// ---------------------------------------------------------------- output

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

fn print_result(output: &Value) {
    let columns: Vec<String> = output["columns"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| c.as_str().unwrap_or_default().to_owned())
        .collect();
    let rows: Vec<Vec<Value>> = output["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| row.as_array().cloned().unwrap_or_default())
        .collect();
    if !columns.is_empty() {
        let names: Vec<&str> = columns.iter().map(String::as_str).collect();
        print_table(&names, &rows);
    }
    let stats = &output["stats"];
    let changes: Vec<String> = [
        ("nodes_created", "node created", "nodes created"),
        ("nodes_deleted", "node deleted", "nodes deleted"),
        ("edges_created", "edge created", "edges created"),
        ("edges_deleted", "edge deleted", "edges deleted"),
        ("edges_retracted", "edge retracted", "edges retracted"),
        ("properties_set", "property set", "properties set"),
        ("labels_added", "label added", "labels added"),
        ("labels_removed", "label removed", "labels removed"),
    ]
    .iter()
    .filter_map(|(key, one, many)| {
        let count = stats[key].as_u64().unwrap_or(0);
        match count {
            0 => None,
            1 => Some(format!("1 {one}")),
            n => Some(format!("{n} {many}")),
        }
    })
    .collect();
    if !changes.is_empty() {
        println!("{}", changes.join(", "));
    } else if columns.is_empty() {
        println!("No changes.");
    }
    // PROFILE: what each step did.
    if let Some(profile) = output["profile"].as_array() {
        println!();
        let rows: Vec<Vec<Value>> = profile
            .iter()
            .map(|step| {
                vec![
                    step["step"].clone(),
                    step["operation"].clone(),
                    step["rows"].clone(),
                    step["reads"].clone(),
                    Value::String(format!("{:.2}", step["time_ms"].as_f64().unwrap_or(0.0))),
                    step["details"].clone(),
                ]
            })
            .collect();
        print_table(
            &["step", "operation", "rows", "reads", "ms", "details"],
            &rows,
        );
    }
}

fn print_table(columns: &[&str], rows: &[Vec<Value>]) {
    const MAX_WIDTH: usize = 60;
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| truncate(&cell(value), MAX_WIDTH))
                .collect()
        })
        .collect();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (index, text) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(text.chars().count());
            }
        }
    }
    let line = |values: Vec<String>| {
        let padded: Vec<String> = values
            .iter()
            .zip(&widths)
            .map(|(text, width)| format!("{text}{}", " ".repeat(width - text.chars().count())))
            .collect();
        println!(" {}", padded.join(" | ").trim_end());
    };
    line(columns.iter().map(|c| c.to_string()).collect());
    println!(
        "{}",
        widths
            .iter()
            .map(|width| "-".repeat(width + 2))
            .collect::<Vec<_>>()
            .join("+")
    );
    for row in cells {
        line(row);
    }
    let count = rows.len();
    println!("({count} {})", if count == 1 { "row" } else { "rows" });
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max - 1).collect();
    format!("{cut}…")
}

/// How a value reads in a table. Nodes and edges are recognized by their
/// record fields.
fn cell(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::String(s) => s.clone(),
        Value::Object(object)
            if object.contains_key("labels") && object.contains_key("created_at") =>
        {
            let labels: Vec<&str> = object["labels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let props = &object["props"];
            let name = ["name", "title", "label"]
                .iter()
                .find_map(|key| props[key].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    let id = object["id"].as_str().unwrap_or_default();
                    id.get(id.len().saturating_sub(8)..)
                        .unwrap_or(id)
                        .to_owned()
                });
            format!("({name}:{})", labels.join(":"))
        }
        Value::Object(object)
            if object.contains_key("from") && object.contains_key("recorded_at") =>
        {
            format!("[:{}]", object["label"].as_str().unwrap_or_default())
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(cell).collect();
            format!("[{}]", items.join(", "))
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_statements_outside_strings_and_comments() {
        let (statements, rest) = split_statements("RETURN 'a;b'; // c;\nRETURN `x;y`;\nMATCH (n)");
        assert_eq!(statements, ["RETURN 'a;b'", "// c;\nRETURN `x;y`"]);
        assert_eq!(rest.trim(), "MATCH (n)");
        let (statements, rest) = split_statements("RETURN \"it\\\"s;\";");
        assert_eq!(statements, ["RETURN \"it\\\"s;\""]);
        assert!(rest.is_empty());
    }

    #[test]
    fn cells_show_nodes_edges_and_values() {
        let node = json!({ "id": "0190f5c2-7a3e-7cde-8f01-00000000000a", "labels": ["Person", "Admin"],
                           "props": { "name": "Ana" }, "created_at": 0 });
        assert_eq!(cell(&node), "(Ana:Person:Admin)");
        let edge = json!({ "label": "knows", "from": "a", "to": "b", "recorded_at": 0 });
        assert_eq!(cell(&edge), "[:knows]");
        assert_eq!(cell(&json!([1, "x", null])), "[1, x, null]");
        assert_eq!(truncate(&"x".repeat(70), 60).chars().count(), 60);
    }
}
