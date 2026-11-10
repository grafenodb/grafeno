//! CSV and GraphML: a namespace's graph in the formats other tools read
//! and write: spreadsheets, graph drawing and analysis programs, and other
//! databases.
//!
//! Exports hold current knowledge: retracted facts are left out; a fact's
//! valid dates are columns (`valid_from`, `valid_to`). Imports keep ids that
//! are Grafeno ids (a re-import updates the same nodes); other ids are the
//! source's, mapped to new nodes and remembered per namespace (state kind
//! `import.id`), so the edges of a later file find them.
//!
//! CSV, two files whose headers carry each column's role and type:
//!
//! ```text
//! id:ID,:LABEL,name:string,age:int,:TEXT
//! 0190…,Person;Admin,Ana,34,"Ana writes ""graph"" notes"
//!
//! id,:START_ID,:END_ID,:TYPE,valid_from,valid_to,since:int
//! 0191…,0190…,0192…,works_at,2024-01-01T00:00:00Z,,2024
//! ```
//!
//! A column's type is `string`, `int`, `float`, `boolean` or `json` (lists
//! and maps); without one, an import reads integers, decimals and
//! `true`/`false` as such. Empty cells are absent properties.
//!
//! GraphML: `<key>`s for properties (with `attr.type` `string`, `long`,
//! `double`, `boolean`), a node's labels in the `labels` key (`:A:B`), an edge's label in `label` (or its `label`
//! attribute), text in `text`.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};

use grafeno_core::{Edge, EdgeId, Error, NewEdge, NewNode, NodeId, Props, Result, Timestamp};
use serde::{Deserialize, Serialize};

use crate::db::{Grafeno, ReadTx, WriteTx};

/// State kind of the source ids an import mapped to nodes.
pub const IMPORT_ID: &str = "import.id";

/// Records per transaction of an import.
const CHUNK: usize = 2_000;
/// Records per page of an export.
const PAGE: usize = 2_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterchangeStats {
    pub nodes: u64,
    pub edges: u64,
    /// Rows or elements that could not be read, with why (the first 100).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(default)]
    pub failed: u64,
}

impl InterchangeStats {
    fn fail(&mut self, error: String) {
        self.failed += 1;
        if self.errors.len() < 100 {
            self.errors.push(error);
        }
    }
}

/// How an import reads what the file leaves open.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InterchangeOptions {
    /// The label of nodes without one.
    pub default_label: String,
    /// The label of edges without one.
    pub default_edge_label: String,
}

impl Default for InterchangeOptions {
    fn default() -> Self {
        Self {
            default_label: "Node".into(),
            default_edge_label: "RELATED_TO".into(),
        }
    }
}

// ---------------------------------------------------------------- types

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Int,
    Float,
    Bool,
    Str,
    Json,
}

impl Kind {
    fn of(value: &serde_json::Value) -> Kind {
        match value {
            serde_json::Value::Number(n) if n.is_i64() => Kind::Int,
            serde_json::Value::Number(_) => Kind::Float,
            serde_json::Value::Bool(_) => Kind::Bool,
            serde_json::Value::String(_) => Kind::Str,
            _ => Kind::Json,
        }
    }

    /// The kind both values fit.
    fn join(self, other: Kind) -> Kind {
        match (self, other) {
            (a, b) if a == b => a,
            (Kind::Int, Kind::Float) | (Kind::Float, Kind::Int) => Kind::Float,
            _ => Kind::Json,
        }
    }

    fn csv_name(self) -> &'static str {
        match self {
            Kind::Int => "int",
            Kind::Float => "float",
            Kind::Bool => "boolean",
            Kind::Str => "string",
            Kind::Json => "json",
        }
    }

    fn graphml_name(self) -> &'static str {
        match self {
            Kind::Int => "long",
            Kind::Float => "double",
            Kind::Bool => "boolean",
            Kind::Str | Kind::Json => "string",
        }
    }

    fn from_name(name: &str) -> Option<Kind> {
        Some(match name.to_ascii_lowercase().as_str() {
            "int" | "long" | "integer" | "short" | "byte" => Kind::Int,
            "float" | "double" => Kind::Float,
            "boolean" | "bool" => Kind::Bool,
            "string" | "char" => Kind::Str,
            "json" => Kind::Json,
            _ => return None,
        })
    }
}

/// A cell as written: strings as they are, other values as JSON.
fn cell(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A cell as read, by its column's kind (`None`: guess).
fn parse_cell(text: &str, kind: Option<Kind>) -> std::result::Result<serde_json::Value, String> {
    let guess = || -> serde_json::Value {
        if let Ok(i) = text.parse::<i64>() {
            return i.into();
        }
        if let Ok(f) = text.parse::<f64>()
            && f.is_finite()
        {
            return f.into();
        }
        match text {
            "true" | "TRUE" | "True" => true.into(),
            "false" | "FALSE" | "False" => false.into(),
            _ => text.into(),
        }
    };
    Ok(match kind {
        None => guess(),
        Some(Kind::Str) => text.into(),
        Some(Kind::Int) => text
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("{text:?} is not an integer"))?
            .into(),
        Some(Kind::Float) => text
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("{text:?} is not a number"))?
            .into(),
        Some(Kind::Bool) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => true.into(),
            "false" => false.into(),
            _ => return Err(format!("{text:?} is not true or false")),
        },
        Some(Kind::Json) => serde_json::from_str(text).map_err(|e| format!("{text:?}: {e}"))?,
    })
}

fn kinds<'a>(props: impl Iterator<Item = &'a Props>) -> BTreeMap<String, Kind> {
    let mut out: BTreeMap<String, Kind> = BTreeMap::new();
    for props in props {
        for (key, value) in props {
            if value.is_null() {
                continue;
            }
            let kind = Kind::of(value);
            out.entry(key.clone())
                .and_modify(|k| *k = k.join(kind))
                .or_insert(kind);
        }
    }
    out
}

// ---------------------------------------------------------------- CSV

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) || text.starts_with(' ') || text.ends_with(' ') {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

fn csv_line(out: &mut impl Write, fields: &[String]) -> Result<()> {
    let line: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
    writeln!(out, "{}", line.join(",")).map_err(io)
}

fn io(error: std::io::Error) -> Error {
    Error::Storage(format!("cannot write: {error}"))
}

/// Reads CSV records (RFC 4180: quoted fields may hold commas, quotes as
/// `""` and line breaks).
pub(crate) fn csv_records(mut input: impl BufRead) -> Result<Vec<Vec<String>>> {
    let mut text = String::new();
    input
        .read_to_string(&mut text)
        .map_err(|e| Error::Invalid(format!("cannot read the CSV: {e}")))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        any = true;
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                other => field.push(other),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            ',' => record.push(std::mem::take(&mut field)),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                record.push(std::mem::take(&mut field));
                if !(record.len() == 1 && record[0].is_empty()) {
                    records.push(std::mem::take(&mut record));
                } else {
                    record.clear();
                }
                any = false;
            }
            other => field.push(other),
        }
    }
    if quoted {
        return Err(Error::Invalid("the CSV ends inside a quoted field".into()));
    }
    if any || !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    Ok(records)
}

/// A header cell: `name:type`, or a role such as `:ID`, `:LABEL`.
fn header(cell: &str) -> (String, Option<String>) {
    match cell.rsplit_once(':') {
        Some((name, kind)) => (name.trim().to_owned(), Some(kind.trim().to_owned())),
        None => (cell.trim().to_owned(), None),
    }
}

impl ReadTx {
    /// The namespace's nodes as CSV (`id:ID,:LABEL,…,:TEXT`).
    pub fn export_nodes_csv(&self, ns: &str, out: &mut impl Write) -> Result<u64> {
        let kinds = kinds_of_nodes(self, ns)?;
        let mut head = vec!["id:ID".to_owned(), ":LABEL".to_owned()];
        head.extend(
            kinds
                .iter()
                .map(|(k, kind)| format!("{k}:{}", kind.csv_name())),
        );
        head.push(":TEXT".into());
        csv_line(out, &head)?;
        let mut count = 0;
        let mut after = None;
        loop {
            let page = self.nodes_page(ns, None, after, PAGE)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for node in &page {
                let mut row = vec![node.id.to_string(), node.labels.join(";")];
                for key in kinds.keys() {
                    row.push(
                        node.props
                            .get(key)
                            .filter(|v| !v.is_null())
                            .map(cell)
                            .unwrap_or_default(),
                    );
                }
                row.push(self.text(ns, node.id)?.unwrap_or_default());
                csv_line(out, &row)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// The namespace's current facts as CSV (`id,:START_ID,:END_ID,:TYPE,
    /// valid_from,valid_to,…`).
    pub fn export_edges_csv(&self, ns: &str, out: &mut impl Write) -> Result<u64> {
        let kinds = kinds_of_edges(self, ns)?;
        let mut head: Vec<String> = [
            "id",
            ":START_ID",
            ":END_ID",
            ":TYPE",
            "valid_from",
            "valid_to",
        ]
        .map(String::from)
        .to_vec();
        head.extend(
            kinds
                .iter()
                .map(|(k, kind)| format!("{k}:{}", kind.csv_name())),
        );
        csv_line(out, &head)?;
        let mut count = 0;
        for_each_edge(self, ns, |edge| {
            let mut row = vec![
                edge.id.to_string(),
                edge.from.to_string(),
                edge.to.to_string(),
                edge.label.clone(),
                edge.valid_from.map(Timestamp::iso).unwrap_or_default(),
                edge.valid_to.map(Timestamp::iso).unwrap_or_default(),
            ];
            for key in kinds.keys() {
                row.push(
                    edge.props
                        .get(key)
                        .filter(|v| !v.is_null())
                        .map(cell)
                        .unwrap_or_default(),
                );
            }
            count += 1;
            csv_line(out, &row)
        })?;
        Ok(count)
    }

    /// The namespace's nodes and current facts as GraphML.
    pub fn export_graphml(&self, ns: &str, out: &mut impl Write) -> Result<(u64, u64)> {
        let node_kinds = kinds_of_nodes(self, ns)?;
        let edge_kinds = kinds_of_edges(self, ns)?;
        let w = |out: &mut dyn Write, text: &str| out.write_all(text.as_bytes()).map_err(io);
        w(out, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n")?;
        w(
            out,
            "<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">\n",
        )?;
        w(
            out,
            "  <key id=\"labels\" for=\"node\" attr.name=\"labels\" attr.type=\"string\"/>\n",
        )?;
        w(
            out,
            "  <key id=\"text\" for=\"node\" attr.name=\"text\" attr.type=\"string\"/>\n",
        )?;
        w(
            out,
            "  <key id=\"label\" for=\"edge\" attr.name=\"label\" attr.type=\"string\"/>\n",
        )?;
        w(
            out,
            "  <key id=\"valid_from\" for=\"edge\" attr.name=\"valid_from\" attr.type=\"string\"/>\n",
        )?;
        w(
            out,
            "  <key id=\"valid_to\" for=\"edge\" attr.name=\"valid_to\" attr.type=\"string\"/>\n",
        )?;
        for (key, kind) in &node_kinds {
            w(
                out,
                &format!(
                    "  <key id=\"n_{}\" for=\"node\" attr.name=\"{}\" attr.type=\"{}\"/>\n",
                    escape(key),
                    escape(key),
                    kind.graphml_name()
                ),
            )?;
        }
        for (key, kind) in &edge_kinds {
            w(
                out,
                &format!(
                    "  <key id=\"e_{}\" for=\"edge\" attr.name=\"{}\" attr.type=\"{}\"/>\n",
                    escape(key),
                    escape(key),
                    kind.graphml_name()
                ),
            )?;
        }
        w(
            out,
            &format!("  <graph id=\"{}\" edgedefault=\"directed\">\n", escape(ns)),
        )?;
        let data = |key: &str, value: &str| {
            format!("<data key=\"{}\">{}</data>", escape(key), escape(value))
        };
        let (mut nodes, mut edges) = (0, 0);
        let mut after = None;
        loop {
            let page = self.nodes_page(ns, None, after, PAGE)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for node in &page {
                let mut line = format!("    <node id=\"{}\">", node.id);
                line.push_str(&data("labels", &format!(":{}", node.labels.join(":"))));
                for (key, value) in &node.props {
                    if !value.is_null() {
                        line.push_str(&data(&format!("n_{key}"), &cell(value)));
                    }
                }
                if let Some(text) = self.text(ns, node.id)? {
                    line.push_str(&data("text", &text));
                }
                line.push_str("</node>\n");
                w(out, &line)?;
                nodes += 1;
            }
        }
        for_each_edge(self, ns, |edge| {
            let mut line = format!(
                "    <edge id=\"{}\" source=\"{}\" target=\"{}\" label=\"{}\">",
                edge.id,
                edge.from,
                edge.to,
                escape(&edge.label)
            );
            line.push_str(&data("label", &edge.label));
            if let Some(at) = edge.valid_from {
                line.push_str(&data("valid_from", &at.iso()));
            }
            if let Some(at) = edge.valid_to {
                line.push_str(&data("valid_to", &at.iso()));
            }
            for (key, value) in &edge.props {
                if !value.is_null() {
                    line.push_str(&data(&format!("e_{key}"), &cell(value)));
                }
            }
            line.push_str("</edge>\n");
            edges += 1;
            w(out, &line)
        })?;
        w(out, "  </graph>\n</graphml>\n")?;
        Ok((nodes, edges))
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Characters XML 1.0 cannot hold at all.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

fn kinds_of_nodes(read: &ReadTx, ns: &str) -> Result<BTreeMap<String, Kind>> {
    let mut all: BTreeMap<String, Kind> = BTreeMap::new();
    let mut after = None;
    loop {
        let page = read.nodes_page(ns, None, after, PAGE)?;
        let Some(last) = page.last() else { break };
        after = Some(last.id);
        for (key, kind) in kinds(page.iter().map(|n| &n.props)) {
            all.entry(key)
                .and_modify(|k| *k = k.join(kind))
                .or_insert(kind);
        }
    }
    Ok(all)
}

fn kinds_of_edges(read: &ReadTx, ns: &str) -> Result<BTreeMap<String, Kind>> {
    let mut all: BTreeMap<String, Kind> = BTreeMap::new();
    for_each_edge(read, ns, |edge| {
        for (key, kind) in kinds(std::iter::once(&edge.props)) {
            all.entry(key)
                .and_modify(|k| *k = k.join(kind))
                .or_insert(kind);
        }
        Ok(())
    })?;
    Ok(all)
}

/// Current (not retracted) edges, a page at a time.
fn for_each_edge(read: &ReadTx, ns: &str, mut f: impl FnMut(&Edge) -> Result<()>) -> Result<()> {
    let mut after: Option<EdgeId> = None;
    loop {
        let page = read.edges_page(ns, None, after, PAGE)?;
        let Some(last) = page.last() else { break };
        after = Some(last.id);
        for edge in page.iter().filter(|e| e.expired_at.is_none()) {
            f(edge)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- imports

/// A node to write: its source id, labels, properties and text.
struct InNode {
    source: String,
    labels: Vec<String>,
    props: Props,
    text: Option<String>,
}

struct InEdge {
    source: Option<String>,
    from: String,
    to: String,
    label: String,
    props: Props,
    valid_from: Option<Timestamp>,
    valid_to: Option<Timestamp>,
}

fn grafeno_id(text: &str) -> Option<NodeId> {
    text.parse::<uuid_like::Uuid>()
        .ok()
        .map(|u| NodeId::from_u128(u.0))
}

/// Parses a UUID without a dependency on the `uuid` crate here.
mod uuid_like {
    pub struct Uuid(pub u128);

    impl std::str::FromStr for Uuid {
        type Err = ();
        fn from_str(text: &str) -> Result<Self, ()> {
            let hex: String = text.chars().filter(|c| *c != '-').collect();
            if hex.len() != 32 || text.len() != 36 {
                return Err(());
            }
            u128::from_str_radix(&hex, 16).map(Uuid).map_err(|_| ())
        }
    }
}

fn write_nodes(
    db: &Grafeno,
    ns: &str,
    nodes: Vec<InNode>,
    stats: &mut InterchangeStats,
) -> Result<()> {
    for chunk in nodes.chunks(CHUNK) {
        let written = db.write(|tx| {
            let mut written = 0;
            for node in chunk {
                let new = NewNode {
                    labels: node.labels.clone(),
                    props: node.props.clone(),
                    text: node.text.clone(),
                    ..NewNode::default()
                };
                match grafeno_id(&node.source) {
                    Some(id) => {
                        tx.upsert_node(ns, id, new)?;
                    }
                    None => {
                        let id = match mapped(tx, ns, &node.source)? {
                            Some(id) => {
                                tx.upsert_node(ns, id, new)?;
                                id
                            }
                            None => tx.create_node(ns, new)?,
                        };
                        if !node.source.is_empty() {
                            tx.put_state(ns, IMPORT_ID, &node.source, &id.as_u128().to_le_bytes())?;
                        }
                    }
                }
                written += 1;
            }
            Ok(written)
        })?;
        stats.nodes += written;
    }
    Ok(())
}

fn mapped(tx: &WriteTx, ns: &str, source: &str) -> Result<Option<NodeId>> {
    if source.is_empty() {
        return Ok(None);
    }
    Ok(tx
        .state(ns, IMPORT_ID, source)?
        .and_then(|bytes| <[u8; 16]>::try_from(bytes.as_slice()).ok())
        .map(|bytes| NodeId::from_u128(u128::from_le_bytes(bytes))))
}

fn write_edges(
    db: &Grafeno,
    ns: &str,
    edges: Vec<InEdge>,
    stats: &mut InterchangeStats,
) -> Result<()> {
    for chunk in edges.chunks(CHUNK) {
        let (written, errors) = db.write(|tx| {
            let mut written = 0;
            let mut errors = Vec::new();
            for edge in chunk {
                let end = |tx: &WriteTx, text: &str| -> Result<Option<NodeId>> {
                    match grafeno_id(text) {
                        Some(id) if tx.node(ns, id)?.is_some() => Ok(Some(id)),
                        _ => mapped(tx, ns, text),
                    }
                };
                let (Some(from), Some(to)) = (end(tx, &edge.from)?, end(tx, &edge.to)?) else {
                    errors.push(format!(
                        "edge {}: no node {:?} or {:?} in the namespace",
                        edge.source.as_deref().unwrap_or("?"),
                        edge.from,
                        edge.to
                    ));
                    continue;
                };
                let new = NewEdge {
                    from,
                    label: edge.label.clone(),
                    to,
                    props: edge.props.clone(),
                    sources: Vec::new(),
                    valid_from: edge.valid_from,
                    valid_to: edge.valid_to,
                };
                match edge
                    .source
                    .as_deref()
                    .and_then(|s| grafeno_id(s).map(|id| EdgeId::from_u128(id.as_u128())))
                {
                    // A Grafeno id: written as that edge (a re-import
                    // replaces it rather than adding a copy).
                    Some(id) => {
                        let now = Timestamp::now();
                        tx.restore_edge(
                            ns,
                            Edge {
                                id,
                                label: new.label,
                                from,
                                to,
                                props: new.props,
                                sources: Vec::new(),
                                valid_from: new.valid_from,
                                valid_to: new.valid_to,
                                recorded_at: now,
                                expired_at: None,
                                recorded_by: tx.actor().map(|a| a.name.clone()),
                            },
                        )?;
                    }
                    None => {
                        tx.create_edge(ns, new)?;
                    }
                }
                written += 1;
            }
            Ok((written, errors))
        })?;
        stats.edges += written;
        for error in errors {
            stats.fail(error);
        }
    }
    Ok(())
}

fn time_cell(text: &str) -> std::result::Result<Option<Timestamp>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    Timestamp::parse(text.trim())
        .map(Some)
        .map_err(|e| format!("{text:?}: {e}"))
}

impl Grafeno {
    /// Imports nodes from CSV (see the module). Returns what it wrote.
    pub fn import_nodes_csv(
        &self,
        ns: &str,
        input: impl BufRead,
        options: &InterchangeOptions,
    ) -> Result<InterchangeStats> {
        let records = csv_records(input)?;
        let mut stats = InterchangeStats::default();
        let Some((head, rows)) = records.split_first() else {
            return Ok(stats);
        };
        enum Role {
            Id,
            Labels,
            Text,
            Prop(String, Option<Kind>),
            Ignore,
        }
        let roles: Vec<Role> = head
            .iter()
            .map(|cell| match header(cell) {
                (_, Some(role)) if role.eq_ignore_ascii_case("ID") => Role::Id,
                (_, Some(role)) if role.eq_ignore_ascii_case("LABEL") => Role::Labels,
                (_, Some(role)) if role.eq_ignore_ascii_case("TEXT") => Role::Text,
                (name, _) if name.eq_ignore_ascii_case("id") => Role::Id,
                (name, _) if name.is_empty() => Role::Ignore,
                (name, kind) => Role::Prop(name, kind.as_deref().and_then(Kind::from_name)),
            })
            .collect();
        let mut nodes = Vec::with_capacity(rows.len());
        for (number, row) in rows.iter().enumerate() {
            let mut node = InNode {
                source: String::new(),
                labels: Vec::new(),
                props: Props::new(),
                text: None,
            };
            let mut failed = None;
            for (role, value) in roles.iter().zip(row) {
                match role {
                    Role::Id => node.source = value.trim().to_owned(),
                    Role::Labels => {
                        node.labels = value
                            .split([';', ':'])
                            .map(str::trim)
                            .filter(|l| !l.is_empty())
                            .map(str::to_owned)
                            .collect()
                    }
                    Role::Text if !value.is_empty() => node.text = Some(value.clone()),
                    Role::Prop(name, kind) if !value.is_empty() => match parse_cell(value, *kind) {
                        Ok(value) => {
                            node.props.insert(name.clone(), value);
                        }
                        Err(error) => failed = Some(format!("row {}: {name}: {error}", number + 2)),
                    },
                    _ => {}
                }
            }
            if let Some(error) = failed {
                stats.fail(error);
                continue;
            }
            if node.labels.is_empty() {
                node.labels.push(options.default_label.clone());
            }
            nodes.push(node);
        }
        write_nodes(self, ns, nodes, &mut stats)?;
        Ok(stats)
    }

    /// Imports edges from CSV (see the module): their ends are Grafeno ids
    /// or ids of nodes imported before.
    pub fn import_edges_csv(
        &self,
        ns: &str,
        input: impl BufRead,
        options: &InterchangeOptions,
    ) -> Result<InterchangeStats> {
        let records = csv_records(input)?;
        let mut stats = InterchangeStats::default();
        let Some((head, rows)) = records.split_first() else {
            return Ok(stats);
        };
        enum Role {
            Id,
            Start,
            End,
            Type,
            From,
            To,
            Prop(String, Option<Kind>),
            Ignore,
        }
        let roles: Vec<Role> = head
            .iter()
            .map(|cell| match header(cell) {
                (_, Some(r)) if r.eq_ignore_ascii_case("START_ID") => Role::Start,
                (_, Some(r)) if r.eq_ignore_ascii_case("END_ID") => Role::End,
                (_, Some(r)) if r.eq_ignore_ascii_case("TYPE") => Role::Type,
                (_, Some(r)) if r.eq_ignore_ascii_case("ID") => Role::Id,
                (n, None) if n.eq_ignore_ascii_case("id") => Role::Id,
                (n, None)
                    if ["source", "from", "start"].contains(&n.to_ascii_lowercase().as_str()) =>
                {
                    Role::Start
                }
                (n, None) if ["target", "to", "end"].contains(&n.to_ascii_lowercase().as_str()) => {
                    Role::End
                }
                (n, None) if ["type", "label"].contains(&n.to_ascii_lowercase().as_str()) => {
                    Role::Type
                }
                (n, _) if n == "valid_from" => Role::From,
                (n, _) if n == "valid_to" => Role::To,
                (n, _) if n.is_empty() => Role::Ignore,
                (n, kind) => Role::Prop(n, kind.as_deref().and_then(Kind::from_name)),
            })
            .collect();
        let mut edges = Vec::with_capacity(rows.len());
        'rows: for (number, row) in rows.iter().enumerate() {
            let mut edge = InEdge {
                source: None,
                from: String::new(),
                to: String::new(),
                label: options.default_edge_label.clone(),
                props: Props::new(),
                valid_from: None,
                valid_to: None,
            };
            for (role, value) in roles.iter().zip(row) {
                let at = || format!("row {}", number + 2);
                match role {
                    Role::Id if !value.is_empty() => edge.source = Some(value.trim().to_owned()),
                    Role::Start => edge.from = value.trim().to_owned(),
                    Role::End => edge.to = value.trim().to_owned(),
                    Role::Type if !value.trim().is_empty() => edge.label = value.trim().to_owned(),
                    Role::From | Role::To => match time_cell(value) {
                        Ok(time) => {
                            if matches!(role, Role::From) {
                                edge.valid_from = time;
                            } else {
                                edge.valid_to = time;
                            }
                        }
                        Err(error) => {
                            stats.fail(format!("{}: {error}", at()));
                            continue 'rows;
                        }
                    },
                    Role::Prop(name, kind) if !value.is_empty() => match parse_cell(value, *kind) {
                        Ok(value) => {
                            edge.props.insert(name.clone(), value);
                        }
                        Err(error) => {
                            stats.fail(format!("{}: {name}: {error}", at()));
                            continue 'rows;
                        }
                    },
                    _ => {}
                }
            }
            if edge.from.is_empty() || edge.to.is_empty() {
                stats.fail(format!("row {}: an edge needs both ends", number + 2));
                continue;
            }
            edges.push(edge);
        }
        write_edges(self, ns, edges, &mut stats)?;
        Ok(stats)
    }

    /// Imports a GraphML document (see the module).
    pub fn import_graphml(
        &self,
        ns: &str,
        input: impl BufRead,
        options: &InterchangeOptions,
    ) -> Result<InterchangeStats> {
        let (nodes, edges, mut stats) = parse_graphml(input, options)?;
        write_nodes(self, ns, nodes, &mut stats)?;
        write_edges(self, ns, edges, &mut stats)?;
        Ok(stats)
    }
}

/// A `<key>`: the property it names and its type.
struct KeyDef {
    name: String,
    kind: Kind,
}

fn parse_graphml(
    input: impl BufRead,
    options: &InterchangeOptions,
) -> Result<(Vec<InNode>, Vec<InEdge>, InterchangeStats)> {
    use quick_xml::events::Event;
    let invalid = |e: &dyn std::fmt::Display| Error::Invalid(format!("invalid GraphML: {e}"));
    let mut reader = quick_xml::Reader::from_reader(input);
    reader.config_mut().trim_text(false);
    let mut keys: HashMap<String, KeyDef> = HashMap::new();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut stats = InterchangeStats::default();
    enum Open {
        Node(InNode),
        Edge(InEdge),
    }
    let mut open: Option<Open> = None;
    let mut data_key: Option<String> = None;
    let mut data_text = String::new();
    let mut buffer = Vec::new();
    let attrs = |e: &quick_xml::events::BytesStart| -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        for attr in e.attributes() {
            let attr = attr.map_err(|e| invalid(&e))?;
            let key = attr.key.local_name().as_ref().to_owned();
            let value = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|e| invalid(&e))?
                .into_owned();
            out.insert(key, value);
        }
        Ok(out)
    };
    loop {
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|e| invalid(&e))?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.local_name().as_ref().to_owned();
                let a = attrs(&e)?;
                match name.as_str() {
                    "key" => {
                        let id = a.get("id").cloned().unwrap_or_default();
                        let name = a.get("attr.name").cloned().unwrap_or_else(|| id.clone());
                        let kind = a
                            .get("attr.type")
                            .and_then(|t| Kind::from_name(t))
                            .unwrap_or(Kind::Str);
                        keys.insert(id, KeyDef { name, kind });
                    }
                    "node" => {
                        let node = InNode {
                            source: a.get("id").cloned().unwrap_or_default(),
                            labels: a
                                .get("labels")
                                .map(|l| {
                                    l.split(':')
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_owned)
                                        .collect()
                                })
                                .unwrap_or_default(),
                            props: Props::new(),
                            text: None,
                        };
                        if empty {
                            nodes.push(node);
                        } else {
                            open = Some(Open::Node(node));
                        }
                    }
                    "edge" => {
                        let edge = InEdge {
                            source: a.get("id").cloned(),
                            from: a.get("source").cloned().unwrap_or_default(),
                            to: a.get("target").cloned().unwrap_or_default(),
                            label: a
                                .get("label")
                                .cloned()
                                .unwrap_or_else(|| options.default_edge_label.clone()),
                            props: Props::new(),
                            valid_from: None,
                            valid_to: None,
                        };
                        if empty {
                            edges.push(edge);
                        } else {
                            open = Some(Open::Edge(edge));
                        }
                    }
                    "data" if !empty => {
                        data_key = a.get("key").cloned();
                        data_text.clear();
                    }
                    _ => {}
                }
            }
            Event::Text(t) if data_key.is_some() => {
                data_text.push_str(&t.xml10_content());
            }
            Event::GeneralRef(r) if data_key.is_some() => {
                let name: &str = r.as_ref();
                let named = match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    other => other
                        .strip_prefix("#x")
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .or_else(|| other.strip_prefix('#').and_then(|d| d.parse().ok()))
                        .and_then(char::from_u32),
                };
                if let Some(c) = named {
                    data_text.push(c);
                }
            }
            Event::CData(t) if data_key.is_some() => {
                data_text.push_str(t.as_ref());
            }
            Event::End(e) => {
                let name = e.local_name().as_ref().to_owned();
                match name.as_str() {
                    "data" => {
                        let key = data_key.take().unwrap_or_default();
                        let def = keys.get(&key);
                        let prop = def.map_or(key.as_str(), |d| d.name.as_str()).to_owned();
                        let kind = def.map(|d| d.kind);
                        let text = std::mem::take(&mut data_text);
                        match &mut open {
                            Some(Open::Node(node)) => match prop.as_str() {
                                "labels" => {
                                    node.labels = text
                                        .split(':')
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_owned)
                                        .collect()
                                }
                                "text" => node.text = Some(text),
                                _ => match parse_cell(
                                    &text,
                                    kind.filter(|k| *k != Kind::Str).or(Some(Kind::Str)),
                                ) {
                                    Ok(value) => {
                                        node.props.insert(prop, value);
                                    }
                                    Err(error) => {
                                        stats.fail(format!("node {}: {error}", node.source))
                                    }
                                },
                            },
                            Some(Open::Edge(edge)) => match prop.as_str() {
                                "label" if !text.trim().is_empty() => {
                                    edge.label = text.trim().to_owned()
                                }
                                "valid_from" | "valid_to" => match time_cell(&text) {
                                    Ok(time) if prop == "valid_from" => edge.valid_from = time,
                                    Ok(time) => edge.valid_to = time,
                                    Err(error) => stats.fail(format!(
                                        "edge {}: {error}",
                                        edge.source.as_deref().unwrap_or("?")
                                    )),
                                },
                                _ => match parse_cell(&text, kind.or(Some(Kind::Str))) {
                                    Ok(value) => {
                                        edge.props.insert(prop, value);
                                    }
                                    Err(error) => stats.fail(format!(
                                        "edge {}: {error}",
                                        edge.source.as_deref().unwrap_or("?")
                                    )),
                                },
                            },
                            None => {}
                        }
                    }
                    "node" => {
                        if let Some(Open::Node(mut node)) = open.take() {
                            if node.labels.is_empty() {
                                node.labels.push(options.default_label.clone());
                            }
                            nodes.push(node);
                        }
                    }
                    "edge" => {
                        if let Some(Open::Edge(edge)) = open.take() {
                            edges.push(edge);
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    // Strings that hold JSON lists or maps (as exports write them) come
    // back as lists and maps.
    for props in nodes
        .iter_mut()
        .map(|n| &mut n.props)
        .chain(edges.iter_mut().map(|e| &mut e.props))
    {
        for value in props.values_mut() {
            if let serde_json::Value::String(s) = value
                && (s.starts_with('[') || s.starts_with('{'))
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
                && (parsed.is_array() || parsed.is_object())
            {
                *value = parsed;
            }
        }
    }
    for node in &mut nodes {
        if node.labels.is_empty() {
            node.labels.push(options.default_label.clone());
        }
    }
    Ok((nodes, edges, stats))
}
