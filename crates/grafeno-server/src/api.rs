//! Request types shared by the HTTP and MCP servers, and how they map to the
//! engine.

use std::collections::HashMap;

use grafeno::MemoryState;
use grafeno::{
    Direction, EdgeFilter, Error, NewEdge, NewNode, Node, NodeId, Props, ReadTx, Result, Timestamp,
};
use grafeno_memory::{ContextOptions, Expansion, Ranking, RecallQuery};
use serde::{Deserialize, Serialize};

/// A point in time: microseconds since the Unix epoch, or an ISO 8601 string.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum TimeInput {
    Micros(i64),
    Iso(String),
}

impl TimeInput {
    pub fn resolve(&self) -> Result<Timestamp> {
        match self {
            Self::Micros(micros) => Ok(Timestamp::from_micros(*micros)),
            Self::Iso(text) => Timestamp::parse(text),
        }
    }
}

fn resolve_time(time: &Option<TimeInput>) -> Result<Option<Timestamp>> {
    time.as_ref().map(TimeInput::resolve).transpose()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInput {
    /// Only used by upserts: the client's own id.
    #[serde(default)]
    pub id: Option<NodeId>,
    pub labels: Vec<String>,
    #[serde(default)]
    pub props: Props,
    #[serde(default)]
    pub sources: Vec<NodeId>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
}

impl NodeInput {
    pub fn into_new(self) -> NewNode {
        NewNode {
            labels: self.labels,
            props: self.props,
            sources: self.sources,
            text: self.text,
            embedding: self.embedding,
        }
    }
}

/// A node id, or (in a batch) a `$name` given to a node created earlier in it.
pub type NodeRef = String;

pub fn resolve_ref(reference: &str, refs: &HashMap<String, NodeId>) -> Result<NodeId> {
    if reference.starts_with('$') {
        return refs
            .get(reference)
            .copied()
            .ok_or_else(|| Error::Invalid(format!("unknown reference {reference:?}")));
    }
    parse_id(reference)
}

pub fn parse_id(text: &str) -> Result<NodeId> {
    text.parse::<uuid::Uuid>()
        .map(NodeId::from)
        .map_err(|_| Error::Invalid(format!("invalid id {text:?}")))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeInput {
    pub from: NodeRef,
    pub label: String,
    pub to: NodeRef,
    #[serde(default)]
    pub props: Props,
    #[serde(default)]
    pub sources: Vec<NodeId>,
    #[serde(default)]
    pub valid_from: Option<TimeInput>,
    #[serde(default)]
    pub valid_to: Option<TimeInput>,
}

impl EdgeInput {
    pub fn into_new(self, refs: &HashMap<String, NodeId>) -> Result<NewEdge> {
        Ok(NewEdge {
            from: resolve_ref(&self.from, refs)?,
            label: self.label,
            to: resolve_ref(&self.to, refs)?,
            props: self.props,
            sources: self.sources,
            valid_from: resolve_time(&self.valid_from)?,
            valid_to: resolve_time(&self.valid_to)?,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterInput {
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub valid_at: Option<TimeInput>,
    #[serde(default)]
    pub known_at: Option<TimeInput>,
}

impl FilterInput {
    pub fn resolve(&self) -> Result<EdgeFilter> {
        let mut filter = EdgeFilter::any();
        for label in &self.labels {
            filter = filter.label(label);
        }
        if let Some(at) = resolve_time(&self.valid_at)? {
            filter = filter.valid_at(at);
        }
        if let Some(at) = resolve_time(&self.known_at)? {
            filter = filter.known_at(at);
        }
        // Traversals see what the caller may (14.4): this runs in the
        // request, where the caller is known.
        Ok(filter.with_view(crate::auth::current().view()))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpandInput {
    #[serde(default = "default_depth")]
    pub depth: usize,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub filter: FilterInput,
    #[serde(default = "default_decay")]
    pub decay: f32,
    #[serde(default = "default_seeds")]
    pub seeds: usize,
}

fn default_depth() -> usize {
    1
}

fn default_decay() -> f32 {
    0.5
}

fn default_seeds() -> usize {
    5
}

/// `true` (the default) expands with default options, `false` disables it.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ExpandSetting {
    Enabled(bool),
    Options(ExpandInput),
}

impl Default for ExpandSetting {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

/// `deny_unknown_fields` is left out here and in [`ContextInput`]: serde
/// does not support it together with `flatten`.
#[derive(Clone, Debug, Deserialize)]
pub struct RecallInput {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "default_candidates")]
    pub candidates: usize,
    #[serde(default)]
    pub expand: ExpandSetting,
    /// Also return archived memories.
    #[serde(default)]
    pub include_archived: bool,
    /// `true` (default) weighs importance and recency, `false` ranks by
    /// relevance alone, or `{importance_weight, recency_weight, half_life_days}`.
    #[serde(default)]
    pub ranking: RankingSetting,
    /// Record an access to each returned node (for decay).
    #[serde(default)]
    pub touch: bool,
    /// A full-text index for `text`, with its query language (16.6).
    #[serde(default)]
    pub index: Option<String>,
    /// `text`, `vector` or `hybrid` (the default uses what is given).
    #[serde(default)]
    pub mode: Option<grafeno::SearchMode>,
    /// `rank` (default, with `k`) or `weighted` (with `text_weight`).
    #[serde(default)]
    pub fusion: Option<String>,
    #[serde(default)]
    pub k: Option<f32>,
    #[serde(default)]
    pub text_weight: Option<f32>,
}

/// The fusion a request names.
pub fn fusion_of(
    name: Option<&str>,
    k: Option<f32>,
    text_weight: Option<f32>,
) -> Result<grafeno::Fusion> {
    match name {
        None | Some("rank") => Ok(grafeno::Fusion::Rank {
            k: k.unwrap_or(60.0),
        }),
        Some("weighted") => Ok(grafeno::Fusion::Weighted {
            text: text_weight.unwrap_or(0.5),
        }),
        Some(other) => Err(Error::Invalid(format!(
            "unknown fusion {other:?}: rank or weighted"
        ))),
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankingInput {
    #[serde(default)]
    pub importance_weight: Option<f32>,
    #[serde(default)]
    pub recency_weight: Option<f32>,
    #[serde(default)]
    pub half_life_days: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum RankingSetting {
    Enabled(bool),
    Options(RankingInput),
}

impl Default for RankingSetting {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

impl RankingSetting {
    pub fn resolve(&self) -> Option<Ranking> {
        let base = Ranking::default();
        match self {
            Self::Enabled(false) => None,
            Self::Enabled(true) => Some(base),
            Self::Options(options) => Some(Ranking {
                importance_weight: options.importance_weight.unwrap_or(base.importance_weight),
                recency_weight: options.recency_weight.unwrap_or(base.recency_weight),
                half_life_secs: options
                    .half_life_days
                    .map_or(base.half_life_secs, |days| days * 86_400.0),
            }),
        }
    }
}

fn default_limit() -> usize {
    10
}

fn default_candidates() -> usize {
    50
}

impl RecallInput {
    pub fn resolve(&self) -> Result<RecallQuery> {
        if self.text.is_none() && self.embedding.is_none() {
            return Err(Error::Invalid("recall needs `text` or `embedding`".into()));
        }
        let expand = match &self.expand {
            ExpandSetting::Enabled(false) => None,
            ExpandSetting::Enabled(true) => Some(Expansion::default()),
            ExpandSetting::Options(options) => Some(Expansion {
                depth: options.depth,
                direction: options.direction,
                filter: options.filter.resolve()?,
                decay: options.decay,
                seeds: options.seeds,
            }),
        };
        Ok(RecallQuery {
            text: self.text.clone(),
            embedding: self.embedding.clone(),
            limit: self.limit,
            labels: self.labels.clone(),
            candidates: self.candidates,
            expand,
            ranking: self.ranking.resolve(),
            include_archived: self.include_archived,
            index: self.index.clone(),
            mode: self.mode,
            fusion: fusion_of(self.fusion.as_deref(), self.k, self.text_weight)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ContextInput {
    #[serde(flatten)]
    pub query: RecallInput,
    #[serde(default = "default_budget")]
    pub budget: usize,
    #[serde(default = "default_fact_share")]
    pub fact_share: f32,
    #[serde(default = "default_max_facts")]
    pub max_facts: usize,
    #[serde(default)]
    pub facts: FilterInput,
    /// Community summaries that match, put first (0 for none).
    #[serde(default)]
    pub communities: usize,
}

fn default_budget() -> usize {
    2_000
}

fn default_fact_share() -> f32 {
    0.25
}

fn default_max_facts() -> usize {
    50
}

impl ContextInput {
    pub fn resolve(&self) -> Result<(RecallQuery, ContextOptions)> {
        let query = self.query.resolve()?;
        let mut options = ContextOptions::with_budget(self.budget);
        options.fact_share = self.fact_share;
        options.max_facts = self.max_facts;
        options.facts = self.facts.resolve()?;
        options.communities = self.communities;
        Ok((query, options))
    }
}

/// A node with its indexed text.
#[derive(Clone, Debug, Serialize)]
pub struct NodeOutput {
    #[serde(flatten)]
    pub node: Node,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub memory: MemoryState,
}

impl NodeOutput {
    pub fn load(read: &ReadTx, ns: &str, node: Node) -> Result<Self> {
        let text = read.text(ns, node.id)?;
        let memory = read.memory(ns, &node)?;
        Ok(Self { node, text, memory })
    }
}

/// An error with the HTTP status it maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    BadRequest,
    NotFound,
    Internal,
    /// Too much work waiting: the client should retry.
    Busy,
    /// What should be created exists already.
    Conflict,
    /// The principal may not do this.
    Forbidden,
}

pub fn error_kind(error: &Error) -> ErrorKind {
    match error {
        Error::NodeNotFound(_) | Error::EdgeNotFound(_) | Error::NamespaceNotFound(_) => {
            ErrorKind::NotFound
        }
        Error::Conflict(_) => ErrorKind::Conflict,
        Error::Forbidden(_) => ErrorKind::Forbidden,
        Error::Invalid(_) => ErrorKind::BadRequest,
        Error::Storage(_) | Error::Serialization(_) => ErrorKind::Internal,
        Error::Busy(_) => ErrorKind::Busy,
    }
}
