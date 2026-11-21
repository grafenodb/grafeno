use crate::{EdgeId, HyperEdgeId, NodeId};

#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    #[error("node {0} not found")]
    NodeNotFound(NodeId),
    #[error("edge {0} not found")]
    EdgeNotFound(EdgeId),
    #[error("hyperedge {0} not found")]
    HyperEdgeNotFound(HyperEdgeId),
    #[error("namespace {0:?} not found")]
    NamespaceNotFound(String),
    /// The principal may not do this.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// What should be created exists already.
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    /// Too much work is waiting: try again later.
    #[error("busy: {0}")]
    Busy(String),
}

pub type Result<T> = std::result::Result<T, Error>;
