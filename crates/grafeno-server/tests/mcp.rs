use grafeno::Grafeno;
use grafeno_server::mcp::{McpAccess, McpServer};
use serde_json::{Value, json};

fn server() -> McpServer {
    McpServer::new(Grafeno::in_memory().unwrap(), "agent".into())
}

fn request(server: &McpServer, id: u64, method: &str, params: Value) -> Value {
    let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let response = server
        .handle_line(&message.to_string())
        .expect("a response");
    assert_eq!(response["id"], id);
    response
}

/// Calls a tool and returns its result, failing on protocol errors.
fn call(server: &McpServer, tool: &str, arguments: Value) -> Value {
    let response = request(
        server,
        1,
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    );
    response
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("protocol error: {response}"))
}

fn structured(result: &Value) -> &Value {
    assert_eq!(result["isError"], false, "{result}");
    &result["structuredContent"]
}

#[test]
fn handshake_and_tool_list() {
    let server = server();
    let response = request(
        &server,
        1,
        "initialize",
        json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
    );
    assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(response["result"]["serverInfo"]["name"], "grafeno");
    assert!(response["result"]["capabilities"]["tools"].is_object());

    let response = request(
        &server,
        2,
        "initialize",
        json!({ "protocolVersion": "1999-01-01" }),
    );
    assert_eq!(response["result"]["protocolVersion"], "2025-06-18");

    let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert!(server.handle_line(&notification.to_string()).is_none());

    let response = request(&server, 3, "tools/list", json!({}));
    let names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "remember",
            "recall",
            "context",
            "relate",
            "update",
            "forget",
            "get_node",
            "query",
            "merge",
            "undo_merge",
            "overview",
            "changes",
            "summarize_communities",
            "set_summary",
            "extract",
            "add_mentions",
            "review",
            "resolve_review",
            "find_matches",
            "maintain",
            "consolidate",
            "checkpoint",
            "resume",
            "ingest",
            "contradictions",
            "timeline",
            "feedback",
            "palace_create",
            "palace_create_room",
            "palace_anchor",
            "palace_map",
            "palace_walk",
            "palace_enter",
            "palace_route",
            "palace_review",
            "palace_reflect"
        ]
    );
    for tool in response["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object");
    }

    assert_eq!(request(&server, 4, "ping", json!({}))["result"], json!({}));
}

#[test]
fn protocol_errors() {
    let server = server();
    let response = server.handle_line("{not json").unwrap();
    assert_eq!(response["error"]["code"], -32700);
    assert_eq!(
        request(&server, 5, "nope", json!({}))["error"]["code"],
        -32601
    );
    let response = request(&server, 6, "tools/call", json!({ "name": "nope" }));
    assert_eq!(response["error"]["code"], -32602);
}

#[test]
fn remember_relate_recall_and_context() {
    let server = server();
    let ana = call(
        &server,
        "remember",
        json!({ "name": "Ana", "text": "Backend developer who loves Rust", "labels": ["Person"] }),
    );
    let ana = structured(&ana)["id"].as_str().unwrap().to_owned();
    let grafeno = call(
        &server,
        "remember",
        json!({ "name": "Grafeno", "text": "Graph engine for agent memory", "labels": ["Project"],
                "links": [{ "label": "maintained_by", "to": ana, "valid_from": "2026-11-03" }] }),
    );
    let grafeno = structured(&grafeno)["id"].as_str().unwrap().to_owned();
    let preference = call(
        &server,
        "remember",
        json!({ "text": "Ana prefers answers in Spanish", "labels": ["Preference"] }),
    );
    let preference = structured(&preference)["id"].as_str().unwrap().to_owned();
    let fact = call(
        &server,
        "relate",
        json!({ "from": preference, "label": "about", "to": ana }),
    );
    structured(&fact);

    let recalled = call(
        &server,
        "recall",
        json!({ "query": "who maintains the graph engine?" }),
    );
    let results = structured(&recalled)["results"].as_array().unwrap();
    assert_eq!(results[0]["id"], grafeno.as_str());
    assert_eq!(results[0]["why"][0], "text match #1");
    let ana_result = results
        .iter()
        .find(|r| r["id"] == ana.as_str())
        .expect("Ana via the graph");
    assert!(
        ana_result["why"]
            .as_array()
            .unwrap()
            .iter()
            .any(|why| why == "1 hop(s) from Grafeno"),
        "{ana_result}"
    );

    let context = call(&server, "context", json!({ "query": "Ana", "budget": 300 }));
    assert_eq!(context["isError"], false);
    let text = context["content"][0]["text"].as_str().unwrap();
    // The short preference matches "Ana" best. Unnamed memories show only
    // their labels here, and the start of their text where facts name them.
    assert!(
        text.starts_with("## Memory\n[1] (Preference)\nAna prefers answers in Spanish\n"),
        "{text}"
    );
    assert!(text.contains("[2] Ana (Person)"), "{text}");
    assert!(
        text.contains("\n\n## Facts\n") && !text.contains("\n\n\n"),
        "{text}"
    );
    assert!(
        text.contains("- “Ana prefers answers in Spanish” about Ana"),
        "{text}"
    );
    assert!(
        text.contains("- Grafeno maintained_by Ana (from 2026-11-03)"),
        "{text}"
    );

    let empty = call(
        &server,
        "context",
        json!({ "query": "quantum chromodynamics" }),
    );
    assert_eq!(empty["content"][0]["text"], "No relevant memories found.");
}

#[test]
fn update_get_and_forget() {
    let server = server();
    let ana = call(
        &server,
        "remember",
        json!({ "name": "Ana", "text": "Works at Acme", "labels": ["Person"] }),
    );
    let ana = structured(&ana)["id"].as_str().unwrap().to_owned();
    let acme = call(
        &server,
        "remember",
        json!({ "name": "Acme", "text": "A company", "labels": ["Company"] }),
    );
    let acme = structured(&acme)["id"].as_str().unwrap().to_owned();
    let job = call(
        &server,
        "relate",
        json!({ "from": ana, "label": "works_at", "to": acme, "valid_from": "2024-01-01" }),
    );
    let job = structured(&job)["id"].as_str().unwrap().to_owned();

    let updated = call(
        &server,
        "update",
        json!({ "id": ana, "text": "Freelance developer", "props": { "city": "Lima" } }),
    );
    let node = structured(&updated);
    assert_eq!(node["text"], "Freelance developer");
    assert_eq!(node["props"]["city"], "Lima");
    assert_eq!(node["facts"][0]["label"], "works_at");
    assert_eq!(node["facts"][0]["other"]["name"], "Acme");
    assert_eq!(node["facts"][0]["valid_from"], "2024-01-01");

    let forgotten = call(&server, "forget", json!({ "id": job, "at": "2026-06-01" }));
    assert_eq!(
        structured(&forgotten)["result"],
        "invalidated fact (kept in history)"
    );
    let node = call(&server, "get_node", json!({ "id": ana }));
    assert_eq!(structured(&node)["facts"][0]["valid_to"], "2026-06-01");

    let wrong = call(&server, "forget", json!({ "id": ana, "mode": "retract" }));
    assert_eq!(wrong["isError"], true);
    let deleted = call(&server, "forget", json!({ "id": ana }));
    assert_eq!(structured(&deleted)["result"], "deleted node and its edges");
    let missing = call(&server, "get_node", json!({ "id": ana }));
    assert_eq!(missing["isError"], true);

    let bad = call(&server, "remember", json!({ "txt": "typo" }));
    assert_eq!(bad["isError"], true);
    assert!(
        bad["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("invalid arguments")
    );
}

#[test]
fn read_only_queries() {
    let server = server();
    let ana = call(
        &server,
        "remember",
        json!({ "name": "Ana", "text": "Engineer", "labels": ["Person"] }),
    );
    let ana = structured(&ana)["id"].as_str().unwrap().to_owned();
    let acme = call(
        &server,
        "remember",
        json!({ "name": "Acme", "text": "Company", "labels": ["Company"] }),
    );
    let acme = structured(&acme)["id"].as_str().unwrap().to_owned();
    call(
        &server,
        "relate",
        json!({ "from": ana, "label": "works_at", "to": acme, "valid_from": "2024-01-01" }),
    );

    let result = call(
        &server,
        "query",
        json!({ "query": "MATCH (p:Person)-[w:works_at]->(c) WHERE c.name = $company RETURN p.name, date(w.@valid_from)",
                "params": { "company": "Acme" } }),
    );
    let output = structured(&result);
    assert_eq!(output["columns"], json!(["p.name", "date(w.@valid_from)"]));
    assert_eq!(output["rows"], json!([["Ana", "2024-01-01"]]));
    assert_eq!(output["truncated"], false);

    let write = call(&server, "query", json!({ "query": "MATCH (n) DELETE n" }));
    assert_eq!(write["isError"], true);
    assert!(
        write["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("writes")
    );
    let syntax = call(&server, "query", json!({ "query": "MATCH (n RETURN n" }));
    assert!(
        syntax["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("line 1")
    );
}

#[test]
fn remember_reuses_named_entities() {
    let server = server();
    let first = call(
        &server,
        "remember",
        json!({ "name": "Ana García", "labels": ["Person"], "text": "Ana writes Rust", "importance": 0.8 }),
    );
    let ana = structured(&first)["id"].as_str().unwrap().to_owned();
    assert_eq!(structured(&first)["reused"], false);

    // Same person, written differently: updated, not duplicated.
    let second = call(
        &server,
        "remember",
        json!({ "name": "ana garcia", "labels": ["Person", "Engineer"], "text": "Ana lives in Lima", "props": { "city": "Lima" } }),
    );
    assert_eq!(structured(&second)["id"], ana.as_str());
    assert_eq!(structured(&second)["reused"], true);
    // A company with the same name is a different node.
    let company = call(
        &server,
        "remember",
        json!({ "name": "Ana García", "labels": ["Company"], "text": "a shop" }),
    );
    assert_ne!(structured(&company)["id"], ana.as_str());

    let node = call(&server, "get_node", json!({ "id": ana }));
    let node = structured(&node);
    assert_eq!(node["labels"], json!(["Engineer", "Person"]));
    assert_eq!(node["props"]["city"], "Lima");
    assert_eq!(node["props"]["name"], "Ana García", "the first name stays");
    let text = node["text"].as_str().unwrap();
    assert!(
        text.contains("Ana writes Rust") && text.contains("Ana lives in Lima"),
        "{text}"
    );
    assert_eq!(node["importance"], 0.8);
    assert_eq!(node["accesses"], 0);

    // Recall records accesses and says why importance counted.
    let results = call(&server, "recall", json!({ "query": "Lima", "depth": 0 }));
    let first = &structured(&results)["results"][0];
    assert_eq!(first["id"], ana.as_str());
    assert!(
        first["why"]
            .as_array()
            .unwrap()
            .iter()
            .any(|why| why == "importance 0.80"),
        "{first}"
    );
    let node = call(&server, "get_node", json!({ "id": ana }));
    assert_eq!(structured(&node)["accesses"], 1);

    let updated = call(&server, "update", json!({ "id": ana, "importance": 0.3 }));
    assert_eq!(structured(&updated)["importance"], 0.3);
    let bad = call(&server, "update", json!({ "id": ana, "importance": 2 }));
    assert_eq!(bad["isError"], true);
}

#[test]
fn maintain_merge_and_consolidate() {
    let server = server();
    let id = |result: &Value| structured(result)["id"].as_str().unwrap().to_owned();
    let bob = id(&call(
        &server,
        "remember",
        json!({ "name": "Bob Smith", "labels": ["Person"], "text": "Bob" }),
    ));
    let robert = id(&call(
        &server,
        "remember",
        json!({ "name": "Bob Smith Jr", "labels": ["Person"], "text": "Robert" }),
    ));

    let report = call(&server, "maintain", json!({}));
    let report = structured(&report);
    let pair = &report["duplicates"][0];
    let ids = [
        pair["a"]["id"].as_str().unwrap(),
        pair["b"]["id"].as_str().unwrap(),
    ];
    assert!(
        ids.contains(&bob.as_str()) && ids.contains(&robert.as_str()),
        "{report}"
    );
    assert!(report["weak"].as_array().unwrap().is_empty());
    assert!(
        report["consolidation"].as_array().unwrap().is_empty(),
        "memories are too recent"
    );

    let merged = call(&server, "merge", json!({ "keep": bob, "merge": robert }));
    assert_eq!(structured(&merged)["id"], bob.as_str());
    let gone = call(&server, "get_node", json!({ "id": robert }));
    assert_eq!(gone["isError"], true);
    // Undone, and merged again.
    let merge_id = structured(&merged)["merge_id"].as_str().unwrap().to_owned();
    let undone = call(&server, "undo_merge", json!({ "merge_id": merge_id }));
    assert_eq!(structured(&undone)["restored"], robert.as_str(), "{undone}");
    assert_ne!(
        call(&server, "get_node", json!({ "id": robert }))["isError"],
        true
    );
    call(&server, "merge", json!({ "keep": bob, "merge": robert }));
    let report = call(&server, "maintain", json!({}));
    assert!(
        structured(&report)["duplicates"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let mut memories = Vec::new();
    for text in ["Bob likes chess", "Bob moved to Quito"] {
        let result = call(
            &server,
            "remember",
            json!({ "text": text, "links": [{ "label": "about", "to": bob }] }),
        );
        memories.push(id(&result));
    }
    let summary = call(
        &server,
        "consolidate",
        json!({ "summary": "Bob likes chess and lives in Quito.", "sources": memories, "about": bob }),
    );
    let summary_id = id(&summary);
    assert_eq!(structured(&summary)["summarized"], 2);

    let results = call(&server, "recall", json!({ "query": "Quito", "depth": 0 }));
    let found: Vec<&str> = structured(&results)["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        found,
        [summary_id.as_str()],
        "archived memories are left out"
    );
    let results = call(
        &server,
        "recall",
        json!({ "query": "Quito", "depth": 0, "include_archived": true }),
    );
    assert_eq!(structured(&results)["results"].as_array().unwrap().len(), 2);

    let empty = call(
        &server,
        "consolidate",
        json!({ "summary": "nothing", "sources": [] }),
    );
    assert_eq!(empty["isError"], true);
}

#[test]
fn agents_are_fenced_and_recorded() {
    let db = Grafeno::in_memory().unwrap();
    let server = McpServer::new(db, "agent".into()).with_access(McpAccess {
        allow: vec!["agent".into(), "team-*".into()],
        read_only: false,
        actor: Some(grafeno::Actor::new("assistant")),
    });
    let call = |name: &str, arguments: serde_json::Value| {
        let request = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
        server.handle_line(&request.to_string()).unwrap()
    };
    let ok = call(
        "remember",
        serde_json::json!({ "text": "Ana likes Rust", "labels": ["Note"] }),
    );
    assert_eq!(ok["result"]["isError"], false, "{ok}");
    let who = call(
        "query",
        serde_json::json!({ "query": "MATCH (n:Note) RETURN n.@recorded_by" }),
    );
    assert!(who.to_string().contains("assistant"), "{who}");
    let fenced = call(
        "recall",
        serde_json::json!({ "query": "Rust", "namespace": "other" }),
    );
    assert_eq!(fenced["result"]["isError"], true, "{fenced}");
    let allowed = call(
        "recall",
        serde_json::json!({ "query": "Rust", "namespace": "team-a" }),
    );
    assert_eq!(allowed["result"]["isError"], false, "{allowed}");

    let reader =
        McpServer::new(Grafeno::in_memory().unwrap(), "agent".into()).with_access(McpAccess {
            read_only: true,
            ..McpAccess::default()
        });
    let list = reader
        .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .unwrap();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"recall") && !names.contains(&"remember"),
        "{names:?}"
    );
    let refused = reader
        .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"text":"x"}}}"#)
        .unwrap();
    assert_eq!(refused["result"]["isError"], true);
}

#[test]
fn community_tools() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        for names in [
            ["Ana", "Bruno", "Carla", "Dario"],
            ["Eva", "Fabio", "Gala", "Hugo"],
        ] {
            let ids: Vec<grafeno::NodeId> = names
                .iter()
                .map(|name| {
                    tx.create_node("agent", grafeno::NewNode::new("Person").prop("name", *name))
                })
                .collect::<grafeno::Result<_>>()?;
            for i in 0..4 {
                for j in i + 1..4 {
                    tx.create_edge("agent", grafeno::NewEdge::new(ids[j], "works_with", ids[i]))?;
                }
            }
        }
        Ok(())
    })
    .unwrap();
    let server = McpServer::new(db, "agent".into());
    let empty = call(&server, "overview", json!({}));
    assert!(
        empty["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("summarize_communities")
    );
    let report = call(&server, "summarize_communities", json!({}));
    assert_eq!(structured(&report)["created"], 2, "{report}");
    let found = call(&server, "overview", json!({ "query": "Eva" }));
    let hit = &structured(&found)["communities"][0];
    assert!(hit["text"].as_str().unwrap().contains("Eva"), "{found}");
    let id = hit["id"].as_str().unwrap().to_owned();
    let opened = call(&server, "overview", json!({ "id": id }));
    assert_eq!(structured(&opened)["members"].as_array().unwrap().len(), 4);
    let set = call(
        &server,
        "set_summary",
        json!({ "id": id, "text": "The garden team." }),
    );
    assert_eq!(structured(&set)["custom"], true);
    let changes = call(&server, "changes", json!({ "since": 0 }));
    assert!(
        structured(&changes)["last"].as_u64().unwrap() > 0,
        "{changes}"
    );
    // Context puts matching summaries first.
    let context = call(&server, "context", json!({ "query": "garden team" }));
    let text = context["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("## Overview\n- The garden team."),
        "{text}"
    );
}

#[test]
fn checkpoint_and_resume_a_task() {
    let server = server();
    let id = |result: &Value| structured(result)["id"].as_str().unwrap().to_owned();
    let project = id(&call(
        &server,
        "remember",
        json!({ "name": "Billing", "labels": ["Project"], "text": "The billing service" }),
    ));
    let nothing = call(&server, "resume", json!({}));
    assert_eq!(structured(&nothing)["found"], false);

    let first = id(&call(
        &server,
        "checkpoint",
        json!({
            "task": "Migrate billing to events",
            "summary": "Producer done, consumer next.",
            "done": ["producer"],
            "next": ["consumer", "backfill"],
            "about": project,
        }),
    ));
    let second = call(
        &server,
        "checkpoint",
        json!({
            "task": "migrate  BILLING to events",
            "summary": "Consumer done; backfill left.",
            "done": ["producer", "consumer"],
            "next": ["backfill"],
            "decisions": ["at-least-once delivery"],
            "files": ["src/consumer.rs"],
            "about": project,
        }),
    );
    assert_eq!(
        structured(&second)["follows"],
        first.as_str(),
        "the same task in other words"
    );
    let second = id(&second);

    let resumed = call(
        &server,
        "resume",
        json!({ "task": "Migrate billing to events" }),
    );
    let resumed = structured(&resumed);
    assert_eq!(resumed["found"], true);
    assert_eq!(resumed["checkpoint"]["id"], second.as_str());
    assert_eq!(resumed["checkpoint"]["previous"], first.as_str());
    assert_eq!(resumed["checkpoint"]["next"], json!(["backfill"]));
    assert_eq!(resumed["checkpoints_of_this_task"], 2);
    assert_eq!(resumed["about"]["name"], "Billing");
    assert!(
        resumed["changed_since"].as_array().unwrap().is_empty(),
        "nothing was written after it: {resumed}"
    );

    // What is written after the latest checkpoint shows when resuming.
    call(
        &server,
        "remember",
        json!({ "text": "Backfill takes two hours" }),
    );
    let latest = call(&server, "resume", json!({ "about": project }));
    let changed = structured(&latest)["changed_since"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        changed
            .iter()
            .any(|c| c["what"].as_str().unwrap().contains("Backfill")),
        "{changed:?}"
    );

    let unknown = call(&server, "resume", json!({ "task": "something else" }));
    let unknown = structured(&unknown);
    assert_eq!(unknown["found"], false);
    assert_eq!(unknown["tasks"][0]["task"], "migrate  BILLING to events");
}

#[test]
fn ingest_cuts_documents_conversations_and_code() {
    let server = server();
    let doc = "# Runbook\n\nWhat to do when billing stops.\n\n## Restart\n\nRestart the consumer with systemctl.\n\n## Escalate\n\nCall Ana Torres at ana@example.com.";
    let done = call(
        &server,
        "ingest",
        json!({ "content": doc, "source": "docs/runbook.md" }),
    );
    let done = structured(&done).clone();
    assert_eq!(done["chunks"], 3, "{done}");
    assert_eq!(done["replaced"], 0);
    assert!(done["extraction"]["texts_read"].as_u64().unwrap() >= 3);
    let whole = done["id"].as_str().unwrap().to_owned();
    let node = call(&server, "get_node", json!({ "id": whole }));
    let node = structured(&node);
    assert_eq!(node["name"], "docs/runbook.md");
    assert_eq!(node["labels"], json!(["Document"]));

    // Recall finds the chunk, which says where it is.
    let found = call(
        &server,
        "recall",
        json!({ "query": "restart the consumer" }),
    );
    let top = &structured(&found)["results"][0];
    let chunk = call(&server, "get_node", json!({ "id": top["id"] }));
    let chunk = structured(&chunk);
    assert_eq!(chunk["props"]["part"], "Runbook > Restart", "{chunk}");
    assert_eq!(chunk["sources"], json!([whole]));

    // The same source again replaces its chunks.
    let again = call(
        &server,
        "ingest",
        json!({ "content": "# Runbook\n\nOnly this now.", "source": "docs/runbook.md", "extract": false }),
    );
    let again = structured(&again);
    assert_eq!(again["id"], whole.as_str());
    assert_eq!(again["replaced"], 3);
    assert_eq!(again["chunks"], 1);

    let chat = "Ana: Shall we ship Friday?\nBob: Yes, after the docs.\nAna: I will write them.";
    let chat = call(
        &server,
        "ingest",
        json!({ "content": chat, "kind": "conversation", "title": "Standup", "extract": false }),
    );
    assert_eq!(structured(&chat)["chunks"], 1);
    let code = "fn add(a: i32) -> i32 {\n    a + 1\n}\n\nfn sub(a: i32) -> i32 {\n    a - 1\n}\n";
    let code = call(
        &server,
        "ingest",
        json!({ "content": code, "kind": "code", "source": "src/math.rs", "max_chars": 100, "extract": false }),
    );
    assert_eq!(structured(&code)["chunks"], 1, "both fit in one chunk");
    let bad = call(&server, "ingest", json!({ "content": "x", "kind": "poem" }));
    assert_eq!(bad["isError"], true);

    let read_only = McpServer::new(Grafeno::in_memory().unwrap(), "agent".into()).with_access(
        grafeno_server::mcp::McpAccess {
            read_only: true,
            ..Default::default()
        },
    );
    for (tool, arguments) in [
        ("ingest", json!({ "content": "x" })),
        ("checkpoint", json!({ "task": "t", "summary": "s" })),
        ("feedback", json!({ "id": "x", "verdict": "useful" })),
    ] {
        assert_eq!(call(&read_only, tool, arguments)["isError"], true, "{tool}");
    }
}

#[test]
fn contradictions_timeline_and_feedback() {
    use grafeno::{Constraint, NewEdge, NewNode, OnConflict, Scope, Timestamp};
    let db = Grafeno::in_memory().unwrap();
    let (ana, acme, first) = db
        .write(|tx| {
            tx.create_constraint(
                "agent",
                Constraint::Edge {
                    name: "one_employer".into(),
                    label: "works_at".into(),
                    scope: Scope::Source,
                    on_conflict: OnConflict::Invalidate,
                },
            )?;
            let ana = tx.create_node(
                "agent",
                NewNode::new("Person").prop("name", "Ana").text("Ana"),
            )?;
            let acme = tx.create_node("agent", NewNode::new("Company").prop("name", "Acme"))?;
            let first = tx.create_edge(
                "agent",
                NewEdge::new(ana, "works_at", acme).valid_from(Timestamp::from_secs(1_577_836_800)),
            )?;
            Ok((ana, acme, first))
        })
        .unwrap();
    db.write(|tx| {
        let globex = tx.create_node("agent", NewNode::new("Company").prop("name", "Globex"))?;
        tx.create_edge(
            "agent",
            NewEdge::new(ana, "works_at", globex).valid_from(Timestamp::from_secs(1_704_067_200)),
        )
    })
    .unwrap();
    let server = McpServer::new(db, "agent".into());

    let found = call(
        &server,
        "contradictions",
        json!({ "node": ana.to_string() }),
    );
    let found = structured(&found);
    let first_one = &found["contradictions"][0];
    assert_eq!(first_one["constraint"], "one_employer", "{found}");
    assert_eq!(first_one["new"]["text"], "Ana works_at Globex");
    assert_eq!(first_one["old"]["text"], "Ana works_at Acme");
    let through_acme = call(
        &server,
        "contradictions",
        json!({ "node": acme.to_string(), "limit": 5 }),
    );
    assert_eq!(
        structured(&through_acme)["contradictions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let timeline = call(&server, "timeline", json!({ "id": ana.to_string() }));
    let timeline = structured(&timeline)["timeline"]
        .as_array()
        .unwrap()
        .clone();
    let kinds: Vec<&str> = timeline
        .iter()
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds[0], "fact_began", "2020 comes first: {kinds:?}");
    assert!(
        timeline
            .iter()
            .any(|m| m["kind"] == "fact_ended" && m["text"] == "ended: Ana works_at Acme"),
        "{timeline:?}"
    );
    assert!(timeline.iter().any(|m| m["kind"] == "contradiction"));
    let dates: Vec<&str> = timeline.iter().map(|m| m["at"].as_str().unwrap()).collect();
    assert!(dates.windows(2).all(|pair| pair[0] <= pair[1]), "{dates:?}");

    // Useful raises importance; obsolete archives; wrong retracts a fact.
    let useful = call(
        &server,
        "feedback",
        json!({ "id": ana.to_string(), "verdict": "useful", "note": "right person" }),
    );
    assert!(
        structured(&useful)["result"]
            .as_str()
            .unwrap()
            .contains("0.60"),
        "{useful}"
    );
    let node = call(&server, "get_node", json!({ "id": ana.to_string() }));
    assert_eq!(structured(&node)["props"]["feedback"], "useful");
    assert_eq!(structured(&node)["props"]["feedback_note"], "right person");
    call(
        &server,
        "feedback",
        json!({ "id": acme.to_string(), "verdict": "obsolete" }),
    );
    let node = call(&server, "get_node", json!({ "id": acme.to_string() }));
    assert!(
        structured(&node)["labels"]
            .as_array()
            .unwrap()
            .contains(&json!("Archived"))
    );
    let wrong = call(
        &server,
        "feedback",
        json!({ "id": first.to_string(), "verdict": "wrong" }),
    );
    assert!(
        structured(&wrong)["result"]
            .as_str()
            .unwrap()
            .contains("retracted")
    );
    for (id, verdict) in [(first.to_string(), "useful"), (ana.to_string(), "great")] {
        let refused = call(&server, "feedback", json!({ "id": id, "verdict": verdict }));
        assert_eq!(refused["isError"], true, "{verdict}");
    }

    // maintain reports health, and checks integrity when asked.
    let report = call(&server, "maintain", json!({ "check": true }));
    let health = &structured(&report)["health"];
    assert_eq!(health["nodes"], 3, "{health}");
    assert_eq!(health["contradictions"], 1);
    assert_eq!(health["check"]["ok"], true);
}

#[test]
fn remember_does_not_take_a_room_for_the_entity_of_its_name() {
    use grafeno_memory::{NewRoom, create_palace, create_room};

    let db = Grafeno::in_memory().unwrap();
    let room = db
        .write(|tx| {
            let palace = create_palace(tx, "agent", "Grafeno", None)?.id;
            Ok(create_room(tx, "agent", palace, NewRoom::new("Grafeno"))?.id)
        })
        .unwrap();
    let server = McpServer::new(db, "agent".into());

    let result = call(
        &server,
        "remember",
        json!({ "text": "Grafeno is a graph database", "name": "Grafeno" }),
    );
    let id = structured(&result)["id"].as_str().unwrap().to_owned();
    assert_eq!(structured(&result)["reused"], false);
    assert_ne!(id, room.to_string());

    // The entity is found again by its name, and the room is as it was.
    let again = call(
        &server,
        "remember",
        json!({ "text": "It keeps one writer", "name": "grafeno" }),
    );
    assert_eq!(structured(&again)["id"], id);
    let node = call(&server, "get_node", json!({ "id": room.to_string() }));
    let text = node["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("graph database"), "{text}");

    // And recall returns the memory, not the palace.
    let found = call(&server, "recall", json!({ "query": "Grafeno" }));
    let results = structured(&found)["results"].as_array().unwrap().clone();
    assert!(!results.is_empty());
    assert!(results.iter().all(|hit| hit["id"] != room.to_string()));
}

/// The text of a tool error.
fn failure(result: &Value) -> String {
    assert_eq!(result["isError"], true, "{result}");
    result["content"][0]["text"].as_str().unwrap().to_owned()
}

fn remembered(server: &McpServer, arguments: Value) -> String {
    let result = call(server, "remember", arguments);
    structured(&result)["id"].as_str().unwrap().to_owned()
}

#[test]
fn palace_tools_organize_and_bring_back_memories() {
    let server = server();
    let empty = call(&server, "palace_map", json!({}));
    assert_eq!(structured(&empty)["palaces"], json!([]));
    assert!(structured(&empty)["hint"].is_string());

    // A palace and a room, made once.
    let palace = call(
        &server,
        "palace_create",
        json!({ "name": "Work", "description": "Projects" }),
    );
    let palace_id = structured(&palace)["id"].as_str().unwrap().to_owned();
    assert_eq!(structured(&palace)["created"], true);
    let again = call(&server, "palace_create", json!({ "name": "  work " }));
    assert_eq!(structured(&again)["id"], palace_id.as_str());
    assert_eq!(structured(&again)["created"], false);

    let room = call(
        &server,
        "palace_create_room",
        json!({ "palace": "work", "name": "Storage", "topic": "the storage engine",
                "loci": ["Decisions"] }),
    );
    let room_id = structured(&room)["id"].as_str().unwrap().to_owned();
    assert_eq!(structured(&room)["created"], true);
    assert_eq!(structured(&room)["palace"]["name"], "Work");
    let loci: Vec<&str> = structured(&room)["room"]["loci"]
        .as_array()
        .unwrap()
        .iter()
        .map(|locus| locus["name"].as_str().unwrap())
        .collect();
    assert_eq!(loci, ["entrance", "Decisions"]);
    // By the palace's id too, and the room that exists is returned.
    let again = call(
        &server,
        "palace_create_room",
        json!({ "palace": palace_id, "name": "storage" }),
    );
    assert_eq!(structured(&again)["id"], room_id.as_str());
    assert_eq!(structured(&again)["created"], false);

    // Memories made with the tools there were, and a fact between two.
    let writer = remembered(
        &server,
        json!({ "text": "Only one transaction writes at a time", "labels": ["Note"] }),
    );
    let file = remembered(
        &server,
        json!({ "text": "The whole database is one file", "labels": ["Note"] }),
    );
    let raft = remembered(
        &server,
        json!({ "text": "A replicated log orders the writes", "labels": ["Note"] }),
    );
    structured(&call(
        &server,
        "relate",
        json!({ "from": writer, "label": "leads_to", "to": raft }),
    ));

    // Anchoring: at a locus that exists, at the entrance by default, and
    // at a locus made on the way.
    let anchored = call(
        &server,
        "palace_anchor",
        json!({ "memory": writer, "room": "Storage", "locus": "decisions",
                "note": "why writes are serial" }),
    );
    let anchored = structured(&anchored);
    assert_eq!(anchored["anchored"], true);
    assert_eq!(anchored["locus_created"], false);
    assert_eq!(
        anchored["memory"]["name"],
        "Only one transaction writes at a time"
    );
    assert_eq!(anchored["places"][0]["room_name"], "Storage");
    assert_eq!(anchored["places"][0]["locus_name"], "Decisions");
    assert_eq!(anchored["places"][0]["note"], "why writes are serial");
    let twice = call(
        &server,
        "palace_anchor",
        json!({ "memory": writer, "room": room_id, "locus": "Decisions" }),
    );
    assert_eq!(structured(&twice)["anchored"], false);
    assert_eq!(structured(&twice)["places"].as_array().unwrap().len(), 1);
    let at_entrance = call(
        &server,
        "palace_anchor",
        json!({ "memory": file, "room": "Storage" }),
    );
    assert_eq!(
        structured(&at_entrance)["places"][0]["locus_name"],
        "entrance"
    );
    let new_locus = call(
        &server,
        "palace_anchor",
        json!({ "memory": writer, "room": "Storage", "locus": "Open questions" }),
    );
    assert_eq!(structured(&new_locus)["locus_created"], true);
    assert_eq!(
        structured(&new_locus)["places"].as_array().unwrap().len(),
        2
    );

    // The memory is the one node it was, with its text and its fact.
    let node = call(&server, "get_node", json!({ "id": writer }));
    let text = node["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Only one transaction writes"), "{text}");
    assert!(text.contains("leads_to"), "{text}");
    let notes = call(
        &server,
        "query",
        json!({ "query": "MATCH (n:Note) RETURN count(n)" }),
    );
    assert!(notes.to_string().contains('3'), "{notes}");

    // The map: every palace, one palace, one room with its memories.
    let all = call(&server, "palace_map", json!({}));
    let all = structured(&all);
    assert_eq!(all["palaces"][0]["name"], "Work");
    assert_eq!(all["palaces"][0]["rooms"][0]["memories"], 2);
    assert!(all["palaces"][0]["rooms"][0]["loci"][0]["memories"].is_null());
    let one = call(&server, "palace_map", json!({ "palace": "Work" }));
    assert_eq!(structured(&one)["palace"]["rooms"][0]["name"], "Storage");
    let inside = call(&server, "palace_map", json!({ "room": "storage" }));
    let loci = structured(&inside)["room"]["loci"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(loci.len(), 3);
    assert_eq!(loci[0]["memories"][0]["id"], file.as_str());
    assert_eq!(loci[1]["memories"][0]["id"], writer.as_str());
    assert_eq!(loci[1]["memories"][0]["note"], "why writes are serial");
    assert_eq!(loci[2]["name"], "Open questions");

    // A walk from the room's topic, and one relation beyond it.
    let walk = call(&server, "palace_walk", json!({ "topic": "storage engine" }));
    let walk = structured(&walk);
    assert_eq!(walk["places"][0]["path"], "Work / Storage");
    let reached: Vec<(&str, u64)> = walk["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|memory| {
            (
                memory["id"].as_str().unwrap(),
                memory["hops"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        reached,
        [(file.as_str(), 0), (writer.as_str(), 0), (raft.as_str(), 1)]
    );
    assert_eq!(walk["memories"][2]["via"][0]["label"], "leads_to");
    assert_eq!(walk["memories"][2]["origin"], writer.as_str());
    let from = call(
        &server,
        "palace_walk",
        json!({ "from": room_id, "hops": 0, "palace": "Work" }),
    );
    assert_eq!(structured(&from)["memories"].as_array().unwrap().len(), 2);
    let nothing = call(&server, "palace_walk", json!({ "topic": "volcanoes" }));
    assert_eq!(structured(&nothing)["memories"], json!([]));
    assert!(structured(&nothing)["hint"].is_string());

    // Entering: the context of the room, and no checkpoint yet.
    let entered = call(&server, "palace_enter", json!({ "room": "Storage" }));
    let entered = structured(&entered);
    assert_eq!(entered["palace"]["name"], "Work");
    assert_eq!(entered["room"]["name"], "Storage");
    let context = entered["context"].as_str().unwrap();
    assert!(context.contains("Only one transaction writes"), "{context}");
    assert!(
        context.contains("The whole database is one file"),
        "{context}"
    );
    assert!(context.contains("leads_to"), "{context}");
    assert!(!context.contains("anchored_at"), "{context}");
    assert_eq!(entered["memories"].as_array().unwrap().len(), 2);
    assert_eq!(entered["left_out"], 0);
    assert!(entered["checkpoint"].is_null());
    assert!(entered["hint"].is_string());

    // The checkpoint tool there was saves where the room's work stands.
    structured(&call(
        &server,
        "checkpoint",
        json!({ "task": "Storage design", "summary": "One writer settled",
                "next": ["Measure the file"], "about": room_id }),
    ));
    let entered = call(
        &server,
        "palace_enter",
        json!({ "room": room_id, "budget": 60 }),
    );
    let entered = structured(&entered);
    assert_eq!(entered["checkpoint"]["summary"], "One writer settled");
    assert_eq!(entered["checkpoint"]["next"][0], "Measure the file");
    assert!(entered["hint"].is_null());
    assert!(entered["tokens"].as_u64().unwrap() <= 60);

    // Removing an anchor leaves the memory.
    let removed = call(
        &server,
        "palace_anchor",
        json!({ "memory": file, "room": "Storage", "remove": true }),
    );
    assert_eq!(structured(&removed)["removed"], true);
    assert_eq!(structured(&removed)["places"], json!([]));
    let removed = call(
        &server,
        "palace_anchor",
        json!({ "memory": file, "room": "Storage", "remove": true }),
    );
    assert_eq!(structured(&removed)["removed"], false);
    structured(&call(&server, "get_node", json!({ "id": file })));

    // Recall and context answer as before: memories, never the palace.
    let recalled = call(&server, "recall", json!({ "query": "storage engine file" }));
    let results = structured(&recalled)["results"].as_array().unwrap().clone();
    assert!(!results.is_empty());
    assert!(
        results
            .iter()
            .all(|hit| hit["id"] != room_id.as_str() && hit["id"] != palace_id.as_str())
    );
}

#[test]
fn palace_tools_explain_bad_input() {
    let server = server();
    structured(&call(&server, "palace_create", json!({ "name": "Work" })));
    structured(&call(&server, "palace_create", json!({ "name": "Home" })));
    for palace in ["Work", "Home"] {
        structured(&call(
            &server,
            "palace_create_room",
            json!({ "palace": palace, "name": "Notes" }),
        ));
    }
    let memory = remembered(&server, json!({ "text": "Something to keep" }));

    let nameless = call(&server, "palace_create", json!({ "name": "  " }));
    assert!(failure(&nameless).contains("needs a name"));
    let missing = call(&server, "palace_create", json!({}));
    assert!(failure(&missing).contains("name"));
    let unknown = call(
        &server,
        "palace_create",
        json!({ "name": "X", "colour": "red" }),
    );
    assert!(failure(&unknown).contains("colour"));

    let no_palace = call(
        &server,
        "palace_create_room",
        json!({ "palace": "Elsewhere", "name": "Room" }),
    );
    assert!(failure(&no_palace).contains("no palace named \"Elsewhere\""));

    // A room name two palaces share needs its palace.
    let ambiguous = call(
        &server,
        "palace_anchor",
        json!({ "memory": memory, "room": "Notes" }),
    );
    let message = failure(&ambiguous);
    assert!(message.contains("several palaces"), "{message}");
    assert!(
        message.contains("Home") && message.contains("Work"),
        "{message}"
    );
    let settled = call(
        &server,
        "palace_anchor",
        json!({ "memory": memory, "room": "Notes", "palace": "Home" }),
    );
    assert_eq!(structured(&settled)["places"][0]["palace_name"], "Home");

    // A room is not made by anchoring into it.
    let no_room = call(
        &server,
        "palace_anchor",
        json!({ "memory": memory, "room": "Attic", "palace": "Home" }),
    );
    assert!(failure(&no_room).contains("no room named \"Attic\" in Home"));
    let no_room = call(&server, "palace_enter", json!({ "room": "Attic" }));
    assert!(failure(&no_room).contains("no room named \"Attic\""));

    let bad_id = call(
        &server,
        "palace_anchor",
        json!({ "memory": "not-an-id", "room": "Notes", "palace": "Home" }),
    );
    assert!(failure(&bad_id).contains("invalid id"));
    let gone = call(
        &server,
        "palace_anchor",
        json!({ "memory": "00000000-0000-4000-8000-000000000000", "room": "Notes",
                "palace": "Home" }),
    );
    assert!(failure(&gone).contains("not found"), "{gone}");
    // A room is not a memory to anchor.
    let room = call(
        &server,
        "palace_map",
        json!({ "room": "Notes", "palace": "Work" }),
    );
    let room_id = structured(&room)["room"]["id"].as_str().unwrap().to_owned();
    let not_memory = call(
        &server,
        "palace_anchor",
        json!({ "memory": room_id, "room": "Notes", "palace": "Home" }),
    );
    assert!(failure(&not_memory).contains("not a memory"));

    let aimless = call(&server, "palace_walk", json!({}));
    assert!(failure(&aimless).contains("topic or a node"));
    // A memory given where a room is expected.
    let not_room = call(&server, "palace_enter", json!({ "room": memory }));
    assert!(failure(&not_room).contains("is not a Room"));
}

#[test]
fn palaces_keep_to_their_namespace_and_to_what_an_agent_may_do() {
    let db = Grafeno::in_memory().unwrap();
    let server = McpServer::new(db, "agent".into()).with_access(McpAccess {
        allow: vec!["agent".into(), "team-*".into()],
        read_only: false,
        actor: Some(grafeno::Actor::new("assistant")),
    });
    structured(&call(&server, "palace_create", json!({ "name": "Work" })));
    structured(&call(
        &server,
        "palace_create_room",
        json!({ "palace": "Work", "name": "Storage" }),
    ));
    let memory = remembered(&server, json!({ "text": "One writer at a time" }));
    structured(&call(
        &server,
        "palace_anchor",
        json!({ "memory": memory, "room": "Storage" }),
    ));

    // Another namespace sees none of it, and may have a palace of the
    // same name.
    let other = call(&server, "palace_map", json!({ "namespace": "team-a" }));
    assert_eq!(structured(&other)["palaces"], json!([]));
    let theirs = call(
        &server,
        "palace_create",
        json!({ "name": "Work", "namespace": "team-a" }),
    );
    assert_eq!(structured(&theirs)["created"], true);
    let no_room = call(
        &server,
        "palace_enter",
        json!({ "room": "Storage", "namespace": "team-a" }),
    );
    assert!(failure(&no_room).contains("no room named"));
    structured(&call(
        &server,
        "palace_create_room",
        json!({ "palace": "Work", "name": "Storage", "namespace": "team-a" }),
    ));
    // A memory of one namespace is not anchored in another.
    let across = call(
        &server,
        "palace_anchor",
        json!({ "memory": memory, "room": "Storage", "namespace": "team-a" }),
    );
    assert!(failure(&across).contains("not found"), "{across}");
    let walk = call(
        &server,
        "palace_walk",
        json!({ "topic": "writer", "namespace": "team-a" }),
    );
    assert_eq!(structured(&walk)["memories"], json!([]));
    let mine = call(&server, "palace_map", json!({}));
    assert_eq!(structured(&mine)["palaces"].as_array().unwrap().len(), 1);
    assert_eq!(structured(&mine)["palaces"][0]["rooms"][0]["memories"], 1);

    // A namespace that is not open to the agent is not reached.
    for (tool, arguments) in [
        ("palace_map", json!({ "namespace": "secret" })),
        (
            "palace_create",
            json!({ "name": "Work", "namespace": "secret" }),
        ),
        (
            "palace_enter",
            json!({ "room": "Storage", "namespace": "secret" }),
        ),
        (
            "palace_walk",
            json!({ "topic": "writer", "namespace": "secret" }),
        ),
    ] {
        let fenced = call(&server, tool, arguments);
        assert!(
            failure(&fenced).contains("not open to this agent"),
            "{tool}"
        );
    }

    // The agent's name is on what it anchored.
    let who = call(
        &server,
        "query",
        json!({ "query": "MATCH (m)-[a:anchored_at]->(l:Locus) RETURN a.@recorded_by" }),
    );
    assert!(who.to_string().contains("assistant"), "{who}");
}

#[test]
fn a_read_only_server_reads_palaces_and_writes_none() {
    let path = std::env::temp_dir().join(format!(
        "grafeno-mcp-palace-{}-{}.redb",
        std::process::id(),
        grafeno::NodeId::new()
    ));
    let (room, memory) = {
        let server = McpServer::new(Grafeno::open(&path).unwrap(), "agent".into());
        structured(&call(&server, "palace_create", json!({ "name": "Work" })));
        let room = call(
            &server,
            "palace_create_room",
            json!({ "palace": "Work", "name": "Storage", "topic": "the storage engine" }),
        );
        let room = structured(&room)["id"].as_str().unwrap().to_owned();
        let memory = remembered(&server, json!({ "text": "One writer at a time" }));
        structured(&call(
            &server,
            "palace_anchor",
            json!({ "memory": memory, "room": "Storage" }),
        ));
        structured(&call(
            &server,
            "checkpoint",
            json!({ "task": "Storage", "summary": "Anchored the first note", "about": room }),
        ));
        (room, memory)
    };

    // The agent stopped and another one starts, on the same file, only to
    // read: the palace is there, and it cannot change it.
    let reader =
        McpServer::new(Grafeno::open(&path).unwrap(), "agent".into()).with_access(McpAccess {
            read_only: true,
            ..McpAccess::default()
        });
    let list = request(&reader, 1, "tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .filter(|name| name.starts_with("palace_"))
        .collect();
    assert_eq!(
        names,
        [
            "palace_map",
            "palace_walk",
            "palace_enter",
            "palace_route",
            "palace_review",
            "palace_reflect"
        ]
    );

    let map = call(&reader, "palace_map", json!({ "room": "Storage" }));
    assert_eq!(structured(&map)["room"]["id"], room.as_str());
    assert_eq!(
        structured(&map)["room"]["loci"][0]["memories"][0]["id"],
        memory.as_str()
    );
    let walk = call(&reader, "palace_walk", json!({ "topic": "storage engine" }));
    assert_eq!(structured(&walk)["memories"][0]["id"], memory.as_str());
    let entered = call(&reader, "palace_enter", json!({ "room": "Storage" }));
    let entered = structured(&entered);
    assert!(entered["context"].as_str().unwrap().contains("One writer"));
    assert_eq!(entered["checkpoint"]["summary"], "Anchored the first note");

    for (tool, arguments) in [
        ("palace_create", json!({ "name": "Home" })),
        (
            "palace_create_room",
            json!({ "palace": "Work", "name": "Other" }),
        ),
        (
            "palace_anchor",
            json!({ "memory": memory, "room": "Storage", "remove": true }),
        ),
    ] {
        let refused = call(&reader, tool, arguments);
        assert!(failure(&refused).contains("read-only"), "{tool}");
    }
    let map = call(&reader, "palace_map", json!({}));
    assert_eq!(structured(&map)["palaces"].as_array().unwrap().len(), 1);
    assert_eq!(structured(&map)["palaces"][0]["rooms"][0]["memories"], 1);
    drop(reader);
    let _ = std::fs::remove_file(&path);
}

/// Phase 2's acceptance: a technical description goes in, an agent's
/// reading of it goes in with one call, and the entities, the relations,
/// their evidence and certainty and their places in a palace come back.
#[test]
fn a_technical_description_becomes_entities_relations_and_places() {
    let server = server();
    let doc = "# Billing\n\nThe billing service runs on Ledger.\n\nAna Torres maintains the billing service. It publishes invoices to the events queue.";
    let ingested = call(
        &server,
        "ingest",
        json!({ "content": doc, "source": "docs/billing.md", "extract": false }),
    );
    let ingested = structured(&ingested).clone();
    let chunk = ingested["chunk_ids"][0].as_str().unwrap().to_owned();

    // The agent read the chunk with its own model and hands over what it
    // read: four entities and three relations.
    let read = call(
        &server,
        "add_mentions",
        json!({
            "id": chunk,
            "mentions": [
                { "surface": "billing service", "label": "Service" },
                { "surface": "Ledger", "label": "Database" },
                { "surface": "Ana Torres", "label": "Person" },
                { "surface": "events queue", "label": "Queue" }
            ],
            "relations": [
                { "from": 0, "label": "runs_on", "to": 1, "origin": "stated", "certainty": 0.95 },
                { "from": "Ana Torres", "label": "maintains", "to": "billing service" },
                { "from": 0, "label": "publishes_to", "to": "events queue", "certainty": 0.6,
                  "valid_from": "2026-01-01" },
                { "from": 0, "label": "owned_by", "to": "Bob" }
            ]
        }),
    );
    let read = structured(&read).clone();
    assert_eq!(read["created"], 4, "{read}");
    assert_eq!(read["relations"].as_array().unwrap().len(), 3);
    assert_eq!(read["skipped"][0]["relation"], 3);
    let entity = |index: usize| read["entities"][index]["id"].as_str().unwrap().to_owned();
    let (service, ana) = (entity(0), entity(2));
    assert_eq!(read["entities"][1]["name"], "Ledger");

    // Each fact says how it is known and where it came from.
    let node = call(&server, "get_node", json!({ "id": service }));
    let node = structured(&node).clone();
    assert_eq!(node["known"], "not stated");
    let fact = |label: &str| {
        node["facts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|fact| fact["label"] == label)
            .unwrap_or_else(|| panic!("no {label} in {node}"))
            .clone()
    };
    let runs_on = fact("runs_on");
    assert_eq!(
        runs_on["known"],
        json!({ "origin": "stated", "certainty": 0.95 })
    );
    assert_eq!(runs_on["sources"], json!([chunk]));
    assert_eq!(runs_on["other"]["name"], "Ledger");
    let maintains = fact("maintains");
    assert_eq!(maintains["direction"], "in");
    assert_eq!(maintains["known"], json!({ "origin": "inferred" }));
    let publishes = fact("publishes_to");
    assert_eq!(publishes["valid_from"], "2026-01-01");
    assert_eq!(
        publishes["known"],
        json!({ "origin": "inferred", "certainty": 0.6 })
    );
    // A fact nobody qualified has no `known` at all.
    let mentions = fact("mentions");
    assert!(mentions["known"].is_null(), "{mentions}");

    // The agent checks one inferred fact against the document and says so.
    let maintains_id = maintains["id"].as_str().unwrap();
    let whole = ingested["id"].as_str().unwrap();
    let verified = call(
        &server,
        "feedback",
        json!({ "id": maintains_id, "verdict": "verified", "source": whole }),
    );
    let verified = structured(&verified);
    assert_eq!(verified["known"]["origin"], "verified");
    assert_eq!(verified["known"]["certainty"], 1.0);
    assert_eq!(verified["known"]["verified_by"], whole);
    let node = call(&server, "get_node", json!({ "id": ana }));
    let after = structured(&node)["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|fact| fact["label"] == "maintains")
        .unwrap()
        .clone();
    assert_eq!(after["known"]["origin"], "verified");

    // A memory of a kind, with how it is known.
    let decision = call(
        &server,
        "remember",
        json!({ "text": "Invoices go through the events queue so billing can be replayed",
                "labels": ["Decision"], "origin": "stated", "certainty": 0.9 }),
    );
    let decision = structured(&decision)["id"].as_str().unwrap().to_owned();
    let found = call(
        &server,
        "recall",
        json!({ "query": "events queue", "labels": ["Decision"] }),
    );
    let results = structured(&found)["results"].as_array().unwrap().clone();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"], decision.as_str());
    assert_eq!(
        results[0]["known"],
        json!({ "origin": "stated", "certainty": 0.9 })
    );

    // The context shows it too.
    let context = call(
        &server,
        "context",
        json!({ "query": "billing service ledger" }),
    );
    let text = context["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("runs_on Ledger [stated, certainty 0.95]"),
        "{text}"
    );

    // The entities and the memory have a place, and a walk finds them
    // with the relations between them.
    structured(&call(&server, "palace_create", json!({ "name": "Work" })));
    structured(&call(
        &server,
        "palace_create_room",
        json!({ "palace": "Work", "name": "Billing", "topic": "billing and invoices" }),
    ));
    for memory in [&service, &decision] {
        structured(&call(
            &server,
            "palace_anchor",
            json!({ "memory": memory, "room": "Billing" }),
        ));
    }
    let walk = call(
        &server,
        "palace_walk",
        json!({ "topic": "billing and invoices", "hops": 1 }),
    );
    let walk = structured(&walk).clone();
    let names: Vec<&str> = walk["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|memory| memory["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"billing service"), "{names:?}");
    assert!(
        names.contains(&"Ledger") && names.contains(&"Ana Torres"),
        "{names:?}"
    );

    // Bad input is refused with a reason, and nothing is written.
    for (arguments, says) in [
        (json!({ "text": "x", "certainty": 1.5 }), "between 0 and 1"),
        (json!({ "text": "x", "origin": "rumour" }), "unknown origin"),
    ] {
        let refused = call(&server, "remember", arguments);
        assert_eq!(refused["isError"], true, "{refused}");
        assert!(refused.to_string().contains(says), "{refused}");
    }
    let refused = call(
        &server,
        "feedback",
        json!({ "id": decision, "verdict": "useful", "source": whole }),
    );
    assert_eq!(refused["isError"], true, "{refused}");
}

/// Found by an outside review: the server read a line of input whole,
/// however long. A line past the limit is skipped, without being held,
/// and the next one is read as usual.
#[test]
fn a_message_past_the_limit_is_skipped() {
    use grafeno_server::mcp::{Message, read_message};
    let mut input = std::io::Cursor::new(
        format!("{{\"a\":1}}\r\n{}\n{{\"b\":2}}\n\nlast", "x".repeat(100)).into_bytes(),
    );
    let reader = &mut std::io::BufReader::with_capacity(16, &mut input);
    assert_eq!(
        read_message(reader, 50).unwrap(),
        Some(Message::Line("{\"a\":1}".into()))
    );
    assert_eq!(
        read_message(reader, 50).unwrap(),
        Some(Message::TooLong(101))
    );
    assert_eq!(
        read_message(reader, 50).unwrap(),
        Some(Message::Line("{\"b\":2}".into()))
    );
    assert_eq!(
        read_message(reader, 50).unwrap(),
        Some(Message::Line(String::new()))
    );
    assert_eq!(
        read_message(reader, 50).unwrap(),
        Some(Message::Line("last".into()))
    );
    assert_eq!(read_message(reader, 50).unwrap(), None);
    // A line of exactly the limit is read; one byte more is not.
    let mut exact =
        std::io::Cursor::new(format!("{}\n{}", "y".repeat(50), "z".repeat(51)).into_bytes());
    assert_eq!(
        read_message(&mut exact, 50).unwrap(),
        Some(Message::Line("y".repeat(50)))
    );
    assert_eq!(
        read_message(&mut exact, 50).unwrap(),
        Some(Message::TooLong(51))
    );
}

/// Phase 3's acceptance over MCP: a question that needs three memories
/// joined is answered by a walk with a context, with the way that joined
/// them; doors lead from a room to another; repeats are left out.
#[test]
fn walks_join_memories_follow_doors_and_leave_out_repeats() {
    let server = server();
    structured(&call(&server, "palace_create", json!({ "name": "Work" })));
    let room = |name: &str, topic: &str| {
        let made = call(
            &server,
            "palace_create_room",
            json!({ "palace": "Work", "name": name, "topic": topic }),
        );
        structured(&made)["id"].as_str().unwrap().to_owned()
    };
    let ops = room("Operations", "incidents");
    let billing = room("Billing", "invoices");
    let outage = remembered(
        &server,
        json!({ "text": "The night outage stopped the invoices", "labels": ["Experience"] }),
    );
    let service = remembered(
        &server,
        json!({ "name": "Billing service", "text": "Sends the invoices", "labels": ["Service"] }),
    );
    let ana = remembered(
        &server,
        json!({ "name": "Ana", "text": "Keeps the invoices running", "labels": ["Person"] }),
    );
    let numbering = remembered(&server, json!({ "text": "Invoices are numbered by year" }));
    for (from, label, to) in [(&outage, "hit", &service), (&ana, "maintains", &service)] {
        structured(&call(
            &server,
            "relate",
            json!({ "from": from, "label": label, "to": to }),
        ));
    }
    structured(&call(
        &server,
        "palace_anchor",
        json!({ "memory": outage, "room": "Operations" }),
    ));
    structured(&call(
        &server,
        "palace_anchor",
        json!({ "memory": numbering, "room": "Billing" }),
    ));
    // A door, with the tool that records any fact.
    structured(&call(
        &server,
        "relate",
        json!({ "from": ops, "label": "leads_to", "to": billing }),
    ));
    let map = call(&server, "palace_map", json!({ "room": "Operations" }));
    assert_eq!(structured(&map)["room"]["doors"][0]["name"], "Billing");

    // "Who keeps running what the outage hit?"
    let walk = call(
        &server,
        "palace_walk",
        json!({ "topic": "incidents", "order": "relevance", "budget": 600 }),
    );
    let walk = structured(&walk).clone();
    let context = walk["context"].as_str().unwrap();
    assert!(context.contains("hit Billing service"), "{context}");
    assert!(
        context.contains("Ana maintains Billing service"),
        "{context}"
    );
    assert!(walk["tokens"].as_u64().unwrap() <= 600);
    let found = |id: &str| {
        walk["memories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|memory| memory["id"] == id)
            .unwrap_or_else(|| panic!("{id} not in {walk}"))
            .clone()
    };
    let ana_found = found(&ana);
    let way: Vec<&str> = ana_found["via"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hop| hop["label"].as_str().unwrap())
        .collect();
    assert_eq!(way, ["hit", "maintains"]);
    // Ordered by relevance, each says how well it matched.
    assert!(ana_found["relevance"].is_number());
    // Through the door: Billing's note, one step away.
    let through = found(&numbering);
    assert_eq!(through["hops"], 1);
    assert_eq!(through["via"][0]["label"], "leads_to");
    assert!(
        walk["places"]
            .as_array()
            .unwrap()
            .iter()
            .any(|place| place["path"] == "Work / Billing" && place["through"] == ops.as_str())
    );

    // A memory stored again in other words is packed once.
    remembered(
        &server,
        json!({ "text": "Invoices are numbered, by year." }),
    );
    let text = call(
        &server,
        "context",
        json!({ "query": "invoices numbered year" }),
    );
    let text = text["content"][0]["text"].as_str().unwrap();
    assert_eq!(text.matches("numbered").count(), 1, "{text}");
    assert!(text.contains("repeated these left out"), "{text}");
    let bad = call(
        &server,
        "palace_walk",
        json!({ "topic": "invoices", "order": "random" }),
    );
    assert!(failure(&bad).contains("unknown order"));
}

/// Phase 4 over MCP: a review proposes and changes nothing; forgetting a
/// room takes its structure and no memory, said first with dry_run;
/// maintain lists newer memories that may overtake older ones; a
/// consolidation is undone.
#[test]
fn palaces_are_reviewed_rooms_removed_and_consolidations_undone() {
    let server = server();
    structured(&call(&server, "palace_create", json!({ "name": "Work" })));
    let mut rooms = Vec::new();
    for name in ["Billing", "Copy", "Empty"] {
        let made = call(
            &server,
            "palace_create_room",
            json!({ "palace": "Work", "name": name }),
        );
        rooms.push(structured(&made)["id"].as_str().unwrap().to_owned());
    }
    let mut notes = Vec::new();
    for i in 0..4 {
        let id = remembered(
            &server,
            json!({ "text": format!("Billing retry note {i}"), "labels": ["Note"] }),
        );
        for room in ["Billing", "Copy"] {
            structured(&call(
                &server,
                "palace_anchor",
                json!({ "memory": id, "room": room }),
            ));
        }
        notes.push(id);
    }
    let count = |label: &str| {
        let result = call(
            &server,
            "query",
            json!({ "query": format!("MATCH (n:{label}) RETURN count(n)") }),
        );
        result.to_string()
    };
    let before = (count("Note"), count("Room"), count("Locus"));

    let review = call(
        &server,
        "palace_review",
        json!({ "palace": "Work", "crowded": 3 }),
    );
    let review = structured(&review).clone();
    let kinds: Vec<&str> = review["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["kind"].as_str().unwrap())
        .collect();
    for kind in ["empty_room", "merge_rooms", "split_room"] {
        assert!(kinds.contains(&kind), "{kind} not in {review}");
    }
    assert!(review["proposals"][0]["how"].is_string());
    assert!(
        review["hint"]
            .as_str()
            .unwrap()
            .contains("summarize_communities")
    );
    assert_eq!((count("Note"), count("Room"), count("Locus")), before);

    // Forgetting a room: said, then done; its notes stay.
    let said = call(
        &server,
        "forget",
        json!({ "id": rooms[1], "dry_run": true }),
    );
    let said = structured(&said).clone();
    assert_eq!(said["anchors"], 4);
    assert_eq!(
        said["left_in_no_place"],
        json!([]),
        "they are in Billing too"
    );
    assert_eq!((count("Note"), count("Room"), count("Locus")), before);
    structured(&call(&server, "forget", json!({ "id": rooms[1] })));
    assert_eq!(count("Note"), before.0);
    let map = call(&server, "palace_map", json!({ "palace": "Work" }));
    let names: Vec<&str> = structured(&map)["palace"]["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|room| room["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Billing", "Empty"]);
    let refused = call(
        &server,
        "forget",
        json!({ "id": rooms[0], "mode": "retract" }),
    );
    assert!(failure(&refused).contains("only be forgotten with mode"));

    // A newer memory that may overtake an older one about the same person.
    let ana = remembered(
        &server,
        json!({ "name": "Ana", "text": "A person", "labels": ["Person"] }),
    );
    let old = remembered(
        &server,
        json!({ "text": "Ana works at Acme as a backend engineer in Lima",
                "links": [{ "label": "about", "to": ana }] }),
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    let new = remembered(
        &server,
        json!({ "text": "Ana works at Globex as a backend engineer in Lima now",
                "links": [{ "label": "about", "to": ana }] }),
    );
    let health = call(&server, "maintain", json!({}));
    let superseded = structured(&health)["superseded"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        superseded
            .iter()
            .any(|pair| pair["older"]["id"] == old.as_str() && pair["newer"]["id"] == new.as_str()),
        "{superseded:?}"
    );

    // A consolidation, and undoing it.
    let made = call(
        &server,
        "consolidate",
        json!({ "summary": "Billing retries, in short", "sources": notes }),
    );
    let summary = structured(&made)["id"].as_str().unwrap().to_owned();
    let found = call(&server, "recall", json!({ "query": "billing retry note" }));
    assert!(
        structured(&found)["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| !notes.contains(&hit["id"].as_str().unwrap().to_owned()))
    );
    let undone = call(&server, "consolidate", json!({ "undo": summary }));
    assert_eq!(structured(&undone)["restored"].as_array().unwrap().len(), 4);
    let found = call(&server, "recall", json!({ "query": "billing retry note" }));
    assert!(
        structured(&found)["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| notes.contains(&hit["id"].as_str().unwrap().to_owned()))
    );
    let wrong = call(&server, "consolidate", json!({ "undo": notes[0] }));
    assert!(failure(&wrong).contains("not a summary"));
}

/// Phase 6 over MCP: a reflection on a topic with partial information
/// separates the known, the inferred, the clashing, the out of date and the
/// missing; a topic with nothing says so.
#[test]
fn reflection_separates_what_is_known_from_what_is_not() {
    let server = server();
    let service = remembered(
        &server,
        json!({ "name": "Billing service", "text": "Billing sends the invoices", "labels": ["Service"] }),
    );
    let doc = remembered(
        &server,
        json!({ "text": "Billing runbook: runs on Ledger" }),
    );
    let ledger = remembered(
        &server,
        json!({ "name": "Ledger", "text": "A database", "labels": ["Database"] }),
    );
    let ana = remembered(
        &server,
        json!({ "name": "Ana", "text": "A person", "labels": ["Person"] }),
    );
    let fact = structured(&call(
        &server,
        "relate",
        json!({ "from": service, "label": "runs_on", "to": ledger }),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    structured(&call(
        &server,
        "feedback",
        json!({ "id": fact, "verdict": "verified", "source": doc }),
    ));
    let guess = structured(&call(
        &server,
        "relate",
        json!({ "from": ana, "label": "maintains", "to": service, "origin": "inferred" }),
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let question = remembered(
        &server,
        json!({ "text": "Who approves billing refunds?", "labels": ["Question"] }),
    );

    let reflection = call(&server, "palace_reflect", json!({ "topic": "billing" }));
    let reflection = structured(&reflection).clone();
    assert_eq!(reflection["nothing"], false);
    assert_eq!(reflection["known"]["verified"][0]["id"], fact.as_str());
    assert!(
        reflection["uncertain"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == guess.as_str() && item["why"] == "inferred, with no source"),
        "{reflection}"
    );
    assert!(
        reflection["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == question.as_str() && item["kind"] == "question")
    );
    assert!(reflection["next"].as_array().unwrap().len() >= 2);
    assert!(
        reflection["hint"]
            .as_str()
            .unwrap()
            .contains("not as truth")
    );

    // Answered, the question is no longer a gap.
    let answer = remembered(
        &server,
        json!({ "text": "The billing lead approves refunds" }),
    );
    structured(&call(
        &server,
        "relate",
        json!({ "from": answer, "label": "answers", "to": question }),
    ));
    let again = call(&server, "palace_reflect", json!({ "topic": "billing" }));
    assert!(
        !structured(&again)["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == question.as_str())
    );

    let nothing = call(&server, "palace_reflect", json!({ "topic": "volcanoes" }));
    let nothing = structured(&nothing).clone();
    assert_eq!(nothing["nothing"], true);
    assert_eq!(nothing["known"]["memories"], 0);
    assert!(
        nothing["hint"]
            .as_str()
            .unwrap()
            .contains("rather than guess")
    );
    let both = call(
        &server,
        "palace_reflect",
        json!({ "topic": "billing", "node": service }),
    );
    assert!(failure(&both).contains("one of"));
}

/// Phase 8's acceptance: related knowledge recorded, found again by its
/// entities, with relations, identity and evidence kept and nothing
/// duplicated without reason.
#[test]
fn knowledge_keeps_identity_relations_and_evidence() {
    let server = server();
    let episode = remembered(
        &server,
        json!({ "text": "Call with Ana: she maintains the billing service, which depends on Ledger",
                "labels": ["Episode"] }),
    );
    let reading = json!({
        "id": episode,
        "mentions": [
            { "surface": "Ana", "label": "Person" },
            { "surface": "billing service", "label": "Service" },
            { "surface": "Ledger", "label": "Database" }
        ],
        "relations": [
            { "from": 0, "label": "maintains", "to": 1, "origin": "stated" },
            { "from": 1, "label": "depends_on", "to": 2, "origin": "stated" }
        ]
    });
    let first = call(&server, "add_mentions", reading.clone());
    let first = structured(&first).clone();
    assert_eq!(first["created"], 3, "{first}");
    // The same reading again: the entities are found, the facts not doubled.
    let again = call(&server, "add_mentions", reading);
    let again = structured(&again).clone();
    assert_eq!(
        (again["created"].clone(), again["linked"].clone()),
        (json!(0), json!(3))
    );
    assert_eq!(again["relations"], first["relations"]);
    let ana = first["entities"][0]["id"].as_str().unwrap().to_owned();
    let service = first["entities"][1]["id"].as_str().unwrap().to_owned();

    // Found again by its entity, with how each fact is known and where it
    // came from.
    let node = call(&server, "get_node", json!({ "id": service }));
    let node = structured(&node).clone();
    let fact = |label: &str| {
        node["facts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|fact| fact["label"] == label)
            .unwrap_or_else(|| panic!("no {label}: {node}"))
            .clone()
    };
    let maintains = fact("maintains");
    assert_eq!(maintains["other"]["id"], ana.as_str());
    assert_eq!(maintains["sources"], json!([episode]));
    assert_eq!(maintains["method"], "agent");
    assert_eq!(maintains["known"]["origin"], "stated");
    assert_eq!(node["provenance"]["sources"], json!([episode]));

    // Evidence, and a cause only with it.
    let log = remembered(
        &server,
        json!({ "text": "Ledger was down from 02:00 to 03:10", "labels": ["Evidence"],
                "origin": "observed", "method": "imported" }),
    );
    let outage = remembered(
        &server,
        json!({ "text": "Invoices failed overnight", "labels": ["Experience"] }),
    );
    let refused = call(
        &server,
        "relate",
        json!({ "from": outage, "label": "caused_by", "to": first["entities"][2]["id"] }),
    );
    assert!(failure(&refused).contains("needs evidence"));
    structured(&call(
        &server,
        "relate",
        json!({ "from": outage, "label": "caused_by", "to": first["entities"][2]["id"],
                "sources": [log] }),
    ));
    structured(&call(
        &server,
        "relate",
        json!({ "from": outage, "label": "supported_by", "to": log }),
    ));
    let twice = call(
        &server,
        "relate",
        json!({ "from": outage, "label": "supported_by", "to": log }),
    );
    assert_eq!(structured(&twice)["duplicate"], true);
    let node = call(&server, "get_node", json!({ "id": outage }));
    let provenance = structured(&node)["provenance"].clone();
    assert_eq!(provenance["supported_by"][0]["id"], log.as_str());
    let evidence = call(&server, "get_node", json!({ "id": log }));
    assert_eq!(structured(&evidence)["provenance"]["method"], "imported");

    // A merge keeps what the merged node was and how it was known.
    let other = remembered(
        &server,
        json!({ "name": "A. Torres", "text": "Ana, again", "labels": ["Person"],
                "origin": "verified", "certainty": 1.0 }),
    );
    structured(&call(
        &server,
        "merge",
        json!({ "keep": ana, "merge": other, "force": true }),
    ));
    let merged = call(&server, "get_node", json!({ "id": ana }));
    let merged = structured(&merged).clone();
    let from = &merged["provenance"]["merged_from"][0];
    assert_eq!(from["id"], other.as_str());
    assert_eq!(from["origin"], "verified");
    assert_eq!(from["name"], "A. Torres");
    // Still one Ana, found by either name.
    let found = call(
        &server,
        "find_matches",
        json!({ "label": "Person", "props": { "name": "A. Torres" } }),
    );
    let _ = found;

    // Kinds are counted by reflection.
    remembered(
        &server,
        json!({ "text": "Billing could move to monthly runs", "labels": ["Hypothesis"] }),
    );
    let reflection = call(&server, "palace_reflect", json!({ "topic": "billing" }));
    let kinds = structured(&reflection)["known"]["memories_by_kind"].clone();
    assert_eq!(kinds["Hypothesis"], 1, "{kinds}");
    assert_eq!(kinds["Episode"], 1, "{kinds}");
}

#[test]
fn a_decision_changed_three_times_keeps_its_reasons() {
    let server = server();
    let decision =
        |text: &str| remembered(&server, json!({ "text": text, "labels": ["Decision"] }));
    let first = decision("Store invoices in one file per month");
    let second = decision("Store invoices in the ledger database");
    let third = decision("Store invoices in the ledger database, one table per year");
    let measure = remembered(
        &server,
        json!({ "text": "Queries over one table took 4 s at 30 million rows", "labels": ["Evidence"] }),
    );
    let replace = |newer: &str, older: &str, reason: &str, sources: Value| {
        let result = call(
            &server,
            "relate",
            json!({ "from": newer, "label": "supersedes", "to": older,
                    "reason": reason, "sources": sources }),
        );
        structured(&result)["id"].as_str().unwrap().to_owned()
    };
    replace(
        &second,
        &first,
        "monthly files could not be searched",
        json!([]),
    );
    replace(&third, &second, "one table grew too slow", json!([measure]));

    // From the first version: the current one, and every change's reason.
    let line = call(&server, "timeline", json!({ "id": first }));
    let history = structured(&line)["history"].clone();
    assert_eq!(history["current"], third.as_str(), "{history}");
    let versions = history["versions"].as_array().unwrap();
    let ids: Vec<&str> = versions.iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [first.as_str(), second.as_str(), third.as_str()]);
    assert!(versions[0].get("reason").is_none());
    assert_eq!(versions[1]["reason"], "monthly files could not be searched");
    assert_eq!(versions[2]["reason"], "one table grew too slow");
    assert_eq!(versions[2]["evidence"], json!([measure]));

    // A memory nothing replaced has no history.
    let alone = call(&server, "timeline", json!({ "id": measure }));
    assert!(structured(&alone).get("history").is_none());

    // Two states of a service: the store changed.
    let service = remembered(
        &server,
        json!({ "text": "Billing service", "name": "billing" }),
    );
    let files = remembered(&server, json!({ "text": "Monthly files", "name": "files" }));
    let ledger = remembered(
        &server,
        json!({ "text": "Ledger database", "name": "ledger" }),
    );
    for (to, from, until) in [
        (&files, "2025-01-01", Some("2026-03-01")),
        (&ledger, "2026-03-01", None),
    ] {
        let mut arguments = json!({ "from": service, "label": "stores_in", "to": to,
                                    "valid_from": from });
        if let Some(until) = until {
            arguments["valid_to"] = json!(until);
        }
        structured(&call(&server, "relate", arguments));
    }
    let compared = call(
        &server,
        "timeline",
        json!({ "id": service, "compare": "2025-06-01", "at": "2026-06-01" }),
    );
    let comparison = structured(&compared)["comparison"].clone();
    let change = &comparison["changed"][0];
    assert_eq!(change["before"][0]["to"], files.as_str(), "{comparison}");
    assert_eq!(change["after"][0]["to"], ledger.as_str());
    assert_eq!(comparison["ended"], json!([]));

    // `at` alone is refused: it says what to compare with.
    let refused = call(
        &server,
        "timeline",
        json!({ "id": service, "at": "2026-06-01" }),
    );
    assert!(failure(&refused).contains("compare"));
}

#[test]
fn two_palaces_share_a_memory_and_both_see_its_change() {
    let path = std::env::temp_dir().join(format!(
        "grafeno-mcp-views-{}-{}.redb",
        std::process::id(),
        grafeno::NodeId::new()
    ));
    let (work_room, home_room, memory) = {
        let server = McpServer::new(Grafeno::open(&path).unwrap(), "agent".into());
        let room = |palace: &str, name: &str| {
            structured(&call(&server, "palace_create", json!({ "name": palace })));
            let room = call(
                &server,
                "palace_create_room",
                json!({ "palace": palace, "name": name }),
            );
            structured(&room)["id"].as_str().unwrap().to_owned()
        };
        let work_room = room("Work", "Billing");
        let home_room = room("Home", "Bills");
        let memory = remembered(
            &server,
            json!({ "text": "Invoices go out on the first day of the month", "labels": ["Decision"] }),
        );
        for room in [&work_room, &home_room] {
            structured(&call(
                &server,
                "palace_anchor",
                json!({ "memory": memory, "room": room }),
            ));
        }
        (work_room, home_room, memory)
    };
    let enter = |server: &McpServer, room: &str| -> String {
        let entered = call(server, "palace_enter", json!({ "room": room }));
        structured(&entered)["context"].as_str().unwrap().to_owned()
    };

    // An agent that may only read cannot change it: both views stay.
    let reader =
        McpServer::new(Grafeno::open(&path).unwrap(), "agent".into()).with_access(McpAccess {
            read_only: true,
            ..McpAccess::default()
        });
    let refused = call(
        &reader,
        "update",
        json!({ "id": memory, "text": "Invoices go out on the fifth" }),
    );
    failure(&refused);
    for room in [&work_room, &home_room] {
        assert!(enter(&reader, room).contains("first day of the month"));
    }
    drop(reader);

    // An authorized change, made once, shows in both palaces: the memory
    // is the same node, never a copy.
    let server = McpServer::new(Grafeno::open(&path).unwrap(), "agent".into());
    structured(&call(
        &server,
        "update",
        json!({ "id": memory, "text": "Invoices go out on the fifth day of the month" }),
    ));
    for room in [&work_room, &home_room] {
        let context = enter(&server, room);
        assert!(context.contains("fifth day"), "{context}");
        assert!(!context.contains("first day"), "{context}");
    }

    // Its context, rebuilt in one call, names both places.
    let node = call(
        &server,
        "get_node",
        json!({ "id": memory, "reconstruct": true }),
    );
    let rebuilt = structured(&node)["reconstruction"].clone();
    let palaces: Vec<&str> = rebuilt["places"]
        .as_array()
        .unwrap()
        .iter()
        .map(|place| place["palace_name"].as_str().unwrap())
        .collect();
    assert_eq!(palaces.len(), 2, "{rebuilt}");
    assert!(palaces.contains(&"Work") && palaces.contains(&"Home"));
    assert!(rebuilt["text"].as_str().unwrap().contains("fifth day"));
    assert!(rebuilt["reflection"]["known"].is_object());
    assert!(rebuilt["timeline"].is_array());
    drop(server);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_route_follows_relations_in_order() {
    let server = server();
    let node = |text: &str| remembered(&server, json!({ "text": text }));
    let incident = node("The billing outage");
    let service = node("The invoice service");
    let queue = node("The events queue");
    let store = node("The ledger store");
    let ana = node("Ana");
    let relate = |from: &str, label: &str, to: &str| {
        structured(&call(
            &server,
            "relate",
            json!({ "from": from, "label": label, "to": to }),
        ));
    };
    relate(&incident, "hit", &service);
    relate(&service, "depends_on", &queue);
    relate(&queue, "depends_on", &store);
    relate(&ana, "maintains", &store);

    // The incident, what it hit, all that depends on, and who maintains
    // the end of the chain.
    let found = call(
        &server,
        "palace_route",
        json!({ "from": incident, "steps": [
            { "label": "hit" },
            { "label": "depends_on", "repeat": true },
            { "label": "maintains", "backward": true }
        ]}),
    );
    let found = structured(&found).clone();
    let routes = found["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 1, "{found}");
    assert_eq!(
        routes[0]["nodes"],
        json!([incident, service, queue, store, ana])
    );
    let labels: Vec<&str> = routes[0]["hops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hop| hop["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["hit", "depends_on", "depends_on", "maintains"]);

    // A goal: only routes that end there.
    let to_queue = call(
        &server,
        "palace_route",
        json!({ "from": service, "steps": [{ "label": "depends_on", "repeat": true }],
                "to": queue }),
    );
    assert_eq!(
        structured(&to_queue)["routes"][0]["nodes"],
        json!([service, queue])
    );
    // A wrong direction finds nothing, and says what to check.
    let none = call(
        &server,
        "palace_route",
        json!({ "from": incident, "steps": [{ "label": "maintains" }] }),
    );
    assert!(structured(&none)["hint"].is_string());
    let empty = call(
        &server,
        "palace_route",
        json!({ "from": incident, "steps": [] }),
    );
    assert!(failure(&empty).contains("step"));
}
