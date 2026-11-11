use std::collections::HashMap;

use grafeno::{Direction, EdgeFilter, Fusion, Node, NodeId, ReadTx, Result, SearchMode, Timestamp};
use serde::Serialize;

/// How recall spreads relevance through the graph.
#[derive(Clone, Debug)]
pub struct Expansion {
    /// Hops to follow from each seed.
    pub depth: usize,
    pub direction: Direction,
    pub filter: EdgeFilter,
    /// Share of a seed's score passed on per hop, from 0 to 1.
    pub decay: f32,
    /// How many of the best direct matches are expanded.
    pub seeds: usize,
}

impl Default for Expansion {
    fn default() -> Self {
        Self {
            depth: 1,
            direction: Direction::Both,
            filter: EdgeFilter::any(),
            decay: 0.5,
            seeds: 5,
        }
    }
}

/// The label that archived memories get: recall leaves them out by default.
pub const ARCHIVED: &str = "Archived";

/// How recall weighs a memory's importance and recency against its
/// relevance: `score × (1 + importance_weight × importance + recency_weight
/// × recency)`, where recency halves every `half_life_secs` since the last
/// access. With both weights at 0, relevance alone decides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ranking {
    pub importance_weight: f32,
    pub recency_weight: f32,
    pub half_life_secs: f64,
}

impl Default for Ranking {
    fn default() -> Self {
        Self {
            importance_weight: 0.5,
            recency_weight: 0.3,
            half_life_secs: 30.0 * 86_400.0,
        }
    }
}

/// What to recall. At least one of `text` and `embedding` should be set;
/// without them nothing matches.
#[derive(Clone, Debug)]
pub struct RecallQuery {
    pub text: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub limit: usize,
    /// Keep only nodes with at least one of these labels (empty: any label).
    pub labels: Vec<String>,
    /// Candidates taken from each index before fusing them.
    pub candidates: usize,
    /// Graph expansion; `None` disables it.
    pub expand: Option<Expansion>,
    /// Importance and recency weighting; `None` ranks by relevance alone.
    pub ranking: Option<Ranking>,
    /// Also return archived memories (label [`ARCHIVED`]).
    pub include_archived: bool,
    /// A full-text index for the text, with its query language (16.6);
    /// without one, the node text, word by word.
    pub index: Option<String>,
    /// Which of the text and the embedding to use: both when `None`, as
    /// given.
    pub mode: Option<SearchMode>,
    /// How the text and vector lists are put together: rank fusion with
    /// k = 60 by default.
    pub fusion: Fusion,
}

impl Default for RecallQuery {
    fn default() -> Self {
        Self {
            text: None,
            embedding: None,
            limit: 10,
            labels: Vec::new(),
            candidates: 50,
            expand: Some(Expansion::default()),
            ranking: Some(Ranking::default()),
            include_archived: false,
            index: None,
            mode: None,
            fusion: Fusion::default(),
        }
    }
}

impl RecallQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    pub fn embedding(mut self, embedding: impl Into<Vec<f32>>) -> Self {
        self.embedding = Some(embedding.into());
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.labels.push(label.into());
        self
    }

    pub fn expand(mut self, expansion: Option<Expansion>) -> Self {
        self.expand = expansion;
        self
    }
}

/// Why a node was recalled. Ranks start at 1.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Signal {
    Text {
        rank: usize,
        score: f32,
    },
    Vector {
        rank: usize,
        similarity: f32,
    },
    Graph {
        seed: NodeId,
        hops: usize,
    },
    /// The boost from the memory's importance and recency (0 to 1 each).
    Memory {
        importance: f32,
        recency: f32,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Recalled {
    pub node: Node,
    /// Fused score: comparable within one recall, not across queries.
    pub score: f32,
    pub signals: Vec<Signal>,
}

/// Hybrid recall: text and vector search fused (by rank, or by weighted
/// scores), then graph expansion.
pub fn recall(read: &ReadTx, ns: &str, query: &RecallQuery) -> Result<Vec<Recalled>> {
    let mut scores: HashMap<NodeId, (f32, Vec<Signal>)> = HashMap::new();
    let use_text = query.mode != Some(SearchMode::Vector);
    let use_vector = query.mode != Some(SearchMode::Text);
    let mut text_hits: Vec<(NodeId, f32)> = Vec::new();
    if let Some(text) = query.text.as_ref().filter(|_| use_text) {
        text_hits = match &query.index {
            Some(index) => read
                .fulltext_search(ns, index, text, query.candidates, false)?
                .into_iter()
                .map(|hit| (hit.node, hit.score))
                .collect(),
            None => read
                .search_text(ns, text, query.candidates)?
                .into_iter()
                .map(|hit| (hit.node, hit.score))
                .collect(),
        };
    }
    let mut vector_hits: Vec<(NodeId, f32)> = Vec::new();
    if let Some(embedding) = query.embedding.as_ref().filter(|_| use_vector) {
        vector_hits = read
            .search_vector(ns, embedding, query.candidates)?
            .into_iter()
            .map(|hit| (hit.node, hit.score))
            .collect();
    }
    for (index, (node, score)) in text_hits.iter().enumerate() {
        scores.entry(*node).or_default().1.push(Signal::Text {
            rank: index + 1,
            score: *score,
        });
    }
    for (index, (node, similarity)) in vector_hits.iter().enumerate() {
        scores.entry(*node).or_default().1.push(Signal::Vector {
            rank: index + 1,
            similarity: *similarity,
        });
    }
    // Both lists fused as asked, even when one is empty: a score means the
    // same whether or not the other list found anything.
    for (node, score) in grafeno::fuse(&text_hits, &vector_hits, SearchMode::Hybrid, query.fusion) {
        scores.entry(node).or_default().0 = score;
    }

    if let Some(expansion) = &query.expand {
        let mut seeds: Vec<(NodeId, f32)> = scores
            .iter()
            .map(|(id, (score, _))| (*id, *score))
            .collect();
        sort_by_score(&mut seeds);
        seeds.truncate(expansion.seeds);
        for (seed, seed_score) in seeds {
            for (node, hops) in read.k_hop(
                ns,
                seed,
                expansion.depth,
                expansion.direction,
                &expansion.filter,
            )? {
                let entry = scores.entry(node).or_default();
                entry.0 += seed_score * expansion.decay.powi(hops as i32);
                entry.1.push(Signal::Graph { seed, hops });
            }
        }
    }

    let mut ranked: Vec<(NodeId, f32)> = scores
        .iter()
        .map(|(id, (score, _))| (*id, *score))
        .collect();
    sort_by_score(&mut ranked);
    // The memory boost can reorder results (at most by its maximum factor),
    // so it is applied to a pool larger than the limit.
    let pool = if query.ranking.is_some() {
        query.limit.saturating_mul(5).max(100)
    } else {
        query.limit
    };
    let now = Timestamp::now();
    let mut out = Vec::new();
    for (id, mut score) in ranked {
        if out.len() >= pool {
            break;
        }
        let Some(node) = read.node(ns, id)? else {
            continue;
        };
        if !query.labels.is_empty() && !query.labels.iter().any(|label| node.has_label(label)) {
            continue;
        }
        // Community summaries answer broad questions (`overview`); recall
        // returns them only when asked for by label.
        if query.labels.is_empty() && node.has_label(crate::communities::COMMUNITY) {
            continue;
        }
        if !query.include_archived && node.has_label(ARCHIVED) {
            continue;
        }
        let mut signals = scores
            .remove(&id)
            .map(|(_, signals)| signals)
            .unwrap_or_default();
        if let Some(ranking) = &query.ranking {
            let state = read.memory(ns, &node)?;
            let recency = state.recency(now, ranking.half_life_secs);
            score *= 1.0
                + ranking.importance_weight * state.importance
                + ranking.recency_weight * recency;
            signals.push(Signal::Memory {
                importance: state.importance,
                recency,
            });
        }
        out.push(Recalled {
            node,
            score,
            signals,
        });
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.id.cmp(&b.node.id)));
    out.truncate(query.limit);
    Ok(out)
}

/// Best first; ties broken by id so results are deterministic.
fn sort_by_score(items: &mut [(NodeId, f32)]) {
    items.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
}
