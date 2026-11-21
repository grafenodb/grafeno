//! Values that queries compute with, and how they compare.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use grafeno::{Edge, HyperEdge, Node, Timestamp};
use serde_json::json;

use crate::temporal::Duration;

/// A value in a query: a property value, a node, an edge, a hyperedge, or a
/// time.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Time(Timestamp),
    /// Months, days and microseconds (see the `temporal` module).
    Duration(Duration),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Node(Arc<Node>),
    Edge(Arc<Edge>),
    /// A matched path (`p = (a)-[…]->(b)`).
    Path(Arc<Path>),
    /// A hyperedge (`MATCH HYPEREDGE h …`).
    HyperEdge(Arc<HyperEdge>),
}

/// Nodes and the edges between them: `nodes[i]` and `nodes[i + 1]` are the
/// ends of `edges[i]`, in either direction.
#[derive(Clone, Debug)]
pub struct Path {
    pub nodes: Vec<Arc<Node>>,
    pub edges: Vec<Arc<Edge>>,
}

impl PartialEq for Value {
    /// Structural equality (used to compare syntax trees); query equality,
    /// where `null` is unknown, is [`equals`].
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Value {
    pub fn from_json(value: &serde_json::Value) -> Value {
        match value {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            serde_json::Value::String(s) => Value::Str(s.clone()),
            serde_json::Value::Array(items) => {
                Value::List(items.iter().map(Value::from_json).collect())
            }
            serde_json::Value::Object(map) => Value::Map(
                map.iter()
                    .map(|(key, value)| (key.clone(), Value::from_json(value)))
                    .collect(),
            ),
        }
    }

    /// Exact JSON for another process (a part of a query split across
    /// cluster members): types kept, floats by their bits, nodes, edges and
    /// paths whole. [`Value::from_wire`] reads it back.
    pub(crate) fn to_wire(&self) -> serde_json::Value {
        match self {
            Value::Null => json!(["n"]),
            Value::Bool(b) => json!(["b", b]),
            Value::Int(i) => json!(["i", i]),
            Value::Float(f) => json!(["f", f.to_bits()]),
            Value::Str(s) => json!(["s", s]),
            Value::Time(t) => json!(["t", t]),
            Value::Duration(d) => json!(["d", d.months, d.days, d.micros]),
            Value::List(items) => {
                json!(["l", items.iter().map(Value::to_wire).collect::<Vec<_>>()])
            }
            Value::Map(map) => json!([
                "m",
                map.iter()
                    .map(|(key, value)| (key.clone(), value.to_wire()))
                    .collect::<serde_json::Map<_, _>>()
            ]),
            Value::Node(node) => json!(["N", node.as_ref()]),
            Value::Edge(edge) => json!(["E", edge.as_ref()]),
            Value::HyperEdge(edge) => json!(["H", edge.as_ref()]),
            Value::Path(path) => json!([
                "P",
                path.nodes.iter().map(|n| n.as_ref()).collect::<Vec<_>>(),
                path.edges.iter().map(|e| e.as_ref()).collect::<Vec<_>>()
            ]),
        }
    }

    /// A value from [`Value::to_wire`].
    pub(crate) fn from_wire(json: &serde_json::Value) -> Result<Value, crate::QueryError> {
        let bad = || crate::QueryError::runtime(format!("not a wire value: {json}"));
        let parts = json.as_array().ok_or_else(bad)?;
        let tag = parts.first().and_then(|t| t.as_str()).ok_or_else(bad)?;
        let at = |i: usize| parts.get(i).ok_or_else(bad);
        fn decode<T: serde::de::DeserializeOwned>(
            value: &serde_json::Value,
        ) -> Result<T, crate::QueryError> {
            serde_json::from_value(value.clone())
                .map_err(|_| crate::QueryError::runtime("not a wire value"))
        }
        Ok(match tag {
            "n" => Value::Null,
            "b" => Value::Bool(at(1)?.as_bool().ok_or_else(bad)?),
            "i" => Value::Int(at(1)?.as_i64().ok_or_else(bad)?),
            "f" => Value::Float(f64::from_bits(at(1)?.as_u64().ok_or_else(bad)?)),
            "s" => Value::Str(at(1)?.as_str().ok_or_else(bad)?.to_owned()),
            "t" => Value::Time(decode(at(1)?)?),
            "d" => Value::Duration(Duration {
                months: at(1)?.as_i64().ok_or_else(bad)?,
                days: at(2)?.as_i64().ok_or_else(bad)?,
                micros: at(3)?.as_i64().ok_or_else(bad)?,
            }),
            "l" => Value::List(
                at(1)?
                    .as_array()
                    .ok_or_else(bad)?
                    .iter()
                    .map(Value::from_wire)
                    .collect::<Result<_, _>>()?,
            ),
            "m" => Value::Map(
                at(1)?
                    .as_object()
                    .ok_or_else(bad)?
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), Value::from_wire(value)?)))
                    .collect::<Result<_, crate::QueryError>>()?,
            ),
            "N" => Value::Node(Arc::new(decode(at(1)?)?)),
            "E" => Value::Edge(Arc::new(decode(at(1)?)?)),
            "H" => Value::HyperEdge(Arc::new(decode(at(1)?)?)),
            "P" => {
                let nodes: Vec<Node> = decode(at(1)?)?;
                let edges: Vec<Edge> = decode(at(2)?)?;
                Value::Path(Arc::new(Path {
                    nodes: nodes.into_iter().map(Arc::new).collect(),
                    edges: edges.into_iter().map(Arc::new).collect(),
                }))
            }
            _ => return Err(bad()),
        })
    }

    /// JSON for results: nodes and edges as their records, times as ISO
    /// 8601 strings.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => json!(b),
            Value::Int(i) => json!(i),
            Value::Float(f) => json!(f),
            Value::Str(s) => json!(s),
            Value::Time(t) => json!(t.iso()),
            Value::Duration(d) => json!(d.iso()),
            Value::List(items) => {
                serde_json::Value::Array(items.iter().map(Value::to_json).collect())
            }
            Value::Map(map) => serde_json::Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), value.to_json()))
                    .collect(),
            ),
            Value::Node(node) => serde_json::to_value(node.as_ref()).unwrap_or_default(),
            Value::Edge(edge) => serde_json::to_value(edge.as_ref()).unwrap_or_default(),
            Value::HyperEdge(edge) => serde_json::to_value(edge.as_ref()).unwrap_or_default(),
            Value::Path(path) => json!({
                "nodes": path.nodes.iter().map(|n| serde_json::to_value(n.as_ref()).unwrap_or_default()).collect::<Vec<_>>(),
                "relationships": path.edges.iter().map(|e| serde_json::to_value(e.as_ref()).unwrap_or_default()).collect::<Vec<_>>(),
            }),
        }
    }

    /// A property value to store. Nodes, edges and times have no place in
    /// properties, except times, which are stored as ISO strings.
    pub(crate) fn to_property(&self) -> Result<serde_json::Value, String> {
        match self {
            Value::Node(_) | Value::Edge(_) | Value::Path(_) | Value::HyperEdge(_) => {
                Err(format!("a property cannot hold a {}", self.type_name()))
            }
            Value::List(items) => items
                .iter()
                .map(Value::to_property)
                .collect::<Result<Vec<_>, _>>()
                .map(serde_json::Value::Array),
            Value::Map(map) => map
                .iter()
                .map(|(key, value)| Ok((key.clone(), value.to_property()?)))
                .collect::<Result<serde_json::Map<_, _>, String>>()
                .map(serde_json::Value::Object),
            other => Ok(other.to_json()),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Time(_) => "time",
            Value::Duration(_) => "duration",
            Value::List(_) => "list",
            Value::Map(_) => "map",
            Value::Node(_) => "node",
            Value::Edge(_) => "edge",
            Value::Path(_) => "path",
            Value::HyperEdge(_) => "hyperedge",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// A canonical text form: equal values (`1` and `1.0` included) get the
    /// same key. Used for `DISTINCT` and grouping.
    pub(crate) fn key(&self) -> String {
        match self {
            Value::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => format!("n{}", *f as i64),
            Value::Float(f) => format!("n{f}"),
            Value::Int(i) => format!("n{i}"),
            Value::Null => "_".into(),
            Value::Bool(b) => format!("b{b}"),
            Value::Str(s) => format!("s{s:?}"),
            Value::Time(t) => format!("t{}", t.as_micros()),
            Value::Duration(d) => format!("d{},{},{}", d.months, d.days, d.micros),
            Value::List(items) => {
                let keys: Vec<String> = items.iter().map(Value::key).collect();
                format!("[{}]", keys.join(","))
            }
            Value::Map(map) => {
                let keys: Vec<String> = map
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{}", v.key()))
                    .collect();
                format!("{{{}}}", keys.join(","))
            }
            Value::Node(node) => format!("N{}", node.id),
            Value::Edge(edge) => format!("E{}", edge.id),
            Value::HyperEdge(edge) => format!("H{}", edge.id),
            Value::Path(path) => {
                let mut key = String::from("P");
                for node in &path.nodes {
                    key.push_str(&format!("{};", node.id));
                }
                for edge in &path.edges {
                    key.push_str(&format!("{};", edge.id));
                }
                key
            }
        }
    }

    fn as_time(&self) -> Option<Timestamp> {
        match self {
            Value::Time(t) => Some(*t),
            Value::Str(s) => Timestamp::parse(s).ok(),
            _ => None,
        }
    }
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Query equality: `None` (unknown) when either side is `null`.
pub(crate) fn equals(a: &Value, b: &Value) -> Option<bool> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x == y),
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            Some(number(a) == number(b))
        }
        (Value::Time(_), Value::Str(_)) | (Value::Str(_), Value::Time(_)) => {
            Some(a.as_time().is_some() && a.as_time() == b.as_time())
        }
        (Value::List(x), Value::List(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (left, right) in x.iter().zip(y) {
                match equals(left, right) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        (Value::Map(x), Value::Map(y)) => {
            if x.len() != y.len() || x.keys().ne(y.keys()) {
                return Some(false);
            }
            let mut unknown = false;
            for (left, right) in x.values().zip(y.values()) {
                match equals(left, right) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        _ => Some(a.key() == b.key()),
    }
}

/// The order of `<`, `<=`, `>`, `>=`: `None` when the values cannot be
/// compared (different types, or `null`).
pub(crate) fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            number(a)?.partial_cmp(&number(b)?)
        }
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Time(_), Value::Time(_) | Value::Str(_)) | (Value::Str(_), Value::Time(_)) => {
            Some(a.as_time()?.cmp(&b.as_time()?))
        }
        (Value::Duration(x), Value::Duration(y)) => {
            x.approx_micros().partial_cmp(&y.approx_micros())
        }
        (Value::List(x), Value::List(y)) => {
            for (left, right) in x.iter().zip(y) {
                match compare(left, right)? {
                    Ordering::Equal => {}
                    other => return Some(other),
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        _ => None,
    }
}

/// A total order for `ORDER BY`: values of different types sort by type,
/// and `null` sorts last.
pub(crate) fn sort_order(a: &Value, b: &Value) -> Ordering {
    fn rank(value: &Value) -> u8 {
        match value {
            Value::Map(_) => 0,
            Value::Node(_) => 1,
            Value::Edge(_) => 2,
            Value::HyperEdge(_) => 3,
            Value::Path(_) => 4,
            Value::List(_) => 5,
            Value::Str(_) => 6,
            Value::Bool(_) => 7,
            Value::Int(_) | Value::Float(_) => 8,
            Value::Time(_) => 9,
            Value::Duration(_) => 10,
            Value::Null => 11,
        }
    }
    match (a, b) {
        (Value::Time(x), Value::Time(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        _ => compare(a, b)
            .filter(|_| rank(a) == rank(b))
            .unwrap_or_else(|| rank(a).cmp(&rank(b)).then_with(|| a.key().cmp(&b.key()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nulls_are_unknown_and_numbers_mix() {
        assert_eq!(equals(&Value::Null, &Value::Int(1)), None);
        assert_eq!(equals(&Value::Int(1), &Value::Float(1.0)), Some(true));
        assert_eq!(
            compare(&Value::Int(1), &Value::Float(1.5)),
            Some(Ordering::Less)
        );
        assert_eq!(compare(&Value::Int(1), &Value::Str("1".into())), None);
        assert_eq!(Value::Int(2).key(), Value::Float(2.0).key());
    }

    #[test]
    fn times_compare_with_iso_strings() {
        let time = Value::Time(Timestamp::parse("2024-05-01").unwrap());
        assert_eq!(
            compare(&time, &Value::Str("2024-01-01".into())),
            Some(Ordering::Greater)
        );
        assert_eq!(
            equals(&time, &Value::Str("2024-05-01T00:00:00Z".into())),
            Some(true)
        );
    }

    #[test]
    fn sorting_is_total_with_nulls_last() {
        let mut values = [
            Value::Null,
            Value::Int(3),
            Value::Str("a".into()),
            Value::Float(1.5),
        ];
        values.sort_by(sort_order);
        assert_eq!(
            values.iter().map(Value::type_name).collect::<Vec<_>>(),
            ["string", "float", "integer", "null"]
        );
    }
}
