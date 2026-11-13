//! Agent memory on top of Grafeno: hybrid recall and token-budgeted context.
//!
//! [`recall`] fuses full-text (BM25) and vector results with reciprocal rank
//! fusion, then spreads part of each top result's score to its graph
//! neighbors. [`context_for`] packs the recalled nodes, and the facts that
//! connect them, into a prompt-ready text that fits a token budget.
//! [`refresh_communities`] summarizes the communities of the graph, at
//! every level, without a model; [`overview`] searches those summaries
//! for broad questions. A [`palace`] arranges memories into rooms and
//! places: [`walk`] goes from a topic to the memories around it, and
//! [`enter`] brings back a room's context and where its work was left.
//!
//! ```no_run
//! use grafeno::{Grafeno, NewNode};
//! use grafeno_memory::{ContextOptions, RecallQuery, context_for};
//!
//! let db = Grafeno::open("memory.redb")?;
//! db.write(|tx| {
//!     tx.create_node("agent", NewNode::new("Fact").text("The user prefers short answers"))
//! })?;
//!
//! let read = db.begin_read()?;
//! let query = RecallQuery::new().text("how should I answer the user?");
//! let context = context_for(&read, "agent", &query, &ContextOptions::with_budget(500))?;
//! println!("{}", context.rendered);
//! # Ok::<(), grafeno::Error>(())
//! ```

pub mod checkpoint;
pub mod communities;
mod context;
pub mod documents;
pub mod extract;
mod maintenance;
pub mod palace;
mod recall;
mod reconcile;
pub mod timeline;

pub use checkpoint::{
    CHECKPOINT, Checkpoint, NewCheckpoint, ResumeQuery, checkpoints, resume, save_checkpoint, tasks,
};
pub use communities::{
    COMMUNITY, CommunityHit, CommunityMembers, CommunityOptions, CommunityPlan, CommunityReport,
    MEMBER_OF, OverviewOptions, PART_OF, apply_communities, community_members, overview,
    plan_communities, refresh_communities, set_community_text,
};
pub use context::{
    Context, ContextFact, ContextItem, ContextOptions, context_for, context_of, estimate_tokens,
};
pub use documents::{
    CHUNK, DEFAULT_CHUNK_CHARS, Ingested, Kind, NewDocument, Piece, cut, ingest_document,
};
pub use maintenance::{
    ConsolidationGroup, ConsolidationOptions, DISTINCT_FROM, DecayOptions, Duplicate,
    DuplicateOptions, Summary, WeakMemory, archive, consolidation_groups, find_duplicates,
    weak_memories, write_summary,
};
pub use palace::{
    ANCHORED_AT, Anchored, AnchoredMemory, Created, EnterOptions, Entered, Hop, LOCUS, LocusMap,
    MapOptions, NewRoom, PALACE, PalaceMap, Place, ROOM, Reached, RoomMap, Walk, WalkPlace,
    WalkQuery, anchor, anchors_of, create_locus, create_palace, create_room, enter, find_locus,
    find_palace, find_room, map_palace, map_room, palaces, unanchor, walk,
};
pub use recall::{ARCHIVED, Expansion, Ranking, RecallQuery, Recalled, Signal, recall};
pub use reconcile::{
    AgentMention, ExtractOptions, ExtractReport, MENTIONS, Occurrence, Plan, Resolution,
    ReviewItem, SOURCE_LABELS, extract, extract_batch, ingest, plan, resolve, review_queue,
};
pub use timeline::{Moment, timeline};
