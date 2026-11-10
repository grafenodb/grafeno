//! Text similarity in GQ: `text.*` functions over `grafeno::textsim`, and
//!
//! A `null` argument gives `null`, as other functions do.

use grafeno::textsim::{self, Measure};

use crate::QueryError;
use crate::value::Value;

fn error(message: impl Into<String>) -> QueryError {
    QueryError::runtime(message)
}

fn text<'a>(name: &str, value: &'a Value) -> Result<&'a str, QueryError> {
    match value {
        Value::Str(s) => Ok(s),
        other => Err(error(format!(
            "{name}() compares strings, not a {}",
            other.type_name()
        ))),
    }
}

fn count(name: &str, value: Option<&Value>, default: usize) -> Result<usize, QueryError> {
    match value {
        None => Ok(default),
        Some(Value::Int(n)) if (1..=8).contains(n) => Ok(*n as usize),
        Some(other) => Err(error(format!(
            "{name}() takes an n-gram size from 1 to 8, not {other:?}"
        ))),
    }
}

/// The function `name` (lowercase) if it is a text function.
pub(crate) fn function(name: &str, args: &[Value]) -> Option<Result<Value, QueryError>> {
    let (min, max) = match name {
        "text.normalize" | "text.soundex" => (1, 1),
        "text.phonetic" | "text.ngrams" => (1, 2),
        "text.levenshtein" | "text.damerau" | "text.jaro" | "text.jarowinkler" | "text.lcs" => {
            (2, 2)
        }
        "text.dice" | "text.jaccard" | "text.similarity" => (2, 3),
        _ => return None,
    };
    Some(run(name, args, min, max))
}

fn run(name: &str, args: &[Value], min: usize, max: usize) -> Result<Value, QueryError> {
    if args.len() < min || args.len() > max {
        return Err(error(format!(
            "{name}() takes {} argument{}",
            if min == max {
                min.to_string()
            } else {
                format!("{min} or {max}")
            },
            if max == 1 { "" } else { "s" }
        )));
    }
    // The strings compared: one or two; a null among them gives null.
    let strings = match name {
        "text.normalize" | "text.soundex" | "text.phonetic" | "text.ngrams" => 1,
        _ => 2,
    };
    if args[..strings].iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let a = text(name, &args[0])?;
    let b = || text(name, &args[1]);
    Ok(match name {
        "text.normalize" => Value::Str(grafeno::normalize_name(a)),
        "text.soundex" => Value::Str(textsim::soundex(a)),
        "text.phonetic" => {
            let language = match args.get(1) {
                None | Some(Value::Null) => "es",
                Some(value) => text(name, value)?,
            };
            match language.to_ascii_lowercase().as_str() {
                "es" | "spanish" => Value::Str(textsim::phonetic_es(a)),
                "en" | "english" => Value::Str(textsim::soundex(a)),
                other => {
                    return Err(error(format!(
                        "text.phonetic() knows 'es' and 'en', not {other:?}"
                    )));
                }
            }
        }
        "text.ngrams" => {
            let n = count(name, args.get(1), 3)?;
            let mut grams: Vec<String> = textsim::ngrams(&grafeno::normalize_name(a), n)
                .into_iter()
                .collect();
            grams.sort();
            Value::List(grams.into_iter().map(Value::Str).collect())
        }
        "text.levenshtein" => Value::Int(textsim::levenshtein(a, b()?) as i64),
        "text.damerau" => Value::Int(textsim::damerau_levenshtein(a, b()?) as i64),
        "text.lcs" => Value::Int(textsim::lcs(a, b()?) as i64),
        "text.jaro" => Value::Float(textsim::jaro(a, b()?)),
        "text.jarowinkler" => Value::Float(textsim::jaro_winkler(a, b()?)),
        "text.dice" => Value::Float(textsim::dice(a, b()?, count(name, args.get(2), 3)?)),
        "text.jaccard" => Value::Float(textsim::jaccard(a, b()?, count(name, args.get(2), 3)?)),
        "text.similarity" => {
            let measure = match args.get(2) {
                None | Some(Value::Null) => Measure::JaroWinkler,
                Some(value) => {
                    let method = text(name, value)?;
                    Measure::by_name(method).ok_or_else(|| {
                        error(format!(
                            "text.similarity() knows exact, edit, jaro, jaro_winkler, dice, bigram, jaccard, tokens, lcs, soundex, phonetic_es, date and number, not {method:?}"
                        ))
                    })?
                }
            };
            Value::Float(measure.compare(a, b()?))
        }
        _ => unreachable!("checked above"),
    })
}
