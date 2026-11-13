//! Ingesting documents, conversations and code: a long text cut into
//! chunks an agent can recall one by one, kept together under the node of
//! the whole, in order, with where each came from. No model is involved:
//! the cuts follow the text's own structure.
//!
//! - **Documents** are cut at their headings (`#`, `##`, … in Markdown),
//!   then at blank lines when a section is long, then at sentence ends.
//!   Each chunk starts with the heading path it is under.
//! - **Conversations** are cut between turns: a line that starts with a
//!   speaker (`Ana: …`, `[10:30] user: …`) begins one, and the lines after
//!   it belong to it. Consecutive turns share a chunk while they fit.
//! - **Code** is cut at definitions that start a line without indentation
//!   (`fn`, `def`, `class`, `function`, `struct`, `impl`, …), with the
//!   comments, attributes and decorators right above them.
//!
//! The whole is a node labeled [`DOCUMENT`], [`CONVERSATION`] or [`CODE`]
//! with the chunks ([`CHUNK`]) as its parts ([`HAS_PART`]) and each chunk
//! linked to the next ([`NEXT`]). Ingesting the same `source` again
//! replaces its chunks.

use std::sync::LazyLock;

use grafeno::{Direction, EdgeFilter, NewEdge, NewNode, NodeId, Result, WriteTx};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DOCUMENT: &str = "Document";
pub const CONVERSATION: &str = "Conversation";
pub const CODE: &str = "Code";
/// The label of every chunk (extraction reads chunks).
pub const CHUNK: &str = "Chunk";
/// The whole → each chunk.
pub const HAS_PART: &str = "has_part";
/// A chunk → the one after it.
pub const NEXT: &str = "next";

/// Characters a chunk holds at most, by default.
pub const DEFAULT_CHUNK_CHARS: usize = 1_500;

/// What kind of text it is, which decides where it is cut.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Document,
    Conversation,
    Code,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Document => DOCUMENT,
            Kind::Conversation => CONVERSATION,
            Kind::Code => CODE,
        }
    }
}

/// A piece of the text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Piece {
    pub text: String,
    /// The heading path, the speakers, or the definitions it holds.
    pub title: Option<String>,
    /// First and last line, from 1.
    pub lines: (usize, usize),
}

/// A unit of the text that is not cut (a paragraph, a turn, a
/// definition), with its lines.
struct Unit {
    text: String,
    title: Option<String>,
    lines: (usize, usize),
}

static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.+?)\s*#*\s*$").expect("valid"));
static TURN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\[[^\]]{1,40}\]\s*)?([\p{L}\p{N}_][\p{L}\p{N}_ .'-]{0,39}):\s+\S")
        .expect("valid")
});
static DEFINITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:export\s+(?:default\s+)?)?(?:pub(?:\([^)]*\))?\s+)?(?:public\s+|private\s+|protected\s+|static\s+|abstract\s+|final\s+)*(?:async\s+)?(?:unsafe\s+)?(fn|def|class|function|struct|enum|trait|impl|interface|type|mod|module|func|record|object)\b\s*([\p{L}_][\p{L}\p{N}_]*)?",
    )
    .expect("valid")
});

/// Cuts `content` into pieces of at most `max_chars` characters (a single
/// sentence or line longer than that is cut where it must be).
pub fn cut(content: &str, kind: Kind, max_chars: usize) -> Vec<Piece> {
    let max_chars = max_chars.max(100);
    let units = match kind {
        Kind::Document => document_units(content, max_chars),
        Kind::Conversation => match conversation_units(content) {
            Some(units) => units,
            None => document_units(content, max_chars),
        },
        Kind::Code => code_units(content),
    };
    pack(units, kind, max_chars)
}

/// Paragraphs under their heading path.
fn document_units(content: &str, max_chars: usize) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut start = 0;
    let title = |path: &[(usize, String)]| {
        (!path.is_empty()).then(|| {
            path.iter()
                .map(|(_, heading)| heading.as_str())
                .collect::<Vec<_>>()
                .join(" > ")
        })
    };
    let mut close = |paragraph: &mut Vec<&str>, start: usize, end: usize, title: Option<String>| {
        if paragraph.iter().any(|line| !line.trim().is_empty()) {
            let text = paragraph.join("\n").trim().to_owned();
            for (at, part) in split_long(&text, max_chars).into_iter().enumerate() {
                let _ = at;
                units.push(Unit {
                    text: part,
                    title: title.clone(),
                    lines: (start, end),
                });
            }
        }
        paragraph.clear();
    };
    for (index, line) in content.lines().enumerate() {
        let number = index + 1;
        if let Some(found) = HEADING.captures(line) {
            close(&mut paragraph, start, number - 1, title(&path));
            let depth = found[1].len();
            path.retain(|(level, _)| *level < depth);
            path.push((depth, found[2].to_owned()));
            continue;
        }
        if line.trim().is_empty() {
            close(&mut paragraph, start, number - 1, title(&path));
            continue;
        }
        if paragraph.is_empty() {
            start = number;
        }
        paragraph.push(line);
    }
    let end = content.lines().count();
    close(&mut paragraph, start, end, title(&path));
    units
}

/// Turns, when at least two lines start one.
fn conversation_units(content: &str) -> Option<Vec<Unit>> {
    let lines: Vec<&str> = content.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| TURN.is_match(line))
        .map(|(at, _)| at)
        .collect();
    if starts.len() < 2 {
        return None;
    }
    let mut units = Vec::new();
    // Lines before the first turn (a header) are a unit of their own.
    if starts[0] > 0 {
        let text = lines[..starts[0]].join("\n").trim().to_owned();
        if !text.is_empty() {
            units.push(Unit {
                text,
                title: None,
                lines: (1, starts[0]),
            });
        }
    }
    for (at, &start) in starts.iter().enumerate() {
        let end = starts.get(at + 1).copied().unwrap_or(lines.len());
        let speaker = TURN
            .captures(lines[start])
            .map(|found| found[1].trim().to_owned());
        units.push(Unit {
            text: lines[start..end].join("\n").trim_end().to_owned(),
            title: speaker,
            lines: (start + 1, end),
        });
    }
    Some(units)
}

/// Definitions, each with the comments, attributes and decorators above
/// it; what comes before the first is a unit of its own.
fn code_units(content: &str) -> Vec<Unit> {
    let lines: Vec<&str> = content.lines().collect();
    let mut starts: Vec<(usize, Option<String>)> = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(found) = DEFINITION.captures(line) else {
            continue;
        };
        let name = found
            .get(2)
            .map(|name| format!("{} {}", &found[1], name.as_str()))
            .unwrap_or_else(|| found[1].to_owned());
        // Comments, attributes and decorators right above belong to it.
        let mut start = at;
        while start > 0 {
            let above = lines[start - 1].trim_start();
            let attached = !lines[start - 1].starts_with(char::is_whitespace)
                && (above.starts_with("//")
                    || above.starts_with('#')
                    || above.starts_with('@')
                    || above.starts_with("/*")
                    || above.starts_with('*')
                    || above.starts_with("--"));
            if !attached {
                break;
            }
            start -= 1;
        }
        if starts.last().is_some_and(|(last, _)| *last >= start) {
            continue;
        }
        starts.push((start, Some(name)));
    }
    let mut units = Vec::new();
    let first = starts.first().map_or(lines.len(), |(start, _)| *start);
    if first > 0 {
        let text = lines[..first].join("\n").trim().to_owned();
        if !text.is_empty() {
            units.push(Unit {
                text,
                title: None,
                lines: (1, first),
            });
        }
    }
    for (at, (start, name)) in starts.iter().enumerate() {
        let end = starts.get(at + 1).map_or(lines.len(), |(next, _)| *next);
        let text = lines[*start..end].join("\n").trim_end().to_owned();
        if !text.trim().is_empty() {
            units.push(Unit {
                text,
                title: name.clone(),
                lines: (start + 1, end),
            });
        }
    }
    units
}

/// Splits a text longer than `max_chars` at sentence ends, then at spaces,
/// then anywhere.
fn split_long(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_owned()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let push = |current: &mut String, out: &mut Vec<String>| {
        let piece = current.trim();
        if !piece.is_empty() {
            out.push(piece.to_owned());
        }
        current.clear();
    };
    // Sentences, keeping their ending.
    let mut sentences = Vec::new();
    let mut start = 0;
    let bytes: Vec<(usize, char)> = text.char_indices().collect();
    for (at, &(offset, c)) in bytes.iter().enumerate() {
        let next_is_space = bytes
            .get(at + 1)
            .is_none_or(|(_, next)| next.is_whitespace());
        if matches!(c, '.' | '!' | '?' | '\n') && next_is_space {
            let end = offset + c.len_utf8();
            sentences.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        sentences.push(&text[start..]);
    }
    for sentence in sentences {
        if current.chars().count() + sentence.chars().count() > max_chars {
            push(&mut current, &mut out);
        }
        if sentence.chars().count() > max_chars {
            // One sentence too long: at spaces, then anywhere.
            for word in sentence.split_inclusive(char::is_whitespace) {
                if current.chars().count() + word.chars().count() > max_chars {
                    push(&mut current, &mut out);
                }
                if word.chars().count() > max_chars {
                    let chars: Vec<char> = word.chars().collect();
                    for part in chars.chunks(max_chars) {
                        out.push(part.iter().collect());
                    }
                    continue;
                }
                current.push_str(word);
            }
            continue;
        }
        current.push_str(sentence);
    }
    push(&mut current, &mut out);
    out
}

/// A piece being filled: its texts, titles and lines.
type Filling = Option<(Vec<String>, Vec<String>, (usize, usize))>;

/// Puts consecutive units together while they fit, and cuts a unit too
/// long for one piece. A document's piece starts with its heading path.
fn pack(units: Vec<Unit>, kind: Kind, max_chars: usize) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut current: Filling = None;
    let flush = |current: &mut Filling, pieces: &mut Vec<Piece>| {
        if let Some((texts, titles, lines)) = current.take() {
            let title = (!titles.is_empty()).then(|| titles.join(", "));
            let body = texts.join("\n\n");
            let text = match (&title, kind) {
                (Some(title), Kind::Document) => format!("{title}\n\n{body}"),
                _ => body,
            };
            pieces.push(Piece { text, title, lines });
        }
    };
    for unit in units {
        let size = unit.text.chars().count();
        let parts = if size > max_chars {
            split_long(&unit.text, max_chars)
        } else {
            vec![unit.text]
        };
        for part in parts {
            let part_size = part.chars().count();
            let fits = current.as_ref().is_some_and(|(texts, titles, _)| {
                let used: usize = texts.iter().map(|t| t.chars().count() + 2).sum();
                // A document's chunk keeps to one section.
                let same_section = kind != Kind::Document
                    || titles.last().map(String::as_str) == unit.title.as_deref()
                    || (titles.is_empty() && unit.title.is_none());
                used + part_size <= max_chars && same_section
            });
            if !fits {
                flush(&mut current, &mut pieces);
            }
            let (texts, titles, lines) = current
                .get_or_insert_with(|| (Vec::new(), Vec::new(), (unit.lines.0, unit.lines.1)));
            texts.push(part);
            if let Some(title) = &unit.title
                && titles.last() != Some(title)
                && !titles.contains(title)
            {
                titles.push(title.clone());
            }
            lines.1 = lines.1.max(unit.lines.1);
        }
    }
    flush(&mut current, &mut pieces);
    pieces
}

/// What to ingest.
#[derive(Clone, Debug, Default)]
pub struct NewDocument {
    pub content: String,
    pub kind: Kind,
    /// Its name; the source, or its first line, when absent.
    pub title: Option<String>,
    /// Where it comes from (a path, a URL): ingesting it again replaces it.
    pub source: Option<String>,
    /// More labels for the whole.
    pub labels: Vec<String>,
    /// Characters per chunk at most ([`DEFAULT_CHUNK_CHARS`] when 0).
    pub max_chars: usize,
}

/// What ingesting did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Ingested {
    /// The node of the whole.
    pub id: Option<NodeId>,
    pub chunks: Vec<NodeId>,
    /// Chunks of an earlier ingest of the same source, deleted.
    pub replaced: usize,
}

/// Stores `new` as a whole and its chunks.
pub fn ingest_document(tx: &mut WriteTx, ns: &str, new: NewDocument) -> Result<Ingested> {
    let max = if new.max_chars == 0 {
        DEFAULT_CHUNK_CHARS
    } else {
        new.max_chars
    };
    let pieces = cut(&new.content, new.kind, max);
    if pieces.is_empty() {
        return Err(grafeno::Error::Invalid("there is nothing to ingest".into()));
    }
    let label = new.kind.label();
    let title = new
        .title
        .clone()
        .or_else(|| new.source.clone())
        .unwrap_or_else(|| {
            let first = new.content.lines().find(|line| !line.trim().is_empty());
            let first = first.unwrap_or_default().trim_start_matches('#').trim();
            first.chars().take(80).collect()
        });

    // The same source again: its old chunks go.
    let mut existing = None;
    if let Some(source) = &new.source {
        let mut after = None;
        'pages: loop {
            let page = tx.nodes_page(ns, Some(label), after, 1_000)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for node in page {
                if node.prop("source").and_then(Value::as_str) == Some(source.as_str()) {
                    existing = Some(node.id);
                    break 'pages;
                }
            }
        }
    }
    let mut replaced = 0;
    let whole = match existing {
        Some(id) => {
            let parts =
                tx.neighbor_ids(ns, id, Direction::Out, &EdgeFilter::any().label(HAS_PART))?;
            for (_, part) in parts {
                tx.delete_node(ns, part)?;
                replaced += 1;
            }
            let mut props = serde_json::Map::new();
            props.insert("name".into(), Value::String(title.clone()));
            props.insert("chunks".into(), Value::from(pieces.len()));
            props.insert("lines".into(), Value::from(new.content.lines().count()));
            tx.set_node_props(ns, id, props)?;
            tx.set_text(ns, id, &title)?;
            for extra in &new.labels {
                tx.add_label(ns, id, extra)?;
            }
            id
        }
        None => {
            let mut node = NewNode::new(label)
                .prop("name", title.clone())
                .prop("kind", serde_json::to_value(new.kind).unwrap_or_default())
                .prop("chunks", pieces.len())
                .prop("lines", new.content.lines().count())
                .text(title.clone());
            if let Some(source) = &new.source {
                node = node.prop("source", source.clone());
            }
            for extra in &new.labels {
                node = node.label(extra.clone());
            }
            tx.create_node(ns, node)?
        }
    };

    let mut chunks = Vec::with_capacity(pieces.len());
    for (index, piece) in pieces.iter().enumerate() {
        let mut node = NewNode::new(CHUNK)
            .prop("index", index)
            .prop("lines", format!("{}-{}", piece.lines.0, piece.lines.1))
            .text(piece.text.clone());
        node.sources = vec![whole];
        if let Some(title) = &piece.title {
            node = node.prop("part", title.clone());
        }
        if let Some(source) = &new.source {
            node = node.prop("source", source.clone());
        }
        let id = tx.create_node(ns, node)?;
        tx.create_edge(ns, NewEdge::new(whole, HAS_PART, id).prop("index", index))?;
        if let Some(&before) = chunks.last() {
            tx.create_edge(ns, NewEdge::new(before, NEXT, id))?;
        }
        chunks.push(id);
    }
    Ok(Ingested {
        id: Some(whole),
        chunks,
        replaced,
    })
}

#[cfg(test)]
mod tests {
    use super::{Kind, code_units, conversation_units, cut};

    #[test]
    fn documents_cut_at_headings_and_keep_their_path() {
        let text = "# Guide\n\nIntro words.\n\n## Install\n\nRun the installer.\n\nThen start it.\n\n## Use\n\nOpen the shell.";
        let pieces = cut(text, Kind::Document, 1_000);
        let titles: Vec<_> = pieces.iter().map(|p| p.title.clone().unwrap()).collect();
        assert_eq!(titles, ["Guide", "Guide > Install", "Guide > Use"]);
        assert!(
            pieces[1]
                .text
                .starts_with("Guide > Install\n\nRun the installer.")
        );
        assert!(pieces[1].text.contains("Then start it."));
        assert_eq!(pieces[2].lines, (13, 13));
    }

    #[test]
    fn long_sections_cut_at_paragraphs_then_sentences() {
        let sentence = "This sentence has some words in it. ";
        let paragraph = sentence.repeat(10);
        let text = format!("{paragraph}\n\n{paragraph}\n\n{}", sentence.repeat(40));
        let pieces = cut(&text, Kind::Document, 400);
        assert!(pieces.len() >= 4);
        assert!(pieces.iter().all(|p| p.text.chars().count() <= 400));
        assert!(pieces.iter().all(|p| p.text.trim_end().ends_with('.')));
    }

    #[test]
    fn conversations_cut_between_turns() {
        let text = "Meeting notes
Ana: We should ship on Friday.
It needs the docs.
Bob: Agreed.
[10:31] Ana: I will write them.";
        let units = conversation_units(text).unwrap();
        let turns: Vec<_> = units.iter().map(|u| (u.title.clone(), u.lines)).collect();
        assert_eq!(
            turns,
            [
                (None, (1, 1)),
                (Some("Ana".into()), (2, 3)),
                (Some("Bob".into()), (4, 4)),
                (Some("Ana".into()), (5, 5)),
            ]
        );
        assert_eq!(
            units[1].text,
            "Ana: We should ship on Friday.
It needs the docs."
        );
        // Turns that fit share a chunk; one that does not starts the next.
        let all = cut(text, Kind::Conversation, 1_000);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title.as_deref(), Some("Ana, Bob"));
        let small = cut(text, Kind::Conversation, 100);
        assert!(small.len() > 1);
        assert!(small.iter().all(|p| p.text.chars().count() <= 100));
        // Without turns, a conversation is cut as a document.
        assert!(conversation_units("just one line: of text").is_none());
    }

    #[test]
    fn code_cuts_at_definitions_with_what_is_above_them() {
        let text = "use std::io;

/// Adds.
#[inline]
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

struct Point {
    x: i32,
}

impl Point {
    fn new() -> Self {
        Point { x: 0 }
    }
}
";
        let units = code_units(text);
        let titles: Vec<_> = units.iter().map(|u| u.title.clone()).collect();
        assert_eq!(
            titles,
            [
                None,
                Some("fn add".into()),
                Some("struct Point".into()),
                Some("impl Point".into())
            ]
        );
        assert!(units[1].text.starts_with(
            "/// Adds.
#[inline]
pub fn add"
        ));
        assert_eq!(units[1].lines, (3, 8));
        // The nested `fn new` is part of `impl Point`, not a unit.
        assert!(units[3].text.contains("fn new"));
        let python = "import os

@cache
def load(path):
    return open(path)

class Store:
    pass
";
        let titles: Vec<_> = code_units(python).iter().map(|u| u.title.clone()).collect();
        assert_eq!(
            titles,
            [None, Some("def load".into()), Some("class Store".into())]
        );
        let packed = cut(text, Kind::Code, 1_000);
        assert_eq!(packed.len(), 1);
        assert_eq!(
            packed[0].title.as_deref(),
            Some("fn add, struct Point, impl Point")
        );
    }
}
