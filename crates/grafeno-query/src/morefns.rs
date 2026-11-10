//! Functions of GQ that the core evaluator does not need:
//! paths, more list and string functions, conversions and trigonometry.
//! A `null` argument gives `null` (checked before these run).

use std::sync::Arc;

use crate::QueryError;
use crate::value::Value;

fn error(message: impl Into<String>) -> QueryError {
    QueryError::runtime(message)
}

fn arity(name: &str, args: &[Value], min: usize, max: usize) -> Result<(), QueryError> {
    if args.len() < min || args.len() > max {
        return Err(error(if min == max {
            format!(
                "{name}() takes {min} argument{}",
                if min == 1 { "" } else { "s" }
            )
        } else {
            format!("{name}() takes {min} to {max} arguments")
        }));
    }
    Ok(())
}

fn float(name: &str, value: &Value) -> Result<f64, QueryError> {
    match value {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(error(format!(
            "{name}() needs a number, not a {}",
            other.type_name()
        ))),
    }
}

fn string<'a>(name: &str, value: &'a Value) -> Result<&'a str, QueryError> {
    match value {
        Value::Str(s) => Ok(s),
        other => Err(error(format!(
            "{name}() needs a string, not a {}",
            other.type_name()
        ))),
    }
}

fn count(name: &str, value: &Value) -> Result<usize, QueryError> {
    match value {
        Value::Int(n) if *n >= 0 => Ok(*n as usize),
        other => Err(error(format!(
            "{name}() needs a length of 0 or more, not {other:?}"
        ))),
    }
}

/// A float in `[0, 1)` from the system's randomness (ids are random).
fn random() -> f64 {
    let bits = grafeno::NodeId::new().uuid().as_u128() as u64 >> 11;
    bits as f64 / (1u64 << 53) as f64
}

pub(crate) fn to_boolean(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(b) => Some(Value::Bool(*b)),
        Value::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => Some(Value::Null),
        },
        Value::Int(i) => Some(Value::Bool(*i != 0)),
        _ => None,
    }
}

/// The function `name` (lowercase), if it is one of these.
pub(crate) fn function(name: &str, args: &[Value]) -> Option<Result<Value, QueryError>> {
    if !NAMES.contains(&name) {
        return None;
    }
    Some((|| -> Result<Value, QueryError> {
        Ok(match name {
            "nodes" | "relationships" | "rels" => {
                arity(name, args, 1, 1)?;
                match &args[0] {
                    Value::Path(path) if name == "nodes" => Value::List(
                        path.nodes
                            .iter()
                            .map(|n| Value::Node(Arc::clone(n)))
                            .collect(),
                    ),
                    Value::Path(path) => Value::List(
                        path.edges
                            .iter()
                            .map(|e| Value::Edge(Arc::clone(e)))
                            .collect(),
                    ),
                    other => {
                        return Err(error(format!(
                            "{name}() needs a path, not a {}",
                            other.type_name()
                        )));
                    }
                }
            }
            "reverse" => {
                arity(name, args, 1, 1)?;
                match &args[0] {
                    Value::Str(s) => Value::Str(s.chars().rev().collect()),
                    Value::List(items) => Value::List(items.iter().rev().cloned().collect()),
                    other => {
                        return Err(error(format!(
                            "reverse() needs a string or list, not a {}",
                            other.type_name()
                        )));
                    }
                }
            }
            "tail" => {
                arity(name, args, 1, 1)?;
                match &args[0] {
                    Value::List(items) => Value::List(items.iter().skip(1).cloned().collect()),
                    other => {
                        return Err(error(format!(
                            "tail() needs a list, not a {}",
                            other.type_name()
                        )));
                    }
                }
            }
            "isempty" => {
                arity(name, args, 1, 1)?;
                match &args[0] {
                    Value::Str(s) => Value::Bool(s.is_empty()),
                    Value::List(items) => Value::Bool(items.is_empty()),
                    Value::Map(map) => Value::Bool(map.is_empty()),
                    other => {
                        return Err(error(format!(
                            "isEmpty() needs a string, list or map, not a {}",
                            other.type_name()
                        )));
                    }
                }
            }
            "toboolean" => {
                arity(name, args, 1, 1)?;
                to_boolean(&args[0]).ok_or_else(|| {
                    error(format!(
                        "toBoolean() needs a boolean, string or integer, not a {}",
                        args[0].type_name()
                    ))
                })?
            }
            "tobooleanlist" => {
                arity(name, args, 1, 1)?;
                match &args[0] {
                    Value::List(items) => Value::List(
                        items
                            .iter()
                            .map(|item| to_boolean(item).unwrap_or(Value::Null))
                            .collect(),
                    ),
                    other => {
                        return Err(error(format!(
                            "toBooleanList() needs a list, not a {}",
                            other.type_name()
                        )));
                    }
                }
            }
            "ltrim" => Value::Str(string(name, &args[0])?.trim_start().to_owned()),
            "rtrim" => Value::Str(string(name, &args[0])?.trim_end().to_owned()),
            "left" | "right" => {
                arity(name, args, 2, 2)?;
                let text = string(name, &args[0])?;
                let n = count(name, &args[1])?;
                let chars: Vec<char> = text.chars().collect();
                let n = n.min(chars.len());
                Value::Str(if name == "left" {
                    chars[..n].iter().collect()
                } else {
                    chars[chars.len() - n..].iter().collect()
                })
            }
            "char_length" | "character_length" => {
                arity(name, args, 1, 1)?;
                Value::Int(string(name, &args[0])?.chars().count() as i64)
            }
            "nullif" => {
                arity(name, args, 2, 2)?;
                if crate::value::equals(&args[0], &args[1]) == Some(true) {
                    Value::Null
                } else {
                    args[0].clone()
                }
            }
            "asin" | "acos" | "atan" | "cot" | "degrees" | "radians" | "haversin" => {
                arity(name, args, 1, 1)?;
                let x = float(name, &args[0])?;
                Value::Float(match name {
                    "asin" => x.asin(),
                    "acos" => x.acos(),
                    "atan" => x.atan(),
                    "cot" => 1.0 / x.tan(),
                    "degrees" => x.to_degrees(),
                    "radians" => x.to_radians(),
                    _ => (1.0 - x.cos()) / 2.0,
                })
            }
            "atan2" => {
                arity(name, args, 2, 2)?;
                Value::Float(float(name, &args[0])?.atan2(float(name, &args[1])?))
            }
            "isnan" => {
                arity(name, args, 1, 1)?;
                Value::Bool(float(name, &args[0])?.is_nan())
            }
            "rand" => {
                arity(name, args, 0, 0)?;
                Value::Float(random())
            }
            "randomuuid" => {
                arity(name, args, 0, 0)?;
                Value::Str(grafeno::NodeId::new().to_string())
            }
            "elementid" => {
                arity(name, args, 1, 1)?;
                match &args[0] {
                    Value::Node(node) => Value::Str(node.id.to_string()),
                    Value::Edge(edge) => Value::Str(edge.id.to_string()),
                    other => {
                        return Err(error(format!(
                            "elementId() needs a node or edge, not a {}",
                            other.type_name()
                        )));
                    }
                }
            }
            _ => unreachable!("listed in NAMES"),
        })
    })())
}

/// The functions of this module.
pub(crate) const NAMES: &[&str] = &[
    "nodes",
    "relationships",
    "rels",
    "reverse",
    "tail",
    "isempty",
    "toboolean",
    "tobooleanlist",
    "ltrim",
    "rtrim",
    "left",
    "right",
    "char_length",
    "character_length",
    "nullif",
    "asin",
    "acos",
    "atan",
    "cot",
    "degrees",
    "radians",
    "haversin",
    "atan2",
    "isnan",
    "rand",
    "randomuuid",
    "elementid",
];
