//! Grafeno: a graph engine for agent memory and knowledge.
//!
//! ```no_run
//! use grafeno::{Direction, EdgeFilter, Grafeno, NewEdge, NewNode};
//!
//! let db = Grafeno::open("memory.redb")?;
//! let (ana, rust) = db.write(|tx| {
//!     let ana = tx.create_node("vault", NewNode::new("Person").prop("name", "Ana"))?;
//!     let rust = tx.create_node(
//!         "vault",
//!         NewNode::new("Topic").prop("name", "Rust").text("Rust is a systems language"),
//!     )?;
//!     tx.create_edge("vault", NewEdge::new(ana, "knows_about", rust))?;
//!     Ok((ana, rust))
//! })?;
//!
//! let read = db.begin_read()?;
//! let topics = read.neighbors("vault", ana, Direction::Out, &EdgeFilter::any())?;
//! assert_eq!(topics[0].node, rust);
//! assert_eq!(read.search_text("vault", "systems", 5)?[0].node, rust);
//! # Ok::<(), grafeno::Error>(())
//! ```

mod access;
pub mod algo;
mod alias;
mod audit;
mod backup;
mod changes;
mod check;
mod compress;
mod constraints;
mod db;
mod dump;
mod find;
mod fulltext;
mod hyperedge;
mod hyperedge_codec;
mod index;
mod indexing;
mod interchange;
mod memory;
mod merge;
mod migrations;
mod namespace;
mod planstats;
mod replication;
mod resolve;
mod rights;
mod view;
mod vindex;
pub use alias::Alias;
pub use audit::{AuditEvent, AuditPage, AuditRetention, NewAuditEvent, Outcome};
pub use db::Origin;
pub use view::View;
mod state;
mod stats;
mod store;
mod subscriptions;
mod text;
pub mod textsim;
mod traverse;
mod usage;
mod vector;
mod writer;

pub use access::{
    Account, AccountKind, ApiKey, Grant, KeyScope, Principal, Role, Session, check_principal_name,
};
pub use backup::{BackupStats, Vacuumed};
pub use changes::{Change, ChangeOp, ChangePage, Part, Retention};
pub use check::{CheckReport, CompactReport, RebuildReport, VectorHealth};
pub use compress::{Compression, DictionaryInfo, StoredKind, TrainOptions};
pub use constraints::{
    Action, Constraint, ConstraintEvent, HistoryEntry, OnConflict, PropertyType, Scope,
};
pub use db::{
    Actor, BASE_LAYOUT, DEFAULT_CACHE, Durability, FileIntegrity, Grafeno, HYPEREDGE_LAYOUT,
    OpenOptions, ReadTx, SCHEMA_VERSION, WriteTx, upgrade_copies, upgrade_copy_path,
};
pub use dump::{DumpStats, EXPORT_FORMAT, EXPORT_VERSION, ImportOptions, Record};
pub use find::{Cmp, Filter, Find, FindPage, IndexPosition, MAX_FIND_LIMIT, OrderBy, ValueKind};
pub use fulltext::{
    Analyzer, Fragment, Fulltext, FulltextHit, Fusion, SearchHit, SearchMode, SearchRequest, fuse,
};
pub use grafeno_core::*;
pub use hyperedge::{DEFAULT_MAX_PARTICIPANTS, HyperEdgeFilter, OnHyperEdges, ParticipantMatch};
pub use index::{FuzzyHit, IndexDef, IndexInfo, IndexRange, RecordKind};
pub use indexing::{
    IndexKind, IndexOrigin, IndexingMode, IndexingPolicy, LabelRule, MAX_AUTO_PER_LABEL,
    MAX_EXACT_CHARS,
};
pub use interchange::{IMPORT_ID, InterchangeOptions, InterchangeStats};
pub use memory::{DEFAULT_IMPORTANCE, MemoryState, names_of, normalize_name};
pub use merge::{
    Conflict, ConflictKind, DEFAULT_STRONG_KEYS, MergeOptions, MergeRecord, MergeReport,
    NodeSnapshot, node_conflicts,
};
pub use migrations::{
    Encoding, Migration, MigrationOptions, MigrationState, MigrationStep, NamespaceDigest, Pending,
    RunControl, StepKind,
};
pub use namespace::{DropStats, MAX_NAMESPACE_LENGTH, RESERVED, check_new_namespace};
pub use planstats::{BUCKETS, COMMON_VALUES, EdgeLabelStats, IndexStats, PlannerStats};
pub use replication::{Bulk, ENTRY_FORMAT, Entry, FENCE, LogPosition, Op};
pub use resolve::{Decision, FieldRule, FieldScore, Match, MatchRule, Probe, SCAN_LIMIT, score};
pub use rights::{
    Act, Applies, BUILT_IN_ROLES, Condition, Holder, Object, PUBLIC, Policy, PolicyValue,
    Privilege, Revoke, Rights, RoleDef, check_role_name, level_actions, level_name, level_of,
};
pub use stats::{NamespaceInfo, NamespaceStats};
pub use subscriptions::{ChangeValues, Delivery, EventTarget, Subscription};
pub use text::{Language, tokenize, tokenize_in};
pub use traverse::{Direction, EdgeFilter, Neighbor, Path, Subgraph};
pub use usage::{NamespaceUsage, StorageUsage, TableUsage};
pub use vector::Similarity;
pub use vindex::{BuildProgress, Building, EMBEDDING_KEY, VectorFilter, VectorSearch, VectorSpec};
pub use writer::{Histogram, HistogramSnapshot, Lane, Writer, WriterOptions, WriterSnapshot};

/// A search result: a node and its score (BM25 for text, cosine similarity
/// for vectors). Higher is better.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct Hit {
    pub node: NodeId,
    pub score: f32,
}
