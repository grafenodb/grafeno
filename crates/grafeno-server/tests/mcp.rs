use grafeno::Grafeno;
use grafeno_server::mcp::McpServer;
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
            "consolidate"
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
    use grafeno_server::mcp::McpAccess;
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
