//! MCP (Model Context Protocol) server over stdio: newline-delimited
//! JSON-RPC 2.0 messages on stdin and stdout. Logs go to stderr.
//!
//! The tools are shaped for language models: few of them, text in and out,
//! names instead of raw records, and dates instead of microseconds.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use grafeno::{
    Direction, EdgeFilter, Error, Grafeno, NewEdge, NewNode, Node, NodeId, Props, ReadTx, Value,
};
use grafeno_memory::{
    ConsolidationOptions, ContextOptions, DecayOptions, DuplicateOptions, Expansion, Ranking,
    RecallQuery, Signal, Summary, consolidation_groups, context_for, find_duplicates, recall,
    weak_memories, write_summary,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::api::{TimeInput, parse_id};

/// Supported protocol versions, newest first.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Grafeno is your long-term memory: a graph of nodes (memories, \
people, projects, documents) and facts (labeled edges between nodes, with the dates they \
hold). Before answering anything that may depend on earlier conversations or stored \
knowledge, call `context` (or `recall` to get ids). Store new facts, decisions and \
preferences with `remember`, and connect nodes with `relate`. When something stops being \
true, `forget` it with mode \"invalidate\" so its history is kept; use \"retract\" only for \
mistakes. Give lasting facts a higher `importance`. From time to time, call `maintain` to \
merge duplicates and `consolidate` old memories into summaries. At the start of a session on \
ongoing work, call `resume`; before stopping, save where it stands with `checkpoint`. Store \
long texts (documents, conversations, code) with `ingest`, and say what helped or misled with \
`feedback`. To keep the memories of a project or subject together, give it a room in a \
palace (`palace_create`, `palace_create_room`) and anchor them there with `palace_anchor`; \
`palace_enter` brings a room's context back at the start of a session, and `palace_walk` goes \
from a topic to the memories around it. Ids are the UUIDs these tools return.";

const SNIPPET_CHARS: usize = 400;

pub struct McpServer {
    db: Grafeno,
    namespace: String,
    access: McpAccess,
}

/// What an agent connected over MCP may do. The process opens the database
/// file itself, so this is a fence for agents, not a defense against the
/// user who starts it.
#[derive(Clone, Debug, Default)]
pub struct McpAccess {
    /// Namespaces it may use (`vault`, `team-*`, `*`); all when empty.
    pub allow: Vec<String>,
    /// Only the tools that read.
    pub read_only: bool,
    /// Who its writes are recorded as, and whether it may change only
    /// what it recorded.
    pub actor: Option<grafeno::Actor>,
}

impl McpAccess {
    fn allows(&self, ns: &str) -> bool {
        self.allow.is_empty()
            || self.allow.iter().any(|pattern| {
                grafeno::Grant {
                    namespace: pattern.clone(),
                    role: grafeno::Role::Read,
                }
                .covers(ns)
            })
    }
}

/// Tools that change the database.
const WRITING_TOOLS: &[&str] = &[
    "extract",
    "add_mentions",
    "resolve_review",
    "remember",
    "relate",
    "update",
    "forget",
    "merge",
    "undo_merge",
    "consolidate",
    "summarize_communities",
    "set_summary",
    "checkpoint",
    "ingest",
    "feedback",
    "palace_create",
    "palace_create_room",
    "palace_anchor",
];

/// Serves MCP on stdin and stdout until stdin closes.
pub fn run(db: Grafeno, namespace: String, access: McpAccess) -> io::Result<()> {
    let server = McpServer::new(db, namespace).with_access(access);
    eprintln!(
        "grafeno MCP server ready (namespace {:?}{}{})",
        server.namespace,
        if server.access.read_only {
            ", read-only"
        } else {
            ""
        },
        server
            .access
            .actor
            .as_ref()
            .map(|actor| format!(", as {}", actor.name))
            .unwrap_or_default()
    );
    let mut stdout = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(&line) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

type ToolResult = Result<ToolOutput, String>;

enum ToolOutput {
    /// Shown to the model as pretty JSON, and returned as structured content.
    Json(Value),
    Text(String),
}

impl McpServer {
    pub fn new(db: Grafeno, namespace: String) -> Self {
        Self {
            db,
            namespace,
            access: McpAccess::default(),
        }
    }

    pub fn with_access(mut self, access: McpAccess) -> Self {
        self.access = access;
        self
    }

    /// A write transaction made by this agent.
    fn write<T>(
        &self,
        work: impl FnOnce(&mut grafeno::WriteTx) -> grafeno::Result<T>,
    ) -> grafeno::Result<T> {
        self.db.write(|tx| {
            tx.act_as(self.access.actor.clone());
            work(tx)
        })
    }

    /// Handles one line of input and returns the response, if any
    /// (notifications get none).
    pub fn handle_line(&self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => {
                return Some(error_response(
                    Value::Null,
                    -32700,
                    &format!("parse error: {error}"),
                ));
            }
        };
        if let Value::Array(messages) = message {
            let responses: Vec<Value> = messages
                .into_iter()
                .filter_map(|message| self.handle(message))
                .collect();
            return (!responses.is_empty()).then_some(Value::Array(responses));
        }
        self.handle(message)
    }

    fn handle(&self, message: Value) -> Option<Value> {
        // Messages without a method are responses to requests we never send.
        let method = message.get("method")?.as_str()?.to_owned();
        // Messages without an id are notifications: no response.
        let id = message.get("id")?.clone();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = match method.as_str() {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tools() })),
            "tools/call" => self.call_tool(&params),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error_response(id, code, &message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = requested
            .filter(|version| PROTOCOL_VERSIONS.contains(version))
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "grafeno", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })
    }

    fn call_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "missing tool name".to_owned()))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if self.access.read_only && WRITING_TOOLS.contains(&name) {
            return Ok(json!({
                "content": [{ "type": "text", "text": format!("{name} writes, and this MCP server is read-only") }],
                "isError": true,
            }));
        }
        let wanted = arguments
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or(&self.namespace);
        if !self.access.allows(wanted) {
            return Ok(json!({
                "content": [{ "type": "text", "text": format!("the namespace {wanted:?} is not open to this agent") }],
                "isError": true,
            }));
        }
        let outcome = match name {
            "remember" => self.remember(arguments),
            "recall" => self.recall(arguments),
            "context" => self.context(arguments),
            "relate" => self.relate(arguments),
            "update" => self.update(arguments),
            "forget" => self.forget(arguments),
            "get_node" => self.get_node(arguments),
            "query" => self.query(arguments),
            "merge" => self.merge(arguments),
            "undo_merge" => self.undo_merge(arguments),
            "maintain" => self.maintain(arguments),
            "find_matches" => self.find_matches(arguments),
            "extract" => self.extract(arguments),
            "add_mentions" => self.add_mentions(arguments),
            "review" => self.review(arguments),
            "resolve_review" => self.resolve_review(arguments),
            "consolidate" => self.consolidate(arguments),
            "overview" => self.overview(arguments),
            "changes" => self.changes(arguments),
            "summarize_communities" => self.summarize_communities(arguments),
            "set_summary" => self.set_summary(arguments),
            "checkpoint" => self.checkpoint(arguments),
            "resume" => self.resume(arguments),
            "ingest" => self.ingest(arguments),
            "contradictions" => self.contradictions(arguments),
            "timeline" => self.timeline(arguments),
            "feedback" => self.feedback(arguments),
            "palace_create" => self.palace_create(arguments),
            "palace_create_room" => self.palace_create_room(arguments),
            "palace_anchor" => self.palace_anchor(arguments),
            "palace_map" => self.palace_map(arguments),
            "palace_walk" => self.palace_walk(arguments),
            "palace_enter" => self.palace_enter(arguments),
            _ => return Err((-32602, format!("unknown tool: {name}"))),
        };
        Ok(match outcome {
            Ok(ToolOutput::Json(value)) => json!({
                "content": [{ "type": "text", "text": pretty(&value) }],
                "structuredContent": value,
                "isError": false,
            }),
            Ok(ToolOutput::Text(text)) => json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }),
            Err(message) => json!({
                "content": [{ "type": "text", "text": message }],
                "isError": true,
            }),
        })
    }

    fn ns<'a>(&'a self, namespace: &'a Option<String>) -> &'a str {
        namespace.as_deref().unwrap_or(&self.namespace)
    }

    // ------------------------------------------------------------ tools

    fn remember(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Link {
            label: String,
            to: String,
            #[serde(default)]
            valid_from: Option<TimeInput>,
            #[serde(default)]
            valid_to: Option<TimeInput>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            text: String,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            labels: Vec<String>,
            #[serde(default)]
            props: Props,
            #[serde(default)]
            sources: Vec<String>,
            #[serde(default)]
            embedding: Option<Vec<f32>>,
            #[serde(default)]
            links: Vec<Link>,
            #[serde(default)]
            importance: Option<f32>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let mut new = NewNode {
            labels: if args.labels.is_empty() {
                vec!["Memory".into()]
            } else {
                args.labels
            },
            props: args.props,
            sources: parse_ids(&args.sources)?,
            text: None,
            embedding: args.embedding,
        };
        // The name is searchable too.
        let text = match &args.name {
            Some(name) if !args.text.to_lowercase().contains(&name.to_lowercase()) => {
                format!("{name}: {}", args.text)
            }
            _ => args.text,
        };
        new.text = Some(text);
        if let Some(name) = args.name {
            new.props.insert("name".into(), Value::String(name));
        }
        let explicit_labels = new.labels != ["Memory"];
        let (id, edges, reused) = self
            .write(|tx| {
                // A named entity that already exists is updated instead of
                // stored twice: same normalized name (or alias) and, when
                // labels were given, one label in common.
                let mut existing = None;
                if let Some(name) = new.props.get("name").and_then(Value::as_str) {
                    for candidate in tx.find_by_name(ns, name)? {
                        let Some(node) = tx.node(ns, candidate)? else {
                            continue;
                        };
                        // A room or a palace of that name is not the entity.
                        if grafeno_memory::palace::is_structure(&node) {
                            continue;
                        }
                        if !explicit_labels || new.labels.iter().any(|label| node.has_label(label))
                        {
                            existing = Some(node);
                            break;
                        }
                    }
                }
                let (id, reused) = match existing {
                    Some(node) => {
                        let id = node.id;
                        let text = new.text.take().unwrap_or_default();
                        let old = tx.text(ns, id)?.unwrap_or_default();
                        if !old.to_lowercase().contains(&text.to_lowercase()) {
                            let joined = if old.is_empty() {
                                text
                            } else {
                                format!("{old}\n\n{text}")
                            };
                            tx.set_text(ns, id, &joined)?;
                        }
                        let mut props = std::mem::take(&mut new.props);
                        props.remove("name");
                        if !props.is_empty() {
                            tx.set_node_props(ns, id, props)?;
                        }
                        if explicit_labels {
                            for label in &new.labels {
                                tx.add_label(ns, id, label)?;
                            }
                        }
                        if let Some(embedding) = &new.embedding
                            && tx.embedding(ns, id)?.is_none()
                        {
                            tx.set_embedding(ns, id, embedding)?;
                        }
                        (id, true)
                    }
                    None => (tx.create_node(ns, new)?, false),
                };
                if let Some(importance) = args.importance {
                    tx.set_importance(ns, id, importance)?;
                }
                let mut edges = Vec::new();
                for link in args.links {
                    let mut edge = NewEdge::new(id, link.label, parse_id(&link.to)?);
                    edge.valid_from = link
                        .valid_from
                        .as_ref()
                        .map(TimeInput::resolve)
                        .transpose()?;
                    edge.valid_to = link.valid_to.as_ref().map(TimeInput::resolve).transpose()?;
                    edges.push(tx.create_edge(ns, edge)?);
                }
                Ok((id, edges, reused))
            })
            .map_err(describe)?;
        Ok(ToolOutput::Json(
            json!({ "id": id, "edges": edges, "reused": reused }),
        ))
    }

    fn recall(&self, arguments: Value) -> ToolResult {
        let args: QueryArgs = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let query = args.query(10)?;
        let read = self.db.begin_read().map_err(describe)?;
        let results = recall(&read, ns, &query).map_err(describe)?;
        let names: HashMap<NodeId, String> = results
            .iter()
            .map(|result| (result.node.id, result.node.display_name()))
            .collect();
        let ids: Vec<NodeId> = results.iter().map(|result| result.node.id).collect();
        let mut out = Vec::new();
        for result in results {
            let text = read
                .text(ns, result.node.id)
                .map_err(describe)?
                .unwrap_or_default();
            let mut why = Vec::new();
            for signal in &result.signals {
                why.push(match signal {
                    Signal::Text { rank, .. } => format!("text match #{rank}"),
                    Signal::Vector { rank, similarity } => {
                        format!("similar meaning #{rank} ({similarity:.2})")
                    }
                    Signal::Graph { seed, hops } => {
                        let seed_name = match names.get(seed) {
                            Some(name) => name.clone(),
                            None => name_of(&read, ns, *seed),
                        };
                        format!("{hops} hop(s) from {seed_name}")
                    }
                    Signal::Memory { importance, .. } => format!("importance {importance:.2}"),
                });
            }
            out.push(json!({
                "id": result.node.id,
                "name": result.node.display_name(),
                "labels": result.node.labels,
                "score": result.score,
                "text": snippet(&text),
                "why": why,
            }));
        }
        drop(read);
        self.touch(ns, &ids)?;
        Ok(ToolOutput::Json(json!({ "results": out })))
    }

    /// Records an access to recalled nodes, which slows their decay.
    fn touch(&self, ns: &str, ids: &[NodeId]) -> Result<(), String> {
        // A read-only agent leaves no trace, accesses included.
        if ids.is_empty() || self.access.read_only {
            return Ok(());
        }
        self.write(|tx| tx.touch(ns, ids, grafeno::Timestamp::now()))
            .map_err(describe)
    }

    fn context(&self, arguments: Value) -> ToolResult {
        let args: QueryArgs = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let query = args.query(20)?;
        let mut options = ContextOptions::with_budget(args.budget.unwrap_or(1_500));
        // Community summaries that match go first, as an overview.
        options.communities = 3;
        let read = self.db.begin_read().map_err(describe)?;
        let context = context_for(&read, ns, &query, &options).map_err(describe)?;
        drop(read);
        let ids: Vec<NodeId> = context.items.iter().map(|item| item.node).collect();
        self.touch(ns, &ids)?;
        Ok(ToolOutput::Text(if context.items.is_empty() {
            "No relevant memories found.".into()
        } else {
            context.rendered
        }))
    }

    fn relate(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            from: String,
            label: String,
            to: String,
            #[serde(default)]
            valid_from: Option<TimeInput>,
            #[serde(default)]
            valid_to: Option<TimeInput>,
            #[serde(default)]
            props: Props,
            #[serde(default)]
            sources: Vec<String>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let mut edge = NewEdge::new(
            parse_id(&args.from).map_err(describe)?,
            args.label,
            parse_id(&args.to).map_err(describe)?,
        );
        edge.props = args.props;
        edge.sources = parse_ids(&args.sources)?;
        edge.valid_from = args
            .valid_from
            .as_ref()
            .map(TimeInput::resolve)
            .transpose()
            .map_err(describe)?;
        edge.valid_to = args
            .valid_to
            .as_ref()
            .map(TimeInput::resolve)
            .transpose()
            .map_err(describe)?;
        let (id, events) = self
            .write(|tx| {
                let id = tx.create_edge(ns, edge)?;
                Ok((id, tx.take_constraint_events()))
            })
            .map_err(describe)?;
        if events.is_empty() {
            return Ok(ToolOutput::Json(json!({ "id": id })));
        }
        // Say what the new fact contradicted, and what was done.
        let changed: Vec<Value> = events
            .iter()
            .map(|event| {
                json!({
                    "fact": event.old_edge,
                    "action": event.action,
                    "constraint": event.constraint,
                    "until": event.at,
                })
            })
            .collect();
        Ok(ToolOutput::Json(
            json!({ "id": id, "contradicted": changed }),
        ))
    }

    fn update(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            id: String,
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            props: Props,
            #[serde(default)]
            add_labels: Vec<String>,
            #[serde(default)]
            remove_labels: Vec<String>,
            #[serde(default)]
            remove_props: Vec<String>,
            #[serde(default)]
            importance: Option<f32>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let id = parse_id(&args.id).map_err(describe)?;
        let mut props = args.props;
        if let Some(name) = args.name {
            props.insert("name".into(), Value::String(name));
        }
        self.write(|tx| {
            tx.node(ns, id)?.ok_or(Error::NodeNotFound(id))?;
            if !props.is_empty() {
                tx.set_node_props(ns, id, props)?;
            }
            if !args.remove_props.is_empty() {
                let keys: Vec<&str> = args.remove_props.iter().map(String::as_str).collect();
                tx.remove_node_props(ns, id, &keys)?;
            }
            for label in &args.add_labels {
                tx.add_label(ns, id, label)?;
            }
            for label in &args.remove_labels {
                tx.remove_label(ns, id, label)?;
            }
            if let Some(text) = &args.text {
                tx.set_text(ns, id, text)?;
            }
            if let Some(importance) = args.importance {
                tx.set_importance(ns, id, importance)?;
            }
            Ok(())
        })
        .map_err(describe)?;
        self.get_node(json!({ "id": id, "namespace": ns }))
    }

    fn forget(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            id: String,
            #[serde(default)]
            mode: Option<String>,
            #[serde(default)]
            at: Option<TimeInput>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let id = parse_id(&args.id).map_err(describe)?;
        let edge_id = grafeno::EdgeId::from_u128(id.as_u128());
        let read = self.db.begin_read().map_err(describe)?;
        let is_node = read.node(ns, id).map_err(describe)?.is_some();
        let is_edge = read.edge(ns, edge_id).map_err(describe)?.is_some();
        drop(read);
        let mode = args.mode.as_deref();
        let done = match (is_node, is_edge, mode) {
            (true, _, None | Some("delete")) => {
                self.write(|tx| tx.delete_node(ns, id)).map_err(describe)?;
                "deleted node and its edges"
            }
            (true, _, Some(other)) => {
                return Err(format!(
                    "a node can only be forgotten with mode \"delete\", not {other:?}"
                ));
            }
            (false, true, None | Some("invalidate")) => {
                let at = args
                    .at
                    .as_ref()
                    .map(TimeInput::resolve)
                    .transpose()
                    .map_err(describe)?;
                self.write(|tx| {
                    tx.invalidate_edge(ns, edge_id, at.unwrap_or_else(grafeno::Timestamp::now))
                })
                .map_err(describe)?;
                "invalidated fact (kept in history)"
            }
            (false, true, Some("retract")) => {
                self.write(|tx| tx.retract_edge(ns, edge_id))
                    .map_err(describe)?;
                "retracted fact (kept in history as a mistake)"
            }
            (false, true, Some("delete")) => {
                self.write(|tx| tx.delete_edge(ns, edge_id))
                    .map_err(describe)?;
                "deleted fact"
            }
            (false, true, Some(other)) => {
                return Err(format!(
                    "unknown mode {other:?}: use \"invalidate\", \"retract\" or \"delete\""
                ));
            }
            (false, false, _) => return Err(format!("no node or fact with id {id}")),
        };
        Ok(ToolOutput::Json(json!({ "id": id, "result": done })))
    }

    fn query(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            query: String,
            #[serde(default)]
            params: serde_json::Map<String, Value>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let params = grafeno_query::params_from_json(&args.params);
        let read = self.db.begin_read().map_err(describe)?;
        let result = grafeno_query::execute_read_with(
            &read,
            ns,
            &args.query,
            &params,
            &grafeno_query::Limits::standard(),
        )
        .map_err(|error| error.to_string())?;
        let total = result.rows.len();
        let mut json = result.to_json();
        if let Some(rows) = json["rows"].as_array_mut() {
            rows.truncate(QUERY_ROWS);
        }
        json["truncated"] = Value::Bool(total > QUERY_ROWS);
        if let Some(object) = json.as_object_mut() {
            object.remove("stats");
        }
        Ok(ToolOutput::Json(json))
    }

    fn get_node(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            id: String,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let id = parse_id(&args.id).map_err(describe)?;
        let read = self.db.begin_read().map_err(describe)?;
        let node = read
            .node(ns, id)
            .map_err(describe)?
            .ok_or_else(|| format!("no node with id {id}"))?;
        let text = read.text(ns, id).map_err(describe)?;
        let memory = read.memory(ns, &node).map_err(describe)?;
        let mut facts = Vec::new();
        for neighbor in read
            .neighbors(ns, id, Direction::Both, &EdgeFilter::any())
            .map_err(describe)?
            .into_iter()
            .take(50)
        {
            let edge = &neighbor.edge;
            facts.push(json!({
                "id": edge.id,
                "direction": if edge.from == id { "out" } else { "in" },
                "label": edge.label,
                "other": { "id": neighbor.node, "name": name_of(&read, ns, neighbor.node) },
                "valid_from": edge.valid_from.map(|at| at.date()),
                "valid_to": edge.valid_to.map(|at| at.date()),
            }));
        }
        Ok(ToolOutput::Json(json!({
            "id": node.id,
            "name": node.display_name(),
            "labels": node.labels,
            "props": node.props,
            "text": text,
            "sources": node.sources,
            "created": node.created_at.date(),
            "updated": node.updated_at.date(),
            "importance": round2(memory.importance),
            "accesses": memory.accesses,
            "last_access": memory.last_access.date(),
            "facts": facts,
        })))
    }

    fn merge(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            keep: String,
            merge: String,
            #[serde(default)]
            force: bool,
            #[serde(default)]
            functional: Vec<String>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let keep = parse_id(&args.keep).map_err(describe)?;
        let merged = parse_id(&args.merge).map_err(describe)?;
        let options = grafeno::MergeOptions {
            force: args.force,
            functional: args.functional,
            ..grafeno::MergeOptions::default()
        };
        let report = self
            .write(|tx| tx.merge_nodes_with(ns, keep, merged, &options))
            .map_err(describe)?;
        let conflicts: Vec<String> = report
            .conflicts
            .iter()
            .map(grafeno::Conflict::describe)
            .collect();
        Ok(ToolOutput::Json(json!({
            "id": report.kept,
            "merged": report.merged,
            "merge_id": report.id,
            "facts_moved": report.edges_moved,
            "facts_dropped": report.edges_dropped,
            "conflicts_overridden": conflicts,
        })))
    }

    fn undo_merge(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            merge_id: String,
            #[serde(default)]
            force: bool,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let id: grafeno::MergeId = args
            .merge_id
            .parse::<uuid::Uuid>()
            .map(grafeno::MergeId::from)
            .map_err(|_| format!("not a merge id: {:?}", args.merge_id))?;
        let record = self
            .write(|tx| tx.undo_merge(ns, id, args.force))
            .map_err(describe)?;
        Ok(ToolOutput::Json(json!({
            "restored": record.merged,
            "kept": record.kept,
            "merge_id": record.id,
        })))
    }

    fn overview(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            query: Option<String>,
            #[serde(default)]
            id: Option<String>,
            #[serde(default)]
            level: Option<i64>,
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let read = self.db.begin_read().map_err(describe)?;
        let limit = args.limit.unwrap_or(10).min(100);
        if let Some(id) = &args.id {
            let opened = grafeno_memory::community_members(
                &read,
                ns,
                parse_id(id).map_err(describe)?,
                limit,
            )
            .map_err(describe)?;
            let members: Vec<Value> = opened
                .members
                .iter()
                .map(|node| json!({ "id": node.id, "name": node.display_name(), "labels": node.labels }))
                .collect();
            return Ok(ToolOutput::Json(json!({
                "community": opened.community,
                "children": opened.children,
                "members": members,
            })));
        }
        let hits = grafeno_memory::overview(
            &read,
            ns,
            args.query.as_deref(),
            &grafeno_memory::OverviewOptions {
                limit,
                level: args.level,
            },
        )
        .map_err(describe)?;
        if hits.is_empty() {
            return Ok(ToolOutput::Text(
                "No community summaries. Run summarize_communities first.".into(),
            ));
        }
        Ok(ToolOutput::Json(json!({ "communities": hits })))
    }

    fn changes(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            since: u64,
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let page = self
            .db
            .begin_read()
            .map_err(describe)?
            .changes(ns, args.since, args.limit.unwrap_or(100).min(1_000))
            .map_err(describe)?;
        Ok(ToolOutput::Json(json!(page)))
    }

    fn summarize_communities(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            min_size: Option<usize>,
            #[serde(default)]
            resolution: Option<f64>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let mut options = grafeno_memory::CommunityOptions::default();
        if let Some(min_size) = args.min_size {
            options.min_size = min_size;
        }
        if let Some(resolution) = args.resolution {
            options.resolution = resolution;
        }
        let plan = grafeno_memory::plan_communities(
            &self.db.begin_read().map_err(describe)?,
            ns,
            &options,
        )
        .map_err(describe)?;
        let report = self
            .write(|tx| grafeno_memory::apply_communities(tx, ns, &plan))
            .map_err(describe)?;
        Ok(ToolOutput::Json(json!(report)))
    }

    fn set_summary(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            id: String,
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let id = parse_id(&args.id).map_err(describe)?;
        self.write(|tx| grafeno_memory::set_community_text(tx, ns, id, args.text.as_deref()))
            .map_err(describe)?;
        Ok(ToolOutput::Json(
            json!({ "id": id, "custom": args.text.is_some() }),
        ))
    }

    fn extract(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            force: bool,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace).to_owned();
        let options = grafeno_memory::ExtractOptions {
            force: args.force,
            ..grafeno_memory::ExtractOptions::default()
        };
        let plan = grafeno_memory::plan(&self.db.begin_read().map_err(describe)?, &ns, &options)
            .map_err(describe)?;
        let mut report = grafeno_memory::ExtractReport {
            unchanged: plan.unchanged,
            ..Default::default()
        };
        for batch in plan.sources.chunks(options.batch) {
            let read = self.db.begin_read().map_err(describe)?;
            let done = self
                .write(|tx| grafeno_memory::extract_batch(tx, &read, &ns, &plan, batch, &options))
                .map_err(describe)?;
            report.read += done.read;
            report.mentions += done.mentions;
            report.linked += done.linked;
            report.created += done.created;
            report.queued += done.queued;
        }
        let waiting =
            grafeno_memory::review_queue(&self.db.begin_read().map_err(describe)?, &ns, 1_000)
                .map_err(describe)?
                .len();
        Ok(ToolOutput::Json(json!({
            "texts_read": report.read,
            "texts_unchanged": report.unchanged,
            "mentions": report.mentions,
            "linked": report.linked,
            "created": report.created,
            "waiting_for_review": waiting,
        })))
    }

    fn add_mentions(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            id: String,
            mentions: Vec<grafeno_memory::AgentMention>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let source = parse_id(&args.id).map_err(describe)?;
        let read = self.db.begin_read().map_err(describe)?;
        let report = self
            .write(|tx| grafeno_memory::ingest(tx, &read, ns, source, &args.mentions, "Entity"))
            .map_err(describe)?;
        Ok(ToolOutput::Json(json!({
            "mentions": report.mentions,
            "linked": report.linked,
            "created": report.created,
            "waiting_for_review": report.queued,
        })))
    }

    fn review(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let read = self.db.begin_read().map_err(describe)?;
        let items = grafeno_memory::review_queue(&read, ns, args.limit.unwrap_or(20).min(100))
            .map_err(describe)?;
        let rows: Vec<Value> = items
            .iter()
            .map(|item| {
                let candidates: Vec<Value> = item
                    .candidates
                    .iter()
                    .map(|id| {
                        let name = read.node(ns, *id).ok().flatten().map(|n| n.display_name());
                        json!({ "id": id, "name": name })
                    })
                    .collect();
                json!({
                    "key": item.key,
                    "kind": item.kind,
                    "text": item.occurrences.first().map(|o| o.surface.clone()),
                    "seen_in": item.occurrences.len(),
                    "candidates": candidates,
                })
            })
            .collect();
        Ok(ToolOutput::Json(json!({
            "items": rows,
            "hint": "Settle each with resolve_review: link to a candidate, create a new entity, or dismiss.",
        })))
    }

    fn resolve_review(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            key: String,
            action: String,
            #[serde(default)]
            node: Option<String>,
            #[serde(default)]
            label: Option<String>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let resolution = match args.action.as_str() {
            "link" => grafeno_memory::Resolution::Link {
                node: parse_id(args.node.as_deref().ok_or("link needs a node")?)
                    .map_err(describe)?,
            },
            "create" => grafeno_memory::Resolution::Create { label: args.label },
            "dismiss" => grafeno_memory::Resolution::Dismiss,
            other => return Err(format!("unknown action {other:?}: link, create or dismiss")),
        };
        let (node, report) = self
            .write(|tx| grafeno_memory::resolve(tx, ns, &args.key, resolution))
            .map_err(describe)?;
        Ok(ToolOutput::Json(
            json!({ "id": node, "mentions": report.mentions }),
        ))
    }

    fn find_matches(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            label: String,
            props: grafeno::Props,
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let probe = grafeno::Probe {
            label: args.label,
            props: args.props,
            neighbors: Vec::new(),
        };
        let read = self.db.begin_read().map_err(describe)?;
        let found = read
            .find_matches(ns, &probe, None, args.limit.unwrap_or(5).min(50), None)
            .map_err(describe)?;
        let rows: Vec<Value> = found
            .iter()
            .map(|m| {
                let name = read
                    .node(ns, m.node)
                    .ok()
                    .flatten()
                    .map(|node| node.display_name());
                json!({
                    "id": m.node,
                    "name": name,
                    "score": (m.score * 1000.0).round() / 1000.0,
                    "decision": m.decision,
                    "decided_by": m.decided_by,
                    "fields": m.fields.iter().map(|f| json!({
                        "key": f.key,
                        "similarity": (f.similarity * 1000.0).round() / 1000.0,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(ToolOutput::Json(json!({
            "matches": rows,
            "hint": "same: link to or merge with that node; review: check the fields before deciding; different: a new entity.",
        })))
    }

    fn maintain(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default)]
            check: bool,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let limit = args.limit.unwrap_or(10);
        let read = self.db.begin_read().map_err(describe)?;
        let brief = |node: &Node| json!({ "id": node.id, "name": node.display_name(), "labels": node.labels });

        let options = DuplicateOptions {
            limit,
            ..DuplicateOptions::default()
        };
        let duplicates: Vec<Value> = find_duplicates(&read, ns, &options)
            .map_err(describe)?
            .iter()
            .map(|pair| {
                json!({
                    "a": brief(&pair.a),
                    "b": brief(&pair.b),
                    "score": round2(pair.score),
                    "reasons": pair.reasons,
                })
            })
            .collect();

        let options = DecayOptions {
            limit,
            ..DecayOptions::default()
        };
        let weak: Vec<Value> = weak_memories(&read, ns, &options)
            .map_err(describe)?
            .iter()
            .map(|item| {
                let mut value = brief(&item.node);
                value["strength"] = json!(round2(item.strength));
                value["last_access"] = json!(item.memory.last_access.date());
                value
            })
            .collect();

        let options = ConsolidationOptions {
            limit,
            ..ConsolidationOptions::default()
        };
        let mut consolidation = Vec::new();
        for group in consolidation_groups(&read, ns, &options).map_err(describe)? {
            let mut memories = Vec::new();
            for memory in &group.memories {
                let text = read
                    .text(ns, memory.id)
                    .map_err(describe)?
                    .unwrap_or_default();
                memories.push(json!({ "id": memory.id, "text": snippet(&text) }));
            }
            consolidation.push(json!({ "about": brief(&group.entity), "memories": memories }));
        }

        // The namespace's size, its vector graphs' wear and, when asked,
        // a full integrity check (it reads every record).
        let stats = read.stats(ns).map_err(describe)?;
        let vectors: Vec<Value> = read
            .vector_health(ns)
            .map_err(describe)?
            .iter()
            .map(|graph| {
                json!({
                    "index": graph.index,
                    "vectors": graph.vectors,
                    "removed_since_built": graph.removed,
                    "churn": (graph.churn * 100.0).round() / 100.0,
                })
            })
            .collect();
        let mut health = json!({
            "nodes": stats.nodes,
            "edges": stats.edges,
            "texts": stats.texts,
            "vector_graphs": vectors,
            "contradictions": read.constraint_events(ns, 10_000).map_err(describe)?.len(),
        });
        if args.check {
            let report = read.check(ns).map_err(describe)?;
            health["check"] = json!({
                "ok": report.is_ok(),
                "problems": report.problems.iter().take(limit).collect::<Vec<_>>(),
                "unreachable_vectors": report.unreachable_vectors,
            });
        }

        Ok(ToolOutput::Json(json!({
            "health": health,
            "duplicates": duplicates,
            "weak": weak,
            "consolidation": consolidation,
            "hint": "Merge true duplicates with `merge`; relate different ones with \
                distinct_from. Summarize each consolidation group with `consolidate`. Weak \
                memories can be left alone, made more important with `update`, or deleted \
                with `forget`.",
        })))
    }

    fn consolidate(&self, arguments: Value) -> ToolResult {
        fn yes() -> bool {
            true
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            summary: String,
            sources: Vec<String>,
            #[serde(default)]
            about: Option<String>,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            embedding: Option<Vec<f32>>,
            #[serde(default = "yes")]
            archive_sources: bool,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        if args.sources.is_empty() {
            return Err("`sources` must name the memories being summarized".into());
        }
        let summary = Summary {
            text: args.summary,
            name: args.name,
            sources: parse_ids(&args.sources)?,
            about: args
                .about
                .as_deref()
                .map(parse_id)
                .transpose()
                .map_err(describe)?,
            embedding: args.embedding,
            archive_sources: args.archive_sources,
            ..Summary::default()
        };
        let count = summary.sources.len();
        let id = self
            .write(|tx| write_summary(tx, ns, summary))
            .map_err(describe)?;
        Ok(ToolOutput::Json(json!({
            "id": id,
            "summarized": count,
            "archived": args.archive_sources,
        })))
    }

    fn checkpoint(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            task: String,
            summary: String,
            #[serde(default)]
            done: Vec<String>,
            #[serde(default)]
            next: Vec<String>,
            #[serde(default)]
            decisions: Vec<String>,
            #[serde(default)]
            files: Vec<String>,
            #[serde(default)]
            about: Option<String>,
            #[serde(default)]
            agent: Option<String>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let about = args
            .about
            .as_deref()
            .map(parse_id)
            .transpose()
            .map_err(describe)?;
        let change = self
            .db
            .begin_read()
            .and_then(|read| read.changes(ns, 0, 1))
            .map(|page| page.last)
            .unwrap_or(0);
        let agent = args
            .agent
            .or_else(|| self.access.actor.as_ref().map(|actor| actor.name.clone()));
        let new = grafeno_memory::NewCheckpoint {
            task: args.task,
            summary: args.summary,
            done: args.done,
            next: args.next,
            decisions: args.decisions,
            files: args.files,
            about,
            agent,
            change,
        };
        let id = self
            .write(|tx| grafeno_memory::save_checkpoint(tx, ns, new))
            .map_err(describe)?;
        let read = self.db.begin_read().map_err(describe)?;
        let saved = grafeno_memory::checkpoints(&read, ns, &grafeno_memory::ResumeQuery::default())
            .map_err(describe)?
            .into_iter()
            .find(|checkpoint| checkpoint.id == id);
        Ok(ToolOutput::Json(json!({
            "id": id,
            "follows": saved.and_then(|checkpoint| checkpoint.previous),
        })))
    }

    fn resume(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            task: Option<String>,
            #[serde(default)]
            about: Option<String>,
            #[serde(default)]
            agent: Option<String>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let query = grafeno_memory::ResumeQuery {
            task: args.task,
            about: args
                .about
                .as_deref()
                .map(parse_id)
                .transpose()
                .map_err(describe)?,
            agent: args.agent,
        };
        let read = self.db.begin_read().map_err(describe)?;
        let all = grafeno_memory::checkpoints(&read, ns, &query).map_err(describe)?;
        let Some(latest) = all.first().cloned() else {
            let tasks = grafeno_memory::tasks(&read, ns, &grafeno_memory::ResumeQuery::default())
                .map_err(describe)?;
            let tasks: Vec<Value> = tasks
                .iter()
                .take(20)
                .map(|checkpoint| {
                    json!({
                        "task": checkpoint.task,
                        "at": checkpoint.at.date(),
                        "agent": checkpoint.agent,
                        "summary": snippet(&checkpoint.summary),
                    })
                })
                .collect();
            return Ok(ToolOutput::Json(json!({
                "found": false,
                "tasks": tasks,
                "hint": "No checkpoint matches. These are the tasks with checkpoints; resume one by its task.",
            })));
        };
        let same_task = all
            .iter()
            .filter(|checkpoint| {
                grafeno_memory::checkpoint::task_key(&checkpoint.task)
                    == grafeno_memory::checkpoint::task_key(&latest.task)
            })
            .count();
        // What was written since, by anyone, its own write left out.
        let since = read.changes(ns, latest.change, 50).map_err(describe)?;
        let mut changed = Vec::new();
        for change in since.changes.iter() {
            if change.node == Some(latest.id)
                || change.from == Some(latest.id)
                || change
                    .labels
                    .iter()
                    .any(|l| l == grafeno_memory::CHECKPOINT)
            {
                continue;
            }
            let what = match (change.node, change.from, change.to) {
                // An unnamed memory by the start of its text.
                (Some(node), _, _) => match read.node(ns, node).map_err(describe)? {
                    Some(record) if record.explicit_name().is_none() => {
                        let text = read.text(ns, node).map_err(describe)?.unwrap_or_default();
                        text.chars().take(80).collect()
                    }
                    _ => name_of(&read, ns, node),
                },
                (None, Some(from), Some(to)) => format!(
                    "{} {} {}",
                    name_of(&read, ns, from),
                    change.labels.join(", "),
                    name_of(&read, ns, to)
                ),
                _ => change.labels.join(", "),
            };
            changed.push(json!({ "op": change.op, "what": what, "labels": change.labels }));
        }
        let about = latest
            .about
            .map(|id| json!({ "id": id, "name": name_of(&read, ns, id) }));
        Ok(ToolOutput::Json(json!({
            "found": true,
            "checkpoint": latest,
            "about": about,
            "checkpoints_of_this_task": same_task,
            "changed_since": changed,
            "more_changes": since.changes.len() >= 50,
        })))
    }

    fn ingest(&self, arguments: Value) -> ToolResult {
        fn yes() -> bool {
            true
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            content: String,
            #[serde(default)]
            kind: grafeno_memory::Kind,
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            source: Option<String>,
            #[serde(default)]
            labels: Vec<String>,
            #[serde(default)]
            max_chars: Option<usize>,
            #[serde(default = "yes")]
            extract: bool,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace).to_owned();
        let new = grafeno_memory::NewDocument {
            content: args.content,
            kind: args.kind,
            title: args.title,
            source: args.source,
            labels: args.labels,
            max_chars: args
                .max_chars
                .unwrap_or(grafeno_memory::DEFAULT_CHUNK_CHARS),
        };
        let done = self
            .write(|tx| grafeno_memory::ingest_document(tx, &ns, new))
            .map_err(describe)?;
        let mut out = json!({
            "id": done.id,
            "chunks": done.chunks.len(),
            "chunk_ids": done.chunks,
            "replaced": done.replaced,
        });
        if args.extract {
            let extracted = self.extract(json!({ "namespace": ns }))?;
            if let ToolOutput::Json(report) = extracted {
                out["extraction"] = report;
            }
        }
        Ok(ToolOutput::Json(out))
    }

    fn contradictions(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            node: Option<String>,
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let node = args
            .node
            .as_deref()
            .map(parse_id)
            .transpose()
            .map_err(describe)?;
        let limit = args.limit.unwrap_or(20);
        let read = self.db.begin_read().map_err(describe)?;
        let fact = |id: grafeno::EdgeId| -> Result<Option<Value>, String> {
            let Some(edge) = read.edge(ns, id).map_err(describe)? else {
                return Ok(None);
            };
            Ok(Some(json!({
                "id": edge.id,
                "text": format!(
                    "{} {} {}",
                    name_of(&read, ns, edge.from),
                    edge.label,
                    name_of(&read, ns, edge.to)
                ),
                "valid_from": edge.valid_from.map(|at| at.date()),
                "valid_to": edge.valid_to.map(|at| at.date()),
                "touches": [edge.from, edge.to],
            })))
        };
        let mut out = Vec::new();
        for event in read.constraint_events(ns, 10_000).map_err(describe)? {
            if out.len() >= limit {
                break;
            }
            let (new, old) = (fact(event.new_edge)?, fact(event.old_edge)?);
            if let Some(node) = node {
                let touches = |fact: &Option<Value>| {
                    fact.as_ref().is_some_and(|fact| {
                        fact["touches"]
                            .as_array()
                            .is_some_and(|ends| ends.iter().any(|end| end == &json!(node)))
                    })
                };
                if !touches(&new) && !touches(&old) {
                    continue;
                }
            }
            out.push(json!({
                "constraint": event.constraint,
                "action": event.action,
                "new": new,
                "old": old,
                "replacement": event.replacement,
                "ends_at": event.at.map(|at| at.date()),
                "at": event.recorded_at.date(),
                "by": event.by,
            }));
        }
        Ok(ToolOutput::Json(json!({
            "contradictions": out,
            "hint": "Each is a fact that contradicted an earlier one under a constraint, and what \
                was done: the earlier one invalidated (it ended), the new one refused, or both \
                kept and flagged. Fix a flagged pair with `forget` (\"invalidate\" or \"retract\").",
        })))
    }

    fn timeline(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            id: String,
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let id = parse_id(&args.id).map_err(describe)?;
        let read = self.db.begin_read().map_err(describe)?;
        let moments =
            grafeno_memory::timeline(&read, ns, id, args.limit.unwrap_or(100)).map_err(describe)?;
        let moments: Vec<Value> = moments
            .iter()
            .map(|moment| {
                json!({
                    "at": moment.at.date(),
                    "kind": moment.kind,
                    "text": moment.text,
                    "fact": moment.fact,
                    "by": moment.by,
                })
            })
            .collect();
        Ok(ToolOutput::Json(json!({
            "id": id,
            "name": name_of(&read, ns, id),
            "timeline": moments,
        })))
    }

    fn feedback(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            id: String,
            verdict: String,
            #[serde(default)]
            note: Option<String>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let id = parse_id(&args.id).map_err(describe)?;
        let edge_id = grafeno::EdgeId::from_u128(id.as_u128());
        let read = self.db.begin_read().map_err(describe)?;
        let node = read.node(ns, id).map_err(describe)?;
        let is_edge = read.edge(ns, edge_id).map_err(describe)?.is_some();
        let importance = match &node {
            Some(node) => Some(read.memory(ns, node).map_err(describe)?.importance),
            None => None,
        };
        drop(read);
        let now = grafeno::Timestamp::now();
        let note = args.note.clone();
        let mark = |tx: &mut grafeno::WriteTx, verdict: &str| -> grafeno::Result<()> {
            let mut props = Props::new();
            props.insert("feedback".into(), json!(verdict));
            props.insert("feedback_at".into(), json!(now.date()));
            if let Some(note) = &note {
                props.insert("feedback_note".into(), json!(note));
            }
            tx.set_node_props(ns, id, props)
        };
        let done = match (node.is_some(), is_edge, args.verdict.as_str()) {
            (true, _, "useful") => {
                let raised = (importance.unwrap_or(0.5) + 0.1).min(1.0);
                self.write(|tx| {
                    tx.set_importance(ns, id, raised)?;
                    tx.touch(ns, &[id], now)?;
                    mark(tx, "useful")
                })
                .map_err(describe)?;
                format!("importance raised to {:.2}", raised)
            }
            (true, _, "obsolete") => {
                self.write(|tx| {
                    grafeno_memory::archive(tx, ns, &[id])?;
                    mark(tx, "obsolete")
                })
                .map_err(describe)?;
                "archived: kept, but left out of recall".to_owned()
            }
            (true, _, "wrong") => {
                self.write(|tx| {
                    grafeno_memory::archive(tx, ns, &[id])?;
                    tx.set_importance(ns, id, 0.0)?;
                    mark(tx, "wrong")
                })
                .map_err(describe)?;
                "marked wrong and archived: kept for the record, left out of recall".to_owned()
            }
            (false, true, "useful") => {
                return Err("a fact cannot be rated useful: rate the node it is about".into());
            }
            (false, true, "obsolete") => {
                self.write(|tx| tx.invalidate_edge(ns, edge_id, now))
                    .map_err(describe)?;
                "fact invalidated: it ended now, and its history is kept".to_owned()
            }
            (false, true, "wrong") => {
                self.write(|tx| tx.retract_edge(ns, edge_id))
                    .map_err(describe)?;
                "fact retracted: kept in history as a mistake".to_owned()
            }
            (_, _, other) if !matches!(other, "useful" | "obsolete" | "wrong") => {
                return Err(format!(
                    "unknown verdict {other:?}: use \"useful\", \"wrong\" or \"obsolete\""
                ));
            }
            _ => return Err(format!("no node or fact with id {id}")),
        };
        Ok(ToolOutput::Json(
            json!({ "id": id, "verdict": args.verdict, "result": done }),
        ))
    }
    // ----------------------------------------------------------- palace

    /// A palace, by its id or its name.
    fn palace_ref(&self, read: &ReadTx, ns: &str, given: &str) -> Result<NodeId, String> {
        if let Ok(id) = parse_id(given) {
            return Ok(id);
        }
        grafeno_memory::find_palace(read, ns, given)
            .map_err(describe)?
            .ok_or_else(|| format!("no palace named {given:?}; palace_map lists the palaces"))
    }

    /// A room, by its id, or by its name in the palace given; without a
    /// palace, by its name when one palace has a room of that name.
    fn room_ref(
        &self,
        read: &ReadTx,
        ns: &str,
        palace: Option<&str>,
        given: &str,
    ) -> Result<NodeId, String> {
        if let Ok(id) = parse_id(given) {
            return Ok(id);
        }
        if let Some(palace) = palace {
            let palace = self.palace_ref(read, ns, palace)?;
            return grafeno_memory::find_room(read, ns, palace, given)
                .map_err(describe)?
                .ok_or_else(|| {
                    format!(
                        "no room named {given:?} in {}; palace_create_room makes one",
                        name_of(read, ns, palace)
                    )
                });
        }
        let key = grafeno::normalize_name(given);
        let mut found: Vec<(NodeId, String)> = Vec::new();
        let options = grafeno_memory::MapOptions::default();
        for palace in grafeno_memory::palaces(read, ns, &options).map_err(describe)? {
            for room in palace.rooms {
                if grafeno::normalize_name(&room.name) == key {
                    found.push((room.id, palace.name.clone()));
                }
            }
        }
        match found.as_slice() {
            [] => Err(format!(
                "no room named {given:?}; palace_map lists the rooms"
            )),
            [(id, _)] => Ok(*id),
            several => Err(format!(
                "several palaces have a room named {given:?} ({}): say which with `palace`",
                several
                    .iter()
                    .map(|(_, palace)| palace.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn palace_create(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            name: String,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let made = self
            .write(|tx| {
                grafeno_memory::create_palace(tx, ns, &args.name, args.description.as_deref())
            })
            .map_err(describe)?;
        let read = self.db.begin_read().map_err(describe)?;
        Ok(ToolOutput::Json(json!({
            "id": made.id,
            "name": name_of(&read, ns, made.id),
            "created": made.created,
        })))
    }

    fn palace_create_room(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            palace: String,
            name: String,
            #[serde(default)]
            topic: Option<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            loci: Vec<String>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let read = self.db.begin_read().map_err(describe)?;
        let palace = self.palace_ref(&read, ns, &args.palace)?;
        drop(read);
        let new = grafeno_memory::NewRoom {
            name: args.name,
            topic: args.topic,
            description: args.description,
            loci: args.loci,
        };
        let made = self
            .write(|tx| grafeno_memory::create_room(tx, ns, palace, new))
            .map_err(describe)?;
        let read = self.db.begin_read().map_err(describe)?;
        let room =
            grafeno_memory::map_room(&read, ns, made.id, &grafeno_memory::MapOptions::default())
                .map_err(describe)?;
        Ok(ToolOutput::Json(json!({
            "id": made.id,
            "created": made.created,
            "palace": { "id": palace, "name": name_of(&read, ns, palace) },
            "room": room,
        })))
    }

    fn palace_anchor(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            memory: String,
            room: String,
            #[serde(default)]
            locus: Option<String>,
            #[serde(default)]
            palace: Option<String>,
            #[serde(default)]
            note: Option<String>,
            #[serde(default)]
            remove: bool,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let memory = parse_id(&args.memory).map_err(describe)?;
        let locus_name = args
            .locus
            .as_deref()
            .unwrap_or(grafeno_memory::palace::ENTRANCE);
        let read = self.db.begin_read().map_err(describe)?;
        let room = self.room_ref(&read, ns, args.palace.as_deref(), &args.room)?;
        let existing = grafeno_memory::find_locus(&read, ns, room, locus_name);
        drop(read);
        let result = if args.remove {
            let removed = match existing.map_err(describe)? {
                Some(locus) => self
                    .write(|tx| grafeno_memory::unanchor(tx, ns, memory, locus))
                    .map_err(describe)?,
                None => false,
            };
            json!({ "removed": removed })
        } else {
            // A locus that is not there yet is made: a room and a palace
            // are not, they are made on purpose.
            let (locus, anchored) = self
                .write(|tx| {
                    let locus = grafeno_memory::create_locus(tx, ns, room, locus_name)?;
                    let anchored =
                        grafeno_memory::anchor(tx, ns, memory, locus.id, args.note.as_deref())?;
                    Ok((locus, anchored))
                })
                .map_err(describe)?;
            json!({
                "anchor": anchored.edge,
                "anchored": anchored.created,
                "locus": locus.id,
                "locus_created": locus.created,
            })
        };
        let read = self.db.begin_read().map_err(describe)?;
        let places = grafeno_memory::anchors_of(&read, ns, memory).map_err(describe)?;
        // An unnamed memory by the start of its text, as the map shows it.
        let title = match read.node(ns, memory).map_err(describe)? {
            Some(node) if node.explicit_name().is_none() => {
                let text = read.text(ns, memory).map_err(describe)?.unwrap_or_default();
                if text.trim().is_empty() {
                    node.display_name()
                } else {
                    snippet(&text)
                }
            }
            _ => name_of(&read, ns, memory),
        };
        let mut output = json!({
            "memory": { "id": memory, "name": title },
            "places": places,
        });
        if let (Some(output), Some(result)) = (output.as_object_mut(), result.as_object()) {
            output.extend(result.clone());
        }
        Ok(ToolOutput::Json(output))
    }

    fn palace_map(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            palace: Option<String>,
            #[serde(default)]
            room: Option<String>,
            #[serde(default)]
            memories: Option<bool>,
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let read = self.db.begin_read().map_err(describe)?;
        let options = grafeno_memory::MapOptions {
            // A room is asked for to see what is in it.
            memories: args.memories.unwrap_or(args.room.is_some()),
            limit: args.limit.unwrap_or(20).clamp(1, 200),
        };
        if let Some(room) = &args.room {
            let room = self.room_ref(&read, ns, args.palace.as_deref(), room)?;
            let map = grafeno_memory::map_room(&read, ns, room, &options).map_err(describe)?;
            return Ok(ToolOutput::Json(json!({ "room": map })));
        }
        if let Some(palace) = &args.palace {
            let palace = self.palace_ref(&read, ns, palace)?;
            let map = grafeno_memory::map_palace(&read, ns, palace, &options).map_err(describe)?;
            return Ok(ToolOutput::Json(json!({ "palace": map })));
        }
        let palaces = grafeno_memory::palaces(&read, ns, &options).map_err(describe)?;
        let mut output = json!({ "palaces": palaces });
        if palaces.is_empty() {
            output["hint"] = json!("No palace in this namespace yet; palace_create makes one.");
        }
        Ok(ToolOutput::Json(output))
    }

    fn palace_walk(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            #[serde(default)]
            topic: Option<String>,
            #[serde(default)]
            from: Option<String>,
            #[serde(default)]
            palace: Option<String>,
            #[serde(default)]
            hops: Option<usize>,
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let read = self.db.begin_read().map_err(describe)?;
        let query = grafeno_memory::WalkQuery {
            topic: args.topic,
            from: args
                .from
                .as_deref()
                .map(parse_id)
                .transpose()
                .map_err(describe)?,
            palace: args
                .palace
                .as_deref()
                .map(|palace| self.palace_ref(&read, ns, palace))
                .transpose()?,
            hops: args.hops.unwrap_or(2),
            limit: args.limit.unwrap_or(20).clamp(1, 100),
        };
        let walk = grafeno_memory::walk(&read, ns, &query).map_err(describe)?;
        let mut output = json!({ "places": walk.places, "memories": walk.memories });
        if walk.memories.is_empty() {
            output["hint"] = json!(
                "Nothing in a palace matches. `recall` searches every memory, anchored or not."
            );
        }
        Ok(ToolOutput::Json(output))
    }

    fn palace_enter(&self, arguments: Value) -> ToolResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            room: String,
            #[serde(default)]
            palace: Option<String>,
            #[serde(default)]
            budget: Option<usize>,
            #[serde(default)]
            namespace: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        let ns = self.ns(&args.namespace);
        let read = self.db.begin_read().map_err(describe)?;
        let room = self.room_ref(&read, ns, args.palace.as_deref(), &args.room)?;
        let options = grafeno_memory::EnterOptions::with_budget(
            args.budget.unwrap_or(1_500).clamp(50, 20_000),
        );
        let entered = grafeno_memory::enter(&read, ns, room, &options).map_err(describe)?;
        drop(read);
        let ids: Vec<NodeId> = entered.context.items.iter().map(|item| item.node).collect();
        self.touch(ns, &ids)?;
        let memories: Vec<Value> = entered
            .context
            .items
            .iter()
            .map(|item| json!({ "id": item.node, "name": item.name, "truncated": item.truncated }))
            .collect();
        let left_out = entered.room.memories.saturating_sub(memories.len());
        let mut output = json!({
            "palace": entered.palace.map(|id| json!({ "id": id, "name": entered.palace_name })),
            "room": entered.room,
            "context": entered.context.rendered,
            "tokens": entered.context.tokens,
            "memories": memories,
            "left_out": left_out,
            "checkpoint": entered.checkpoint,
        });
        if entered.checkpoint.is_none() {
            output["hint"] = json!(
                "No checkpoint about this room. Before stopping, save one with `checkpoint` and \
                 `about` set to the room's id."
            );
        }
        Ok(ToolOutput::Json(output))
    }
}

/// Arguments shared by `recall` and `context`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryArgs {
    query: String,
    #[serde(default)]
    embedding: Option<Vec<f32>>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    depth: Option<usize>,
    #[serde(default)]
    budget: Option<usize>,
    #[serde(default)]
    include_archived: bool,
    /// A full-text index for the query, with its query language.
    #[serde(default)]
    index: Option<String>,
    #[serde(default)]
    mode: Option<grafeno::SearchMode>,
    #[serde(default)]
    namespace: Option<String>,
}

impl QueryArgs {
    fn query(&self, default_limit: usize) -> Result<RecallQuery, String> {
        if self.query.trim().is_empty() && self.embedding.is_none() {
            return Err("`query` must not be empty".into());
        }
        let depth = self.depth.unwrap_or(1);
        Ok(RecallQuery {
            text: Some(self.query.clone()),
            embedding: self.embedding.clone(),
            limit: self.limit.unwrap_or(default_limit),
            labels: self.labels.clone(),
            candidates: 50,
            expand: (depth > 0).then(|| Expansion {
                depth,
                ..Expansion::default()
            }),
            ranking: Some(Ranking::default()),
            include_archived: self.include_archived,
            index: self.index.clone(),
            vector_index: None,
            mode: self.mode,
            fusion: grafeno::Fusion::default(),
        })
    }
}

fn parse_args<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

fn parse_ids(ids: &[String]) -> Result<Vec<NodeId>, String> {
    ids.iter()
        .map(|id| parse_id(id).map_err(describe))
        .collect()
}

fn describe(error: Error) -> String {
    error.to_string()
}

fn name_of(read: &ReadTx, ns: &str, id: NodeId) -> String {
    read.node(ns, id)
        .ok()
        .flatten()
        .map(|node: Node| node.display_name())
        .unwrap_or_else(|| id.to_string())
}

fn snippet(text: &str) -> String {
    match text.char_indices().nth(SNIPPET_CHARS) {
        Some((end, _)) => format!("{}…", text[..end].trim_end()),
        None => text.to_owned(),
    }
}

fn round2(value: f32) -> f64 {
    (f64::from(value) * 100.0).round() / 100.0
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

impl McpServer {
    /// The tools this agent may call.
    fn tools(&self) -> Value {
        let all = tools();
        match all {
            Value::Array(items) if self.access.read_only => Value::Array(
                items
                    .into_iter()
                    .filter(|tool| {
                        !WRITING_TOOLS.contains(&tool["name"].as_str().unwrap_or_default())
                    })
                    .collect(),
            ),
            other => other,
        }
    }
}

fn tools() -> Value {
    let namespace = json!({
        "type": "string",
        "description": "Memory namespace; defaults to the server's namespace."
    });
    let date = json!({
        "type": "string",
        "description": "ISO 8601 date or date-time, such as 2024-05-01 or 2024-05-01T10:30:00Z."
    });
    let id = |description: &str| json!({ "type": "string", "description": description });
    let importance = json!({
        "type": "number", "minimum": 0, "maximum": 1,
        "description": "How much this matters, from 0 to 1 (default 0.5). Important memories rank \
            higher and fade slower."
    });
    let include_archived = json!({
        "type": "boolean",
        "description": "Also return archived memories (replaced by summaries). Default: false."
    });
    json!([
        {
            "name": "remember",
            "title": "Remember",
            "description": "Store a memory: a fact, decision, preference, person, project or document. \
                It becomes searchable by `recall` and `context`. Optionally link it to existing nodes. \
                With a `name` that already exists (ignoring case and accents, and with a label in \
                common when labels are given), the existing node is updated instead and `reused` is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "What to remember, in plain language." },
                    "name": { "type": "string", "description": "Short name, for entities such as a person or project." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "Kinds of node, such as Person, Project, Preference. Default: Memory." },
                    "props": { "type": "object", "description": "Extra structured properties." },
                    "sources": { "type": "array", "items": { "type": "string" }, "description": "Ids of the nodes this memory comes from." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional embedding of the text." },
                    "importance": importance.clone(),
                    "links": {
                        "type": "array",
                        "description": "Facts from the new node to existing nodes.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": { "type": "string", "description": "Relation, such as works_on or prefers." },
                                "to": id("Id of the target node."),
                                "valid_from": date,
                                "valid_to": date
                            },
                            "required": ["label", "to"]
                        }
                    },
                    "namespace": namespace
                },
                "required": ["text"]
            }
        },
        {
            "name": "recall",
            "title": "Recall",
            "description": "Search memory by meaning, words and connections. Returns ids, names, \
                snippets and why each result matched. Use it to find ids for `relate`, `update` or `forget`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What you are looking for." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Maximum results. Default: 10." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "Only nodes with one of these labels." },
                    "depth": { "type": "integer", "minimum": 0, "description": "Graph hops to expand from the best matches. Default: 1." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional embedding of the query." },
                    "index": { "type": "string", "description": "A full-text index to search the query in: then \"a phrase\", +required, -excluded, prefix*, fuzzy~1 and field:word work." },
                    "mode": { "type": "string", "enum": ["text", "vector", "hybrid"], "description": "Use the words, the embedding, or both. Default: what is given." },
                    "include_archived": include_archived.clone(),
                    "namespace": namespace
                },
                "required": ["query"]
            }
        },
        {
            "name": "context",
            "title": "Context",
            "description": "Get the memories and facts relevant to a question as compact text that fits \
                a token budget. Call it before answering questions that may depend on what you know.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The question or topic." },
                    "budget": { "type": "integer", "minimum": 50, "description": "Maximum tokens. Default: 1500." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "Only nodes with one of these labels." },
                    "depth": { "type": "integer", "minimum": 0, "description": "Graph hops to expand. Default: 1." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Maximum memories considered. Default: 20." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional embedding of the query." },
                    "include_archived": include_archived,
                    "namespace": namespace
                },
                "required": ["query"]
            }
        },
        {
            "name": "relate",
            "title": "Relate",
            "description": "Record a fact between two existing nodes, such as Ana works_on Grafeno, \
                optionally with the dates it holds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": id("Id of the source node."),
                    "label": { "type": "string", "description": "Relation, in snake_case, such as works_on." },
                    "to": id("Id of the target node."),
                    "valid_from": date,
                    "valid_to": date,
                    "props": { "type": "object", "description": "Extra properties of the fact." },
                    "sources": { "type": "array", "items": { "type": "string" }, "description": "Ids of the nodes this fact comes from." },
                    "namespace": namespace
                },
                "required": ["from", "label", "to"]
            }
        },
        {
            "name": "update",
            "title": "Update",
            "description": "Change a node: replace its text, rename it, merge properties (null removes one) \
                or change its labels.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id("Id of the node."),
                    "text": { "type": "string", "description": "New text; replaces the old one." },
                    "name": { "type": "string", "description": "New name." },
                    "props": { "type": "object", "description": "Properties to merge; a null is stored as a value." },
                    "remove_props": { "type": "array", "items": { "type": "string" }, "description": "Properties to remove." },
                    "add_labels": { "type": "array", "items": { "type": "string" } },
                    "remove_labels": { "type": "array", "items": { "type": "string" } },
                    "importance": importance,
                    "namespace": namespace
                },
                "required": ["id"]
            }
        },
        {
            "name": "forget",
            "title": "Forget",
            "description": "Forget a node or a fact. For facts, mode \"invalidate\" (default) means it \
                stopped being true at `at` (default now) and keeps its history; \"retract\" means it was \
                a mistake; \"delete\" erases it. Nodes are always deleted with their facts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id("Id of the node or fact."),
                    "mode": { "type": "string", "enum": ["invalidate", "retract", "delete"] },
                    "at": date,
                    "namespace": namespace
                },
                "required": ["id"]
            }
        },
        {
            "name": "get_node",
            "title": "Get node",
            "description": "Read a node: its text, properties, sources and the facts that connect it \
                to other nodes.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": id("Id of the node."), "namespace": namespace },
                "required": ["id"]
            }
        },
        {
            "name": "query",
            "title": "Query",
            "description": "Run a read-only GQ query (graph patterns) for structured questions, such as \
                MATCH (p:Person)-[:works_at]->(c) VALID AT '2024-01-01' RETURN p.name, c.name. \
                Pseudo-properties: n.@id, n.@text, n.@importance, e.@valid_from, e.@valid_to. \
                Also OPTIONAL MATCH, WITH (… WHERE for aggregates), UNWIND, UNION, CASE, list \
                comprehensions, pattern predicates in WHERE and CALL db.labels() / db.search(text); \
                SHOW NAMESPACES lists the namespaces. Returns at most 200 rows.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "One MATCH … RETURN statement." },
                    "params": { "type": "object", "description": "Values for $parameters." },
                    "namespace": namespace
                },
                "required": ["query"]
            }
        },
        {
            "name": "merge",
            "title": "Merge",
            "description": "Merge two nodes that are the same thing: `merge` is folded into `keep`. \
                Facts move to the kept node, labels and missing properties are joined, the merged \
                node's name becomes an alias, and the merged node is deleted. Refused when they \
                disagree (different emails or labels, two values of one property): check, then \
                retry with force to keep `keep`'s values. Returns a merge_id for undo_merge.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keep": id("Id of the node that stays."),
                    "merge": id("Id of the duplicate, which is deleted."),
                    "force": { "type": "boolean", "description": "Merge in spite of conflicts." },
                    "functional": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Fact labels a thing has one of at a time (born_in): different targets conflict."
                    },
                    "namespace": namespace
                },
                "required": ["keep", "merge"]
            }
        },
        {
            "name": "undo_merge",
            "title": "Undo merge",
            "description": "Undo a merge by its merge_id: the merged node and its facts come back as they were.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "merge_id": { "type": "string", "description": "The merge_id merge returned." },
                    "force": { "type": "boolean", "description": "Undo even if the kept node changed since (those changes are lost)." },
                    "namespace": namespace
                },
                "required": ["merge_id"]
            }
        },
        {
            "name": "overview",
            "title": "Overview",
            "description": "For broad questions (what is this memory about, what are the main topics or groups): \
                summaries of the communities of the graph that match a query, or the top ones without one. \
                Pass a community's id to open it: the smaller communities it is made of and its members.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The topic. Omit for the biggest communities." },
                    "id": id("A community to open."),
                    "level": { "type": "integer", "minimum": 0, "description": "Only this level (0 is the finest)." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Default: 10." },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "changes",
            "title": "Changes",
            "description": "What was written since you last looked: the change log after `since` (a sequence                 number; 0 for the start), oldest first, with `last` to pass next time. Each entry says which                 node or fact was created, updated or deleted; read it with get_node. `gap` means older                 changes were pruned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": { "type": "integer", "minimum": 0, "description": "The last sequence number seen. Default: 0." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Default: 100." },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "summarize_communities",
            "title": "Summarize communities",
            "description": "Find the communities of the graph (and communities of communities) and write or refresh \
                a summary of each: central members, distinctive terms, label mix, key facts and time span. \
                Only changed communities are rewritten.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "min_size": { "type": "integer", "minimum": 2, "description": "Smallest community summarized. Default: 3." },
                    "resolution": { "type": "number", "description": "Above 1 for smaller communities. Default: 1." },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "set_summary",
            "title": "Set summary",
            "description": "Replace a community summary's text with your own (refreshes keep it and mark it stale \
                when the community changes); omit text to go back to the generated summary.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id("The community summary."),
                    "text": { "type": "string", "description": "Your summary. Omit to undo." },
                    "namespace": namespace
                },
                "required": ["id"]
            }
        },
        {
            "name": "extract",
            "title": "Extract",
            "description": "Read the memories that are new or changed and link what they mention: known names, emails, \
                URLs, handles, hashtags, dates and amounts; new names seen in several memories become entities. \
                What cannot be decided waits in the review queue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": { "type": "boolean", "description": "Read every memory again." },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "add_mentions",
            "title": "Add mentions",
            "description": "Say which entities a memory mentions (from your own reading of it): each is linked to the \
                node it is (by the label's match rule, or by name), created when new, or queued when ambiguous.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id("The memory that mentions them."),
                    "mentions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "surface": { "type": "string", "description": "As written." },
                                "label": { "type": "string", "description": "Person, Company, Place, …" },
                                "props": { "type": "object", "description": "What you know of it (email, …)." }
                            },
                            "required": ["surface"]
                        }
                    },
                    "namespace": namespace
                },
                "required": ["id", "mentions"]
            }
        },
        {
            "name": "review",
            "title": "Review queue",
            "description": "Names extraction could not decide: ambiguous ones (several nodes have them) and new phrases.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer" },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "resolve_review",
            "title": "Resolve review",
            "description": "Settle a review item: link it to a node, create a new entity, or dismiss it. The decision is remembered.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The item's key from review." },
                    "action": { "type": "string", "enum": ["link", "create", "dismiss"] },
                    "node": id("For link: the node it is."),
                    "label": { "type": "string", "description": "For create: the new entity's label." },
                    "namespace": namespace
                },
                "required": ["key", "action"]
            }
        },
        {
            "name": "find_matches",
            "title": "Find matches",
            "description": "Before creating an entity, find the nodes that may already be it, by the label's match rule                 (strong keys, fields compared by similarity, thresholds): each with a score, a decision                 (same, review, different) and every field's similarity.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "The label whose match rule applies (Person, Company, …)." },
                    "props": { "type": "object", "description": "What is known of the entity: name, email, birth date, …" },
                    "limit": { "type": "integer", "description": "Candidates to return (5 by default)." },
                    "namespace": namespace
                },
                "required": ["label", "props"]
            }
        },
        {
            "name": "maintain",
            "title": "Maintain",
            "description": "Review memory health: the namespace's size, its vector graphs' wear \
                and the contradictions on record; likely duplicates (by name or meaning), weak \
                memories that are fading, and groups of older memories about one entity that \
                could be summarized with `consolidate`. Changes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "description": "Maximum items per section. Default: 10." },
                    "check": { "type": "boolean", "description": "Also check every record and index for integrity (reads them all). Default: false." },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "consolidate",
            "title": "Consolidate",
            "description": "Replace several memories with one summary you wrote. The summary links \
                to its sources and to the entity it is about, and the sources are archived (kept, \
                but left out of recall).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "description": "The summary text." },
                    "sources": { "type": "array", "items": { "type": "string" }, "description": "Ids of the memories summarized." },
                    "about": id("Id of the entity the memories are about."),
                    "name": { "type": "string", "description": "Optional name for the summary." },
                    "embedding": { "type": "array", "items": { "type": "number" }, "description": "Optional embedding of the summary." },
                    "archive_sources": { "type": "boolean", "description": "Archive the sources. Default: true." },
                    "namespace": namespace
                },
                "required": ["summary", "sources"]
            }
        },
        {
            "name": "checkpoint",
            "title": "Checkpoint",
            "description": "Save where a task stands so that you, or another agent, can take it up \
                in a later session with `resume`: what the task is, a summary, what is done, what \
                comes next, decisions taken and files touched. Checkpoints of one task form a \
                chain; the newest is what `resume` returns.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "What the work is, in the same words each time (such as \"migrate billing to events\")." },
                    "summary": { "type": "string", "description": "Where it stands, in a few sentences." },
                    "done": { "type": "array", "items": { "type": "string" }, "description": "What is finished." },
                    "next": { "type": "array", "items": { "type": "string" }, "description": "What comes next, in order." },
                    "decisions": { "type": "array", "items": { "type": "string" }, "description": "Decisions taken, and why." },
                    "files": { "type": "array", "items": { "type": "string" }, "description": "Files or documents touched." },
                    "about": id("Id of the project or node the task is about."),
                    "agent": { "type": "string", "description": "Who saves it. Default: the name this server records writes as." },
                    "namespace": namespace
                },
                "required": ["task", "summary"]
            }
        },
        {
            "name": "resume",
            "title": "Resume",
            "description": "Where a task stands: its latest checkpoint (summary, done, next, \
                decisions, files), how many came before, and what was written in memory since. \
                With no match, lists the tasks that have checkpoints.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The task's words. Omit for the latest checkpoint of any task." },
                    "about": id("Only checkpoints about this node."),
                    "agent": { "type": "string", "description": "Only checkpoints this agent saved." },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "ingest",
            "title": "Ingest",
            "description": "Store a long text as memories: a document cut at its headings and \
                paragraphs, a conversation cut between turns, or code cut at its definitions. \
                The whole becomes one node and each chunk a memory linked to it, in order, with \
                its lines; then the chunks are read for the entities they mention. Ingesting the \
                same source again replaces it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The whole text." },
                    "kind": { "type": "string", "enum": ["document", "conversation", "code"], "description": "How to cut it. Default: document." },
                    "title": { "type": "string", "description": "Its name. Default: the source, or its first line." },
                    "source": { "type": "string", "description": "Where it comes from (a path, a URL); the same source again replaces it." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "More labels for the whole." },
                    "max_chars": { "type": "integer", "minimum": 100, "description": "Characters per chunk at most. Default: 1500." },
                    "extract": { "type": "boolean", "description": "Link the entities the chunks mention. Default: true." },
                    "namespace": namespace
                },
                "required": ["content"]
            }
        },
        {
            "name": "contradictions",
            "title": "Contradictions",
            "description": "Facts that contradicted earlier ones under a constraint (one employer at \
                a time, one birthplace), newest first, and what was done: the earlier fact ended, \
                the new one refused, or both kept and flagged.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node": id("Only contradictions that involve this node."),
                    "limit": { "type": "integer", "minimum": 1, "description": "Default: 20." },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "timeline",
            "title": "Timeline",
            "description": "How a node changed over time, oldest first: when it was recorded, when \
                each of its facts began and ended, the values its properties had before, the \
                merges it took part in and the contradictions its facts ran into.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id("The node: a project, a decision, a person."),
                    "limit": { "type": "integer", "minimum": 1, "description": "The latest moments kept. Default: 100." },
                    "namespace": namespace
                },
                "required": ["id"]
            }
        },
        {
            "name": "feedback",
            "title": "Feedback",
            "description": "Say how a memory or fact turned out. \"useful\": a memory gains \
                importance. \"obsolete\": a memory is archived, a fact ends now. \"wrong\": a \
                memory is archived with no importance, a fact is retracted as a mistake. Nothing \
                is deleted; the verdict is kept on the memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id("The memory or fact."),
                    "verdict": { "type": "string", "enum": ["useful", "wrong", "obsolete"] },
                    "note": { "type": "string", "description": "Why, in a few words." },
                    "namespace": namespace
                },
                "required": ["id", "verdict"]
            }
        },
        {
            "name": "palace_create",
            "title": "Create a palace",
            "description": "A mental palace arranges memories into places so they are found again \
                by where they are: a palace has rooms (one per project, domain or subject), a room \
                has loci (places inside it), and memories are anchored at loci. This makes a \
                palace, or returns the one of that name. It stores no memory by itself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The palace's name, unique in the namespace (such as \"Work\")." },
                    "description": { "type": "string", "description": "What it holds." },
                    "namespace": namespace
                },
                "required": ["name"]
            }
        },
        {
            "name": "palace_create_room",
            "title": "Create a room",
            "description": "Make a room in a palace, or return the one of that name there. A room \
                is created with a locus named \"entrance\"; `loci` adds more, and names given \
                again for a room that exists add the ones it lacks.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "palace": { "type": "string", "description": "The palace: its name or its id." },
                    "name": { "type": "string", "description": "The room's name, unique in its palace." },
                    "topic": { "type": "string", "description": "What the room is about, in the words you would search it by." },
                    "description": { "type": "string" },
                    "loci": { "type": "array", "items": { "type": "string" }, "description": "Places inside the room, in order (such as \"Decisions\", \"Open questions\")." },
                    "namespace": namespace
                },
                "required": ["palace", "name"]
            }
        },
        {
            "name": "palace_anchor",
            "title": "Anchor a memory",
            "description": "Put a memory that already exists at a place in a room. The memory is \
                not copied or changed and keeps its own relations; it can be anchored at several \
                places. A locus that does not exist yet is made; a room is not. With `remove`, \
                takes the anchor away and leaves the memory. Returns every place the memory is at.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory": id("Id of the memory (from `remember` or `recall`)."),
                    "room": { "type": "string", "description": "The room: its name or its id." },
                    "locus": { "type": "string", "description": "The place inside the room. Default: \"entrance\"." },
                    "palace": { "type": "string", "description": "The room's palace (name or id), needed when two palaces have a room of that name." },
                    "note": { "type": "string", "description": "Why it belongs there, in a few words." },
                    "remove": { "type": "boolean", "description": "Take the anchor away instead. Default: false." },
                    "namespace": namespace
                },
                "required": ["memory", "room"]
            }
        },
        {
            "name": "palace_map",
            "title": "Map of the palaces",
            "description": "What is where. With no arguments: the palaces of the namespace, their \
                rooms and loci, and how many memories each holds. With `palace`: that palace. \
                With `room`: that room and the memories at each of its loci.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "palace": { "type": "string", "description": "A palace: its name or its id." },
                    "room": { "type": "string", "description": "A room: its name or its id." },
                    "memories": { "type": "boolean", "description": "List the memories at each locus. Default: true for a room, false otherwise." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Memories listed per locus, newest first. Default: 20." },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "palace_walk",
            "title": "Walk the palace",
            "description": "From a topic, or from a node, to the memories around it. Finds the \
                rooms and loci the topic names and the anchored memories that match it, then \
                follows the graph's relations from those memories. Each memory comes with how \
                many relations away it is, the way there, and where it is anchored. Memories that \
                are in no palace are not returned: use `recall` for those.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": { "type": "string", "description": "What to look for." },
                    "from": id("Id of a palace, room, locus or memory to start from, instead of or besides a topic."),
                    "palace": { "type": "string", "description": "Stay in this palace (name or id). Default: every palace of the namespace." },
                    "hops": { "type": "integer", "minimum": 0, "maximum": 3, "description": "Relations followed from the memories found. Default: 2." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Memories returned, the nearest first. Default: 20." },
                    "namespace": namespace
                }
            }
        },
        {
            "name": "palace_enter",
            "title": "Enter a room",
            "description": "Rebuild the context of a room at the start of a session: its loci, \
                its memories (the strongest first) and the facts that connect them, as text \
                within a token budget, and the latest checkpoint saved about the room.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "room": { "type": "string", "description": "The room: its name or its id." },
                    "palace": { "type": "string", "description": "The room's palace (name or id), needed when two palaces have a room of that name." },
                    "budget": { "type": "integer", "minimum": 50, "description": "Tokens for the memories and facts. Default: 1500." },
                    "namespace": namespace
                },
                "required": ["room"]
            }
        }
    ])
}

const QUERY_ROWS: usize = 200;
