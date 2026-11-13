use std::collections::{HashMap, HashSet, VecDeque};

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
    /// How much well-connected nodes are damped, from 0 (not at all) up.
    /// The share passed along an edge is divided by
    /// `sqrt((1 + hubs × ln d₁) × (1 + hubs × ln d₂))`, where d₁ and d₂ are
    /// the edges of its two ends (counted up to 1 024): a node with one
    /// edge passes and receives the whole share, one with 1 000 edges about
    /// a third at `hubs` 1. A node linked to everything (the user, the
    /// current project, a common tag) then no longer ranks as relevant to
    /// everything.
    pub hubs: f32,
    /// Neighbors followed from each node at most (0: all): those the
    /// search found first, best first, then the newest.
    pub fanout: usize,
}

impl Default for Expansion {
    fn default() -> Self {
        Self {
            depth: 1,
            direction: Direction::Both,
            filter: EdgeFilter::any(),
            decay: 0.5,
            seeds: 5,
            hubs: 1.0,
            fanout: 100,
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
    /// A vector index for the embedding (16.7); without one, the node
    /// embeddings.
    pub vector_index: Option<String>,
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
            vector_index: None,
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
        let found = match &query.vector_index {
            Some(index) => read.vector_search(
                ns,
                embedding,
                &grafeno::VectorSearch {
                    index: Some(index.clone()),
                    limit: query.candidates,
                    ..grafeno::VectorSearch::default()
                },
            )?,
            None => read.search_vector(ns, embedding, query.candidates)?,
        };
        vector_hits = found.into_iter().map(|hit| (hit.node, hit.score)).collect();
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

    // A palace's rooms and loci arrange memories and are not memories:
    // unless asked for by label, they are neither returned nor a way from
    // one memory to another, so recall answers the same with a palace as
    // without one.
    let hidden = if query
        .labels
        .iter()
        .any(|label| crate::palace::STRUCTURE_LABELS.contains(&label.as_str()))
    {
        HashSet::new()
    } else {
        crate::palace::structure(read, ns)?
    };
    scores.retain(|id, _| !hidden.contains(id));

    if let Some(expansion) = &query.expand {
        let direct: HashMap<NodeId, f32> = scores
            .iter()
            .map(|(id, (score, _))| (*id, *score))
            .collect();
        let mut seeds: Vec<(NodeId, f32)> =
            direct.iter().map(|(id, score)| (*id, *score)).collect();
        sort_by_score(&mut seeds);
        seeds.truncate(expansion.seeds);
        let mut degrees = HashMap::new();
        // What a node receives from several seeds adds up: being near
        // several matches is a reason to rank it, and damping keeps a hub
        // near all of them from collecting more than its share.
        for (seed, seed_score) in seeds {
            for (node, hops, share) in
                spread(read, ns, seed, expansion, &direct, &hidden, &mut degrees)?
            {
                let entry = scores.entry(node).or_default();
                entry.0 += seed_score * share;
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

/// The nodes within `expansion.depth` hops of `seed`, in BFS order, each
/// with its distance and the share of the seed's score it receives (along
/// the first path found): `decay` per hop, damped at both ends of each edge
/// by their number of edges (see [`Expansion::hubs`]).
fn spread(
    read: &ReadTx,
    ns: &str,
    seed: NodeId,
    expansion: &Expansion,
    direct: &HashMap<NodeId, f32>,
    hidden: &HashSet<NodeId>,
    degrees: &mut HashMap<NodeId, usize>,
) -> Result<Vec<(NodeId, usize, f32)>> {
    // Negative (or NaN) would amplify hubs: read as 0.
    let hubs = expansion.hubs.max(0.0);
    let damping = |degree: usize| 1.0 + hubs * (degree.max(1) as f32).ln();
    let mut reached = HashMap::from([(seed, (0usize, 1.0f32))]);
    let mut queue = VecDeque::from([seed]);
    let mut out = Vec::new();
    while let Some(current) = queue.pop_front() {
        let (hops, share) = reached[&current];
        if hops >= expansion.depth {
            continue;
        }
        let mut entries = read.neighbor_ids(ns, current, expansion.direction, &expansion.filter)?;
        entries.retain(|(_, other)| !hidden.contains(other));
        degrees.insert(current, entries.len().min(DEGREE_CAP));
        let mut seen = HashSet::new();
        let mut next: Vec<NodeId> = Vec::new();
        for (_, other) in entries {
            if !reached.contains_key(&other) && seen.insert(other) {
                next.push(other);
            }
        }
        if expansion.fanout > 0 && next.len() > expansion.fanout {
            next.sort_by(|a, b| {
                let score = |id| direct.get(id).copied().unwrap_or(f32::NEG_INFINITY);
                score(b).total_cmp(&score(a)).then(b.cmp(a))
            });
            next.truncate(expansion.fanout);
        }
        let from = damping(degrees[&current]);
        for node in next {
            let to = if hubs > 0.0 {
                damping(degree(read, ns, node, expansion, hidden, degrees)?)
            } else {
                1.0
            };
            let passed = share * expansion.decay / (from * to).sqrt();
            reached.insert(node, (hops + 1, passed));
            out.push((node, hops + 1, passed));
            queue.push_back(node);
        }
    }
    Ok(out)
}

/// Damping counts a node's edges up to this many: past it, a node is a hub
/// either way, and counting the rest would read all of them.
const DEGREE_CAP: usize = 1_024;

/// The edges of `node` that expansion follows (up to [`DEGREE_CAP`]),
/// counted once per recall.
fn degree(
    read: &ReadTx,
    ns: &str,
    node: NodeId,
    expansion: &Expansion,
    hidden: &HashSet<NodeId>,
    degrees: &mut HashMap<NodeId, usize>,
) -> Result<usize> {
    if let Some(degree) = degrees.get(&node) {
        return Ok(*degree);
    }
    let degree = if hidden.is_empty() {
        read.degree(ns, node, expansion.direction, &expansion.filter, DEGREE_CAP)?
    } else {
        // Anchors do not make a memory a hub: its edges to a palace are
        // not counted.
        read.neighbor_ids(ns, node, expansion.direction, &expansion.filter)?
            .iter()
            .filter(|(_, other)| !hidden.contains(other))
            .count()
            .min(DEGREE_CAP)
    };
    degrees.insert(node, degree);
    Ok(degree)
}

/// Best first; ties broken by id so results are deterministic.
fn sort_by_score(items: &mut [(NodeId, f32)]) {
    items.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
}
