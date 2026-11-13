//! Hierarchical community summaries, without a model.
//!
//! [`refresh_communities`] runs Louvain over a namespace and keeps every
//! level of aggregation: communities, communities of communities, up to the
//! coarsest. Each community gets a `Community` node that summarizes it from
//! the graph alone: its most central members, its distinctive terms (TF-IDF
//! against the namespace), its label mix, its key facts and its time span.
//! Members link to the lowest summary that holds them (`member_of`), and a
//! summary to its parent (`part_of`). A refresh matches the new communities
//! to the stored ones by their members and rewrites only those that
//! changed. An agent may replace a summary's text with its own
//! ([`set_community_text`]); a refresh keeps that text and marks it stale
//! when the community changes.
//!
//! [`overview`] is the global search for broad questions: the summaries
//! that match a query (or the top of the hierarchy), and
//! [`community_members`] drills down into one.

use std::collections::{BTreeMap, HashMap, HashSet};

use grafeno::algo::{Louvain, Projection, louvain_levels, never};
use grafeno::{
    Direction, EdgeFilter, EdgeId, Grafeno, NewEdge, NewNode, Node, NodeId, Props, ReadTx, Result,
    Timestamp, Value, WriteTx, tokenize,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::recall::ARCHIVED;

/// The label of community summaries.
pub const COMMUNITY: &str = "Community";
/// A member to the lowest summary of a community that holds it.
pub const MEMBER_OF: &str = "member_of";
/// A summary to the summary of its parent community.
pub const PART_OF: &str = "part_of";

/// What [`refresh_communities`] summarizes, and how much of it.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommunityOptions {
    /// Nodes with any of these labels; every node when empty (summaries
    /// and archived memories are always left out).
    pub labels: Vec<String>,
    /// Edges with any of these labels; every current fact when empty.
    pub edge_labels: Vec<String>,
    /// The edge property that weighs each edge (1 when missing).
    pub weight: Option<String>,
    /// Louvain resolution: above 1 favors smaller communities.
    pub resolution: f64,
    /// At most this many levels.
    pub max_levels: usize,
    /// Smaller communities get no summary of their own (their members
    /// belong to the summary of the level above).
    pub min_size: usize,
    /// Central members named in a summary.
    pub central: usize,
    /// Distinctive terms in a summary.
    pub terms: usize,
    /// Key facts in a summary.
    pub facts: usize,
}

impl Default for CommunityOptions {
    fn default() -> Self {
        Self {
            labels: Vec::new(),
            edge_labels: Vec::new(),
            weight: None,
            resolution: 1.0,
            max_levels: 10,
            min_size: 3,
            central: 5,
            terms: 8,
            facts: 5,
        }
    }
}

/// What a refresh did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CommunityReport {
    /// Nodes considered.
    pub nodes: usize,
    /// Levels of the hierarchy that got summaries.
    pub levels: usize,
    /// Summaries after the refresh.
    pub communities: usize,
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Agent-written summaries whose community changed (text kept, marked
    /// `stale`).
    pub stale: usize,
    /// Modularity of the coarsest level.
    pub modularity: f64,
}

/// A summary found by [`overview`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommunityHit {
    pub id: NodeId,
    pub name: String,
    pub level: i64,
    /// Nodes it holds, at every level below.
    pub size: i64,
    pub text: String,
    /// BM25 score for a query; the size share otherwise.
    pub score: f32,
    pub parent: Option<NodeId>,
    /// Summaries of the communities it is made of.
    pub children: usize,
    /// The text was written by an agent.
    pub custom: bool,
    /// An agent's text whose community has changed since.
    pub stale: bool,
}

/// What [`overview`] returns.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct OverviewOptions {
    pub limit: usize,
    /// Only summaries of this level (0 is the finest).
    pub level: Option<i64>,
}

impl Default for OverviewOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            level: None,
        }
    }
}

/// One community, opened: the summaries it is made of and the members
/// directly in it, central ones first.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommunityMembers {
    pub community: CommunityHit,
    pub children: Vec<CommunityHit>,
    pub members: Vec<Node>,
}

/// A community of the new hierarchy.
struct Group {
    level: usize,
    /// Positions of the nodes it holds.
    members: Vec<u32>,
    /// The group of its parent summary.
    parent: Option<usize>,
    props: Props,
    text: String,
    fingerprint: String,
}

/// A stored summary.
struct Stored {
    node: Node,
    leaves: HashSet<NodeId>,
    parent: Option<NodeId>,
    fingerprint: String,
    custom: bool,
}

/// FNV-1a, stable across builds (fingerprints are stored).
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

fn int(node: &Node, key: &str) -> i64 {
    node.prop(key).and_then(Value::as_i64).unwrap_or(0)
}

fn flag(node: &Node, key: &str) -> bool {
    node.prop(key).and_then(Value::as_bool).unwrap_or(false)
}

fn string(node: &Node, key: &str) -> String {
    node.prop(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The words of a node: its text and its string properties.
fn words(node: &Node, text: Option<&str>) -> Vec<String> {
    let mut all = String::new();
    if let Some(text) = text {
        all.push_str(text);
    }
    for value in node.props.values() {
        if let Some(value) = value.as_str() {
            all.push(' ');
            all.push_str(value);
        }
    }
    tokenize(&all)
        .into_iter()
        .filter(|token| token.chars().count() >= 3 && !token.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

fn stored_summaries(read: &ReadTx, ns: &str) -> Result<Vec<Stored>> {
    let mut nodes: HashMap<NodeId, Node> = HashMap::new();
    for id in read.nodes_with_label(ns, COMMUNITY)? {
        if let Some(node) = read.node(ns, id)? {
            nodes.insert(id, node);
        }
    }
    let any = EdgeFilter::any();
    let mut direct: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    let mut children: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut parents: HashMap<NodeId, NodeId> = HashMap::new();
    for &id in nodes.keys() {
        for neighbor in read.neighbors(ns, id, Direction::In, &any.clone().label(MEMBER_OF))? {
            direct.entry(id).or_default().insert(neighbor.node);
        }
        for neighbor in read.neighbors(ns, id, Direction::In, &any.clone().label(PART_OF))? {
            children.entry(id).or_default().push(neighbor.node);
        }
        if let Some(parent) = read
            .neighbors(ns, id, Direction::Out, &any.clone().label(PART_OF))?
            .into_iter()
            .next()
        {
            parents.insert(id, parent.node);
        }
    }
    // Leaves of a summary: its direct members and those of its children,
    // from the finest level up.
    let mut order: Vec<NodeId> = nodes.keys().copied().collect();
    order.sort_by_key(|id| (int(&nodes[id], "level"), *id));
    let mut leaves: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for id in &order {
        let mut set = direct.remove(id).unwrap_or_default();
        for child in children.get(id).into_iter().flatten() {
            if let Some(below) = leaves.get(child) {
                set.extend(below.iter().copied());
            }
        }
        leaves.insert(*id, set);
    }
    Ok(order
        .into_iter()
        .map(|id| {
            let node = nodes.remove(&id).expect("listed");
            Stored {
                fingerprint: string(&node, "fingerprint"),
                custom: flag(&node, "custom"),
                leaves: leaves.remove(&id).unwrap_or_default(),
                parent: parents.get(&id).copied(),
                node,
            }
        })
        .collect())
}

/// Recomputes the community hierarchy of the namespace and brings the
/// summaries up to date: new communities get one, changed ones are
/// rewritten, gone ones are deleted, and unchanged ones are left alone.
/// [`plan_communities`] then [`apply_communities`] in one transaction.
pub fn refresh_communities(
    db: &Grafeno,
    ns: &str,
    options: &CommunityOptions,
) -> Result<CommunityReport> {
    let plan = plan_communities(&db.begin_read()?, ns, options)?;
    db.write(|tx| apply_communities(tx, ns, &plan))
}

/// What a refresh will write, computed from a read transaction (the slow
/// part: Louvain, centrality, terms) so the write is short.
#[derive(Debug)]
pub struct CommunityPlan {
    groups: Vec<Planned>,
    gone: Vec<NodeId>,
    /// Each node, the group of its lowest summary, and its `member_of`
    /// edges to summaries that stay.
    members: Vec<Membership>,
    report: CommunityReport,
}

/// A node, the group of its lowest summary, and its `member_of` edges.
type Membership = (NodeId, Option<usize>, Vec<(EdgeId, NodeId)>);

#[derive(Debug)]
struct Planned {
    /// The stored summary it keeps, with its parent and whether its text is
    /// an agent's.
    stored: Option<(NodeId, Option<NodeId>, bool)>,
    unchanged: bool,
    parent: Option<usize>,
    props: Props,
    text: String,
}

/// Computes a refresh (see [`refresh_communities`]).
pub fn plan_communities(
    read: &ReadTx,
    ns: &str,
    options: &CommunityOptions,
) -> Result<CommunityPlan> {
    let now = Timestamp::now();
    let mut nodes: Vec<Node> = read
        .nodes(ns)?
        .into_iter()
        .filter(|node| {
            !node.has_label(COMMUNITY)
                && !node.has_label(ARCHIVED)
                && !crate::palace::is_structure(node)
                && (options.labels.is_empty()
                    || options.labels.iter().any(|label| node.has_label(label)))
        })
        .collect();
    nodes.sort_by_key(|node| node.id);
    let position: HashMap<NodeId, u32> = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id, i as u32))
        .collect();
    let mut edges: Vec<(u32, u32, EdgeId, f64, grafeno::Edge)> = Vec::new();
    for edge in read.edges(ns)? {
        if edge.expired_at.is_some()
            || !edge.holds_at(now)
            || edge.label == MEMBER_OF
            || edge.label == PART_OF
            || edge.from == edge.to
            || (!options.edge_labels.is_empty() && !options.edge_labels.contains(&edge.label))
        {
            continue;
        }
        let (Some(&from), Some(&to)) = (position.get(&edge.from), position.get(&edge.to)) else {
            continue;
        };
        let weight = options
            .weight
            .as_ref()
            .and_then(|key| edge.props.get(key))
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        edges.push((from, to, edge.id, weight, edge));
    }
    let projection = Projection::from_edges(
        nodes.iter().map(|node| node.id).collect(),
        edges.iter().map(|(from, to, id, weight, _)| {
            (
                nodes[*from as usize].id,
                nodes[*to as usize].id,
                *id,
                *weight,
            )
        }),
        options.weight.is_some(),
    );
    let settings = Louvain {
        resolution: options.resolution,
        max_levels: options.max_levels.max(1),
        ..Louvain::default()
    };
    let (hierarchy, modularity, _) = louvain_levels(&projection, &settings, &never)?;

    // The groups worth a summary: big enough, and not the same as the one
    // community below them.
    let mut groups: Vec<Group> = Vec::new();
    // For each level, the group of each community (if summarized).
    let mut group_of: Vec<HashMap<u32, usize>> = Vec::new();
    for (level, membership) in hierarchy.iter().enumerate() {
        let mut by_community: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (node, community) in membership.iter().enumerate() {
            by_community
                .entry(*community)
                .or_default()
                .push(node as u32);
        }
        let mut here = HashMap::new();
        for (community, members) in by_community {
            if members.len() < options.min_size.max(2) {
                continue;
            }
            if level > 0 {
                let below = &hierarchy[level - 1];
                let first = below[members[0] as usize];
                if members.iter().all(|m| below[*m as usize] == first) {
                    continue;
                }
            }
            here.insert(community, groups.len());
            groups.push(Group {
                level,
                members,
                parent: None,
                props: Props::new(),
                text: String::new(),
                fingerprint: String::new(),
            });
        }
        group_of.push(here);
    }
    // Drop levels without summaries and renumber the rest.
    let used: Vec<usize> = {
        let mut levels: Vec<usize> = groups.iter().map(|g| g.level).collect();
        levels.sort_unstable();
        levels.dedup();
        levels
    };
    // Each node's lowest summary, and each summary's parent.
    let mut lowest: Vec<Option<usize>> = vec![None; nodes.len()];
    for (index, group) in groups.iter().enumerate() {
        for &member in &group.members {
            if lowest[member as usize].is_none() {
                lowest[member as usize] = Some(index);
            }
        }
    }
    for group in &mut groups {
        let first = group.members[0] as usize;
        group.parent = (group.level + 1..hierarchy.len())
            .find_map(|up| group_of[up].get(&hierarchy[up][first]).copied());
    }

    // Words, document frequencies and names.
    let mut texts: Vec<Vec<String>> = Vec::with_capacity(nodes.len());
    let mut df: HashMap<String, usize> = HashMap::new();
    let mut names: Vec<String> = Vec::with_capacity(nodes.len());
    for node in &nodes {
        let text = read.text(ns, node.id)?;
        let tokens = words(node, text.as_deref());
        let unique: HashSet<&String> = tokens.iter().collect();
        for token in unique {
            *df.entry(token.clone()).or_default() += 1;
        }
        names.push(crate::context::readable_name(
            node,
            text.as_deref().unwrap_or(""),
        ));
        texts.push(tokens);
    }
    let total = nodes.len().max(1) as f64;

    // The edges inside each group, one pass over the edges per level.
    let mut internal_of: Vec<Vec<usize>> = vec![Vec::new(); groups.len()];
    for (level, membership) in hierarchy.iter().enumerate() {
        if group_of[level].is_empty() {
            continue;
        }
        for (index, (from, to, _, _, _)) in edges.iter().enumerate() {
            let community = membership[*from as usize];
            if membership[*to as usize] == community
                && let Some(&g) = group_of[level].get(&community)
            {
                internal_of[g].push(index);
            }
        }
    }
    for (group, internal) in groups.iter_mut().zip(internal_of) {
        let level = group.level;
        let mut degree: HashMap<u32, f64> = HashMap::new();
        let mut facts: Vec<(f64, usize)> = Vec::new();
        let mut fnv = Fnv::new();
        fnv.write(&(level as u64).to_le_bytes());
        for &member in &group.members {
            let node = &nodes[member as usize];
            fnv.write(&node.id.as_u128().to_le_bytes());
            fnv.write(&node.updated_at.as_micros().to_le_bytes());
        }
        for &index in &internal {
            let (from, to, _, weight, _) = &edges[index];
            *degree.entry(*from).or_default() += weight;
            *degree.entry(*to).or_default() += weight;
        }
        for &index in &internal {
            let (from, to, id, _, _) = &edges[index];
            fnv.write(&id.as_u128().to_le_bytes());
            let score =
                degree.get(from).copied().unwrap_or(0.0) + degree.get(to).copied().unwrap_or(0.0);
            facts.push((score, index));
        }
        let mut central: Vec<u32> = group.members.clone();
        central.sort_by(|a, b| {
            let (da, db) = (
                degree.get(a).copied().unwrap_or(0.0),
                degree.get(b).copied().unwrap_or(0.0),
            );
            db.total_cmp(&da)
                .then(nodes[*a as usize].id.cmp(&nodes[*b as usize].id))
        });
        central.truncate(options.central);
        facts.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        facts.truncate(options.facts);

        let mut tf: HashMap<&str, usize> = HashMap::new();
        let mut labels: BTreeMap<&str, usize> = BTreeMap::new();
        let (mut from, mut to) = (None::<Timestamp>, None::<Timestamp>);
        let mut span = |at: Timestamp| {
            from = Some(from.map_or(at, |f| f.min(at)));
            to = Some(to.map_or(at, |t| t.max(at)));
        };
        for &member in &group.members {
            let node = &nodes[member as usize];
            for token in &texts[member as usize] {
                *tf.entry(token.as_str()).or_default() += 1;
            }
            for label in &node.labels {
                *labels.entry(label.as_str()).or_default() += 1;
            }
            span(node.created_at);
            span(node.updated_at);
        }
        for &index in &internal {
            if let Some(at) = edges[index].4.valid_from {
                span(at);
            }
        }
        let mut terms: Vec<(f64, &str)> = tf
            .iter()
            .map(|(term, count)| {
                let idf = (total / df.get(*term).copied().unwrap_or(1) as f64).ln();
                (*count as f64 * idf, *term)
            })
            // A term in every node tells nothing apart.
            .filter(|(score, _)| *score > 0.0)
            .collect();
        terms.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
        terms.truncate(options.terms);
        let mut label_mix: Vec<(&str, usize)> = labels.into_iter().collect();
        label_mix.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

        let central_names: Vec<String> =
            central.iter().map(|p| names[*p as usize].clone()).collect();
        let fact_lines: Vec<String> = facts
            .iter()
            .map(|(_, index)| {
                let (from, to, _, _, edge) = &edges[*index];
                format!(
                    "{} {} {}",
                    names[*from as usize], edge.label, names[*to as usize]
                )
            })
            .collect();
        let term_list: Vec<&str> = terms.iter().map(|(_, term)| *term).collect();
        let name = central_names
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let mut text = format!(
            "A group of {} ({}).",
            group.members.len(),
            label_mix
                .iter()
                .map(|(label, count)| format!("{label} {count}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if !central_names.is_empty() {
            text.push_str(&format!(" Central: {}.", central_names.join(", ")));
        }
        if !term_list.is_empty() {
            text.push_str(&format!(" About: {}.", term_list.join(", ")));
        }
        if !fact_lines.is_empty() {
            text.push_str(&format!(" Facts: {}.", fact_lines.join("; ")));
        }
        if let (Some(from), Some(to)) = (from, to) {
            text.push_str(&format!(" From {} to {}.", from.date(), to.date()));
        }
        let level_number = used.binary_search(&level).unwrap_or(level) as i64;
        let mut props = Props::new();
        props.insert("name".into(), json!(name));
        props.insert("level".into(), json!(level_number));
        props.insert("size".into(), json!(group.members.len()));
        props.insert("central".into(), json!(central_names));
        props.insert(
            "central_ids".into(),
            json!(
                central
                    .iter()
                    .map(|p| nodes[*p as usize].id.to_string())
                    .collect::<Vec<_>>()
            ),
        );
        props.insert("terms".into(), json!(term_list));
        props.insert(
            "labels".into(),
            Value::Object(
                label_mix
                    .iter()
                    .map(|(label, count)| ((*label).to_owned(), json!(count)))
                    .collect(),
            ),
        );
        props.insert("facts".into(), json!(fact_lines));
        props.insert("from".into(), json!(from.map(Timestamp::iso)));
        props.insert("to".into(), json!(to.map(Timestamp::iso)));
        group.fingerprint = format!("{:016x}", fnv.0);
        props.insert("fingerprint".into(), json!(group.fingerprint));
        group.props = props;
        group.text = text;
    }

    // Match the new groups to the stored summaries by the members they
    // share (Jaccard of at least one half), at any level: a community stays
    // the same summary when the levels around it change. Best pairs first.
    let stored = stored_summaries(read, ns)?;
    let mut by_leaf: HashMap<NodeId, Vec<usize>> = HashMap::new();
    for (index, summary) in stored.iter().enumerate() {
        for leaf in &summary.leaves {
            by_leaf.entry(*leaf).or_default().push(index);
        }
    }
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (g, group) in groups.iter().enumerate() {
        let mut shared: HashMap<usize, usize> = HashMap::new();
        for &member in &group.members {
            for &s in by_leaf
                .get(&nodes[member as usize].id)
                .into_iter()
                .flatten()
            {
                *shared.entry(s).or_default() += 1;
            }
        }
        for (s, common) in shared {
            let union = group.members.len() + stored[s].leaves.len() - common;
            let jaccard = common as f64 / union.max(1) as f64;
            if jaccard >= 0.5 {
                pairs.push((jaccard, g, s));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut taken: HashMap<usize, usize> = HashMap::new();
    let mut used_stored: HashSet<usize> = HashSet::new();
    for (_, g, s) in pairs {
        if taken.contains_key(&g) || used_stored.contains(&s) {
            continue;
        }
        taken.insert(g, s);
        used_stored.insert(s);
    }

    // What the edges should be, against what they are.
    let mut want_member: HashMap<NodeId, usize> = HashMap::new();
    for (p, group) in lowest.iter().enumerate() {
        if let Some(group) = group {
            want_member.insert(nodes[p].id, *group);
        }
    }
    let mut have_member: HashMap<NodeId, Vec<(EdgeId, NodeId)>> = HashMap::new();
    for edge in read.edges(ns)? {
        if edge.label == MEMBER_OF {
            have_member
                .entry(edge.from)
                .or_default()
                .push((edge.id, edge.to));
        }
    }
    let report = CommunityReport {
        nodes: nodes.len(),
        levels: used.len(),
        communities: groups.len(),
        modularity,
        ..CommunityReport::default()
    };
    // Stored summaries no group took: deleted, with their edges.
    let kept: HashSet<usize> = taken.values().copied().collect();
    let gone: HashSet<NodeId> = stored
        .iter()
        .enumerate()
        .filter(|(index, _)| !kept.contains(index))
        .map(|(_, summary)| summary.node.id)
        .collect();
    let members = nodes
        .iter()
        .map(|node| {
            let live: Vec<(EdgeId, NodeId)> = have_member
                .get(&node.id)
                .into_iter()
                .flatten()
                .filter(|(_, to)| !gone.contains(to))
                .copied()
                .collect();
            (node.id, want_member.get(&node.id).copied(), live)
        })
        .collect();
    let groups = groups
        .into_iter()
        .enumerate()
        .map(|(g, group)| {
            let old = taken.get(&g).map(|s| &stored[*s]);
            Planned {
                stored: old.map(|old| {
                    (
                        old.node.id,
                        old.parent.filter(|parent| !gone.contains(parent)),
                        old.custom,
                    )
                }),
                unchanged: old.is_some_and(|old| old.fingerprint == group.fingerprint),
                parent: group.parent,
                props: group.props,
                text: group.text,
            }
        })
        .collect();
    let mut gone: Vec<NodeId> = gone.into_iter().collect();
    gone.sort_unstable();
    Ok(CommunityPlan {
        groups,
        gone,
        members,
        report,
    })
}

/// Writes a planned refresh. Returns what it did.
pub fn apply_communities(
    tx: &mut WriteTx,
    ns: &str,
    plan: &CommunityPlan,
) -> Result<CommunityReport> {
    let mut report = plan.report.clone();
    for id in &plan.gone {
        // A summary deleted since the plan is already gone.
        if tx.node(ns, *id)?.is_some() {
            tx.delete_node(ns, *id)?;
        }
        report.removed += 1;
    }
    let mut summaries: Vec<NodeId> = Vec::with_capacity(plan.groups.len());
    let now = Timestamp::now().iso();
    for group in &plan.groups {
        match group.stored {
            Some((id, _, _)) if group.unchanged => {
                report.unchanged += 1;
                summaries.push(id);
            }
            Some((id, _, custom)) => {
                let mut props = group.props.clone();
                if custom {
                    props.insert("stale".into(), json!(true));
                    report.stale += 1;
                } else {
                    tx.set_text(ns, id, &group.text)?;
                }
                props.insert("refreshed_at".into(), json!(now));
                tx.set_node_props(ns, id, props)?;
                report.updated += 1;
                summaries.push(id);
            }
            None => {
                let mut new = NewNode::new(COMMUNITY).text(group.text.clone());
                new.props = group.props.clone();
                new.props.insert("refreshed_at".into(), json!(now));
                summaries.push(tx.create_node(ns, new)?);
                report.created += 1;
            }
        }
    }
    // Parents.
    for (g, group) in plan.groups.iter().enumerate() {
        let want = group.parent.map(|p| summaries[p]);
        let have = group.stored.and_then(|(_, parent, _)| parent);
        if want != have {
            tx.delete_edges_from(ns, summaries[g], Some(PART_OF))?;
            if let Some(parent) = want {
                tx.create_edge(ns, NewEdge::new(summaries[g], PART_OF, parent))?;
            }
        }
    }
    // Members.
    for (node, want, live) in &plan.members {
        let want = want.map(|g| summaries[g]);
        let right = match want {
            Some(want) => live.len() == 1 && live[0].1 == want,
            None => live.is_empty(),
        };
        if right {
            continue;
        }
        for (edge, _) in live {
            tx.delete_edge(ns, *edge)?;
        }
        if let Some(summary) = want {
            tx.create_edge(ns, NewEdge::new(*node, MEMBER_OF, summary))?;
        }
    }
    Ok(report)
}

fn hit(read: &ReadTx, ns: &str, node: &Node, score: f32) -> Result<CommunityHit> {
    let parent = read
        .neighbors(
            ns,
            node.id,
            Direction::Out,
            &EdgeFilter::any().label(PART_OF),
        )?
        .into_iter()
        .next()
        .map(|n| n.node);
    let children = read
        .neighbors(
            ns,
            node.id,
            Direction::In,
            &EdgeFilter::any().label(PART_OF),
        )?
        .len();
    Ok(CommunityHit {
        id: node.id,
        name: string(node, "name"),
        level: int(node, "level"),
        size: int(node, "size"),
        text: read.text(ns, node.id)?.unwrap_or_default(),
        score,
        parent,
        children,
        custom: flag(node, "custom"),
        stale: flag(node, "stale"),
    })
}

/// Global search for broad questions: the community summaries that match
/// `query` best (BM25 over their text), or, without a query, the top of
/// the hierarchy (the summaries without a parent), biggest first. Drill
/// down with [`community_members`].
pub fn overview(
    read: &ReadTx,
    ns: &str,
    query: Option<&str>,
    options: &OverviewOptions,
) -> Result<Vec<CommunityHit>> {
    let fits = |node: &Node| {
        node.has_label(COMMUNITY)
            && options
                .level
                .is_none_or(|level| int(node, "level") == level)
    };
    let mut out = Vec::new();
    match query.filter(|q| !q.trim().is_empty()) {
        Some(query) => {
            let pool = options.limit.saturating_mul(20).max(100);
            for found in read.search_text(ns, query, pool)? {
                if out.len() >= options.limit {
                    break;
                }
                if let Some(node) = read.node(ns, found.node)?
                    && fits(&node)
                {
                    out.push(hit(read, ns, &node, found.score)?);
                }
            }
        }
        None => {
            let mut all = Vec::new();
            let mut total = 0i64;
            for id in read.nodes_with_label(ns, COMMUNITY)? {
                if let Some(node) = read.node(ns, id)?
                    && fits(&node)
                {
                    let summary = hit(read, ns, &node, 0.0)?;
                    if options.level.is_some() || summary.parent.is_none() {
                        total += summary.size;
                        all.push(summary);
                    }
                }
            }
            for summary in &mut all {
                summary.score = summary.size as f32 / total.max(1) as f32;
            }
            all.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
            all.truncate(options.limit);
            out = all;
        }
    }
    Ok(out)
}

/// Opens a community: the summaries of the communities it is made of
/// (biggest first) and up to `limit` of the members directly in it, its
/// central ones first.
pub fn community_members(
    read: &ReadTx,
    ns: &str,
    id: NodeId,
    limit: usize,
) -> Result<CommunityMembers> {
    let node = read
        .node(ns, id)?
        .filter(|node| node.has_label(COMMUNITY))
        .ok_or(grafeno::Error::NodeNotFound(id))?;
    let community = hit(read, ns, &node, 0.0)?;
    let mut children = Vec::new();
    for child in read.neighbors(ns, id, Direction::In, &EdgeFilter::any().label(PART_OF))? {
        if let Some(child) = read.node(ns, child.node)? {
            children.push(hit(read, ns, &child, 0.0)?);
        }
    }
    children.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
    let central: Vec<String> = node
        .prop("central_ids")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let rank = |member: &Node| {
        central
            .iter()
            .position(|c| *c == member.id.to_string())
            .unwrap_or(usize::MAX)
    };
    let mut members = Vec::new();
    for member in read.neighbors(ns, id, Direction::In, &EdgeFilter::any().label(MEMBER_OF))? {
        if let Some(member) = read.node(ns, member.node)? {
            members.push(member);
        }
    }
    members.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.id.cmp(&b.id)));
    members.truncate(limit);
    Ok(CommunityMembers {
        community,
        children,
        members,
    })
}

/// Replaces a summary's text with an agent's own; refreshes keep it (and
/// mark it `stale` when the community changes) until `text` is `None`,
/// which goes back to the generated summary on the next refresh.
pub fn set_community_text(
    tx: &mut WriteTx,
    ns: &str,
    id: NodeId,
    text: Option<&str>,
) -> Result<()> {
    let node = tx
        .node(ns, id)?
        .filter(|node| node.has_label(COMMUNITY))
        .ok_or(grafeno::Error::NodeNotFound(id))?;
    let mut props = Props::new();
    match text {
        Some(text) => {
            tx.set_text(ns, id, text)?;
            props.insert("custom".into(), json!(true));
        }
        None => {
            props.insert("custom".into(), json!(false));
            // Forces a rewrite on the next refresh.
            props.insert("fingerprint".into(), json!(""));
        }
    }
    props.insert("stale".into(), json!(false));
    let _ = node;
    tx.set_node_props(ns, id, props)
}
