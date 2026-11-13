use std::collections::{HashMap, HashSet};

use grafeno::{Direction, Edge, EdgeFilter, EdgeId, Node, NodeId, ReadTx, Result};
use serde::Serialize;

use crate::{RecallQuery, recall};

const MEMORY_HEADER: &str = "## Memory\n";
const FACTS_HEADER: &str = "\n## Facts\n";
const OVERVIEW_HEADER: &str = "## Overview\n";
/// Below this many tokens, a truncated item is not worth including.
const MIN_TRUNCATED_TOKENS: usize = 24;
const ELLIPSIS: &str = "…";

/// A token count that errs on the high side, for when no tokenizer is
/// given. Four characters a token holds for English prose but runs short on
/// code, structured data and other languages, so it counts by kind of
/// character: a run of ASCII letters is a token per four letters (at least
/// one a word), digits a token per three, every other character outside
/// ASCII (accented letters, other scripts, emoji) a token each, and every
/// punctuation mark or symbol a token. A run of whitespace costs a token
/// when it holds a line break or more than one space (indentation), and
/// nothing between words. The count only grows as text is added.
pub fn estimate_tokens(text: &str) -> usize {
    #[derive(Clone, Copy, PartialEq)]
    enum Run {
        Other,
        Letters,
        Digits,
        Space,
    }
    fn close(run: Run, length: usize) -> usize {
        match run {
            Run::Letters => length.div_ceil(4),
            Run::Digits => length.div_ceil(3),
            Run::Other | Run::Space => 0,
        }
    }
    let mut tokens = 0;
    let mut run = Run::Other;
    let mut length = 0;
    // Whether the current run of whitespace already cost its token.
    let mut space_counted = false;
    for c in text.chars() {
        let kind = if c.is_ascii_alphabetic() {
            Run::Letters
        } else if c.is_ascii_digit() {
            Run::Digits
        } else if c.is_whitespace() {
            Run::Space
        } else {
            Run::Other
        };
        if kind != run {
            tokens += close(run, length);
            length = 0;
            space_counted = false;
        }
        match kind {
            Run::Other => tokens += 1,
            Run::Space if !space_counted && (c == '\n' || length > 0) => {
                tokens += 1;
                space_counted = true;
            }
            _ => {}
        }
        length += 1;
        run = kind;
    }
    tokens + close(run, length)
}

#[derive(Clone, Debug)]
pub struct ContextOptions {
    /// Maximum tokens of the rendered context.
    pub budget: usize,
    /// Share of the budget kept for facts, from 0 to 1. Facts also get
    /// whatever the items leave unused.
    pub fact_share: f32,
    /// Maximum facts considered.
    pub max_facts: usize,
    /// Facts (edges) must pass this filter.
    pub facts: EdgeFilter,
    /// Counts the tokens of a piece of text. Defaults to [`estimate_tokens`];
    /// pass the model's tokenizer for exact budgets.
    pub count_tokens: fn(&str) -> usize,
    /// Community summaries matching the query put first, as an overview
    /// (0, the default, for none). They share the items' budget.
    pub communities: usize,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self::with_budget(2_000)
    }
}

impl ContextOptions {
    pub fn with_budget(budget: usize) -> Self {
        Self {
            budget,
            fact_share: 0.25,
            max_facts: 50,
            facts: EdgeFilter::any(),
            count_tokens: estimate_tokens,
            communities: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextItem {
    pub node: NodeId,
    pub name: String,
    pub labels: Vec<String>,
    /// The indexed text, or the other properties as JSON.
    pub text: String,
    pub score: f32,
    pub sources: Vec<NodeId>,
    /// The text was cut to fit the budget.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextFact {
    pub edge: EdgeId,
    pub from: NodeId,
    pub label: String,
    pub to: NodeId,
    /// The rendered line, such as `Ana works_at Acme (from 2024-01-01)`.
    pub text: String,
}

/// Recalled knowledge packed for a prompt.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Context {
    /// Community summaries put first (`ContextOptions::communities`).
    pub overview: Vec<NodeId>,
    pub items: Vec<ContextItem>,
    pub facts: Vec<ContextFact>,
    /// Tokens of `rendered`, as counted by `ContextOptions::count_tokens`
    /// (the sum of its pieces).
    pub tokens: usize,
    pub rendered: String,
}

/// Recalls nodes for `query` and packs them, best first, with the facts
/// that connect them, into at most `options.budget` tokens.
pub fn context_for(
    read: &ReadTx,
    ns: &str,
    query: &RecallQuery,
    options: &ContextOptions,
) -> Result<Context> {
    let fact_reserve = (options.budget as f32 * options.fact_share.clamp(0.0, 1.0)) as usize;
    let item_budget = options.budget.saturating_sub(fact_reserve);
    let mut context = Context::default();
    if options.communities > 0
        && let Some(text) = &query.text
    {
        add_overview(read, ns, text, options, item_budget, &mut context)?;
    }
    let nodes = recall(read, ns, query)?
        .into_iter()
        .map(|recalled| (recalled.node, recalled.score))
        .collect();
    add_items(read, ns, nodes, options, item_budget, &mut context)?;
    add_facts(read, ns, options, &mut context)?;
    Ok(context)
}

/// Packs these nodes, in the order given, with the facts that connect
/// them, into at most `options.budget` tokens: [`context_for`] for nodes
/// chosen some other way than by a recall. Each comes with its score.
pub fn context_of(
    read: &ReadTx,
    ns: &str,
    nodes: Vec<(Node, f32)>,
    options: &ContextOptions,
) -> Result<Context> {
    let fact_reserve = (options.budget as f32 * options.fact_share.clamp(0.0, 1.0)) as usize;
    let item_budget = options.budget.saturating_sub(fact_reserve);
    let mut context = Context::default();
    add_items(read, ns, nodes, options, item_budget, &mut context)?;
    add_facts(read, ns, options, &mut context)?;
    Ok(context)
}

/// The items, each whole or cut to what is left of `item_budget`.
fn add_items(
    read: &ReadTx,
    ns: &str,
    nodes: Vec<(Node, f32)>,
    options: &ContextOptions,
    item_budget: usize,
    context: &mut Context,
) -> Result<()> {
    let count = options.count_tokens;
    for (node, score) in &nodes {
        let mut text = item_body(read, ns, node)?;
        let name = readable_name(node, &text);
        // An unnamed item's name is the start of its own text: don't repeat it.
        let heading = if node.explicit_name().is_some() || text.is_empty() {
            name.clone()
        } else {
            String::new()
        };
        let number = context.items.len() + 1;
        let header_cost = if context.items.is_empty() {
            count(MEMORY_HEADER)
        } else {
            0
        };
        let available = item_budget.saturating_sub(context.tokens + header_cost);

        // Its labels, and how it is known when someone said.
        let known = crate::knowledge::of_node(node).note();
        let tags = if known.is_empty() {
            node.labels.join(", ")
        } else {
            format!("{}; {known}", node.labels.join(", "))
        };
        let mut block = render_item(number, &heading, &tags, &text);
        let mut cost = count(&block);
        let mut truncated = false;
        if cost > available {
            let frame = count(&render_item(number, &heading, &tags, ""));
            if text.is_empty() || available < frame + MIN_TRUNCATED_TOKENS {
                continue;
            }
            // The frame and the text may cost a little more together than
            // apart (whitespace where they meet): take the excess off the
            // text and try again.
            let whole = text;
            let mut limit = available - frame;
            loop {
                text = truncate(&whole, limit, count);
                block = render_item(number, &heading, &tags, &text);
                cost = count(&block);
                if cost <= available || limit < MIN_TRUNCATED_TOKENS {
                    break;
                }
                limit = limit.saturating_sub(cost - available);
            }
            if cost > available {
                continue;
            }
            truncated = true;
        }
        if header_cost > 0 {
            context.rendered.push_str(MEMORY_HEADER);
        }
        context.rendered.push_str(&block);
        context.tokens += header_cost + cost;
        context.items.push(ContextItem {
            node: node.id,
            name,
            labels: node.labels.clone(),
            text,
            score: *score,
            sources: node.sources.clone(),
            truncated,
        });
    }
    Ok(())
}

/// The community summaries that match the query, before the memories.
fn add_overview(
    read: &ReadTx,
    ns: &str,
    text: &str,
    options: &ContextOptions,
    budget: usize,
    context: &mut Context,
) -> Result<()> {
    let count = options.count_tokens;
    let hits = crate::communities::overview(
        read,
        ns,
        Some(text),
        &crate::communities::OverviewOptions {
            limit: options.communities,
            level: None,
        },
    )?;
    for hit in hits {
        let header = if context.overview.is_empty() {
            OVERVIEW_HEADER
        } else {
            ""
        };
        let line = format!(
            "- {}\n",
            hit.text.split_whitespace().collect::<Vec<_>>().join(" ")
        );
        let header_cost = if header.is_empty() { 0 } else { count(header) };
        let cost = header_cost + count(&line);
        if context.tokens + cost + count("\n") > budget {
            continue;
        }
        context.rendered.push_str(header);
        context.rendered.push_str(&line);
        context.tokens += cost;
        context.overview.push(hit.id);
    }
    if !context.overview.is_empty() {
        context.rendered.push('\n');
        context.tokens += count("\n");
    }
    Ok(())
}

fn add_facts(
    read: &ReadTx,
    ns: &str,
    options: &ContextOptions,
    context: &mut Context,
) -> Result<()> {
    let count = options.count_tokens;
    let names: HashMap<NodeId, String> = context
        .items
        .iter()
        .map(|item| (item.node, item.name.clone()))
        .collect();
    let scores: HashMap<NodeId, f32> = context
        .items
        .iter()
        .map(|item| (item.node, item.score))
        .collect();

    // Facts between two recalled nodes first, then facts touching one.
    let mut seen = HashSet::new();
    let mut candidates: Vec<(bool, f32, Edge)> = Vec::new();
    for item in &context.items {
        for neighbor in read.neighbors(ns, item.node, Direction::Both, &options.facts)? {
            if !seen.insert(neighbor.edge.id) {
                continue;
            }
            // Where a memory is anchored is not a fact about it.
            if neighbor.edge.label == crate::palace::ANCHORED_AT {
                continue;
            }
            let both = names.contains_key(&neighbor.node);
            let score = item.score + scores.get(&neighbor.node).copied().unwrap_or(0.0);
            candidates.push((both, score, neighbor.edge));
        }
    }
    candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.total_cmp(&a.1))
            .then(a.2.id.cmp(&b.2.id))
    });
    candidates.truncate(options.max_facts);

    let mut other_names: HashMap<NodeId, String> = HashMap::new();
    for (_, _, edge) in candidates {
        let from = name_of(read, ns, edge.from, &names, &mut other_names)?;
        let to = name_of(read, ns, edge.to, &names, &mut other_names)?;
        // How the fact is known, when someone said.
        let known = crate::knowledge::of_edge(&edge).note();
        let known = if known.is_empty() {
            String::new()
        } else {
            format!(" [{known}]")
        };
        let line = format!("- {from} {} {to}{}{known}\n", edge.label, validity(&edge));
        // One blank line between the memories and the facts.
        let rendered = &context.rendered;
        let header = if !context.facts.is_empty() {
            ""
        } else if rendered.is_empty() || rendered.ends_with("\n\n") {
            FACTS_HEADER.trim_start()
        } else {
            FACTS_HEADER
        };
        let header_cost = if header.is_empty() { 0 } else { count(header) };
        let cost = count(&line);
        if context.tokens + header_cost + cost > options.budget {
            continue;
        }
        context.rendered.push_str(header);
        context.rendered.push_str(&line);
        context.tokens += header_cost + cost;
        context.facts.push(ContextFact {
            edge: edge.id,
            from: edge.from,
            label: edge.label,
            to: edge.to,
            text: line.trim_start_matches("- ").trim_end().to_owned(),
        });
    }
    Ok(())
}

fn name_of(
    read: &ReadTx,
    ns: &str,
    id: NodeId,
    names: &HashMap<NodeId, String>,
    cache: &mut HashMap<NodeId, String>,
) -> Result<String> {
    if let Some(name) = names.get(&id).or_else(|| cache.get(&id)) {
        return Ok(name.clone());
    }
    let name = match read.node(ns, id)? {
        Some(node) => {
            let text = if node.explicit_name().is_some() {
                String::new()
            } else {
                read.text(ns, id)?.unwrap_or_default()
            };
            readable_name(&node, &text)
        }
        None => id.to_string(),
    };
    cache.insert(id, name.clone());
    Ok(name)
}

/// The node's explicit name, or else the start of its text in quotes (an
/// unnamed memory reads better as "“Ana prefers answers…”" than by its id).
pub(crate) fn readable_name(node: &Node, text: &str) -> String {
    const NAME_CHARS: usize = 40;
    if node.explicit_name().is_some() || text.trim().is_empty() {
        return node.display_name();
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(NAME_CHARS) {
        Some((end, _)) => format!("“{}…”", text[..end].trim_end()),
        None => format!("“{text}”"),
    }
}

fn validity(edge: &Edge) -> String {
    match (edge.valid_from, edge.valid_to) {
        (None, None) => String::new(),
        (Some(from), None) => format!(" (from {})", from.date()),
        (None, Some(to)) => format!(" (until {})", to.date()),
        (Some(from), Some(to)) => format!(" ({} to {})", from.date(), to.date()),
    }
}

/// The indexed text of a node, or its properties other than the name as JSON.
fn item_body(read: &ReadTx, ns: &str, node: &Node) -> Result<String> {
    if let Some(text) = read.text(ns, node.id)? {
        return Ok(text);
    }
    let mut props = node.props.clone();
    for key in ["name", "title", "label"] {
        props.remove(key);
    }
    Ok(if props.is_empty() {
        String::new()
    } else {
        grafeno::Value::Object(props).to_string()
    })
}

fn render_item(number: usize, name: &str, labels: &str, body: &str) -> String {
    let heading = if name.is_empty() {
        format!("[{number}] ({labels})")
    } else {
        format!("[{number}] {name} ({labels})")
    };
    if body.is_empty() {
        format!("{heading}\n")
    } else {
        format!("{heading}\n{body}\n\n")
    }
}

/// The longest prefix of `text`, plus an ellipsis, within `max_tokens`.
fn truncate(text: &str, max_tokens: usize, count: fn(&str) -> usize) -> String {
    let ends: Vec<usize> = text
        .char_indices()
        .map(|(at, c)| at + c.len_utf8())
        .collect();
    let fits = |chars: usize| {
        let end = if chars == 0 { 0 } else { ends[chars - 1] };
        count(&format!("{}{ELLIPSIS}", &text[..end])) <= max_tokens
    };
    let (mut low, mut high) = (0, ends.len());
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let end = if low == 0 { 0 } else { ends[low - 1] };
    format!("{}{ELLIPSIS}", text[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use super::estimate_tokens;

    /// The old estimate, four characters a token.
    fn by_four(text: &str) -> usize {
        text.chars().count().div_ceil(4)
    }

    #[test]
    fn prose_costs_about_a_token_a_short_word() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("the cat sat on the mat"), 6);
        let prose = "Ownership rules and the borrow checker";
        assert_eq!(estimate_tokens(prose), 11);
        assert!(estimate_tokens(prose) >= by_four(prose));
    }

    #[test]
    fn code_data_and_other_languages_count_higher_than_four_characters() {
        for text in [
            "fn main() {\n    println!(\"{}\", x[0]);\n}",
            r#"{"id": 42, "tags": ["a", "b"], "ok": true}"#,
            "La decisión sobre la comunicación entre servicios",
            "知识图谱",
            "2026-11-12T10:30:00Z",
        ] {
            assert!(
                estimate_tokens(text) > by_four(text),
                "{text:?}: {} against {}",
                estimate_tokens(text),
                by_four(text)
            );
        }
        assert_eq!(estimate_tokens("知识图谱"), 4);
    }

    #[test]
    fn whitespace_costs_only_line_breaks_and_indentation() {
        assert_eq!(estimate_tokens("one two"), 2);
        assert_eq!(estimate_tokens("one\ntwo"), 3);
        assert_eq!(estimate_tokens("one\n        two"), 3);
        assert_eq!(estimate_tokens("one  two"), 3);
    }

    #[test]
    fn the_count_only_grows_as_text_is_added() {
        // Truncation searches for the longest prefix that fits.
        let text = "Ana's team: {\"budget\": 1200}\n    décision → 知识 ok";
        let mut last = 0;
        for (at, c) in text.char_indices() {
            let count = estimate_tokens(&text[..at + c.len_utf8()]);
            assert!(count >= last, "{:?}", &text[..at + c.len_utf8()]);
            last = count;
        }
    }
}
