//! Analysis (16.8): k-core decomposition, HITS, eigenvector centrality,
//! articulation points and bridges, spanning trees, maximum flow and
//! minimum cut, bipartite matching, topological order and cycles, random
//! walks, conductance, and Leiden's refinement of Louvain communities.
//!
//! Like the other algorithms, these take a [`Projection`] and positions;
//! undirected ones read both directions of every edge.

use std::collections::{HashMap, VecDeque};

use grafeno_core::Result;

use super::community::{Louvain, louvain, modularity};
use super::{Projection, Random, Stop, check, stopped};
use crate::traverse::Direction;

/// The core number of every node: the largest `k` such that the node is
/// in a subgraph where every node has at least `k` neighbors (edges taken
/// both ways; each neighbor counts once).
pub fn k_core(p: &Projection) -> Vec<u32> {
    let n = p.len();
    let neighbors: Vec<Vec<u32>> = (0..n as u32)
        .map(|node| {
            let mut list: Vec<u32> = p
                .neighbors(node, Direction::Both)
                .map(|step| step.node)
                .filter(|other| *other != node)
                .collect();
            list.sort_unstable();
            list.dedup();
            list
        })
        .collect();
    let mut degree: Vec<usize> = neighbors.iter().map(Vec::len).collect();
    let max = degree.iter().copied().max().unwrap_or(0);
    // Nodes in buckets by current degree, peeled from the smallest.
    let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); max + 1];
    for (node, d) in degree.iter().enumerate() {
        buckets[*d].push(node as u32);
    }
    let mut core = vec![0u32; n];
    let mut done = vec![false; n];
    let mut k = 0;
    let mut level = 0;
    while level <= max {
        let Some(node) = buckets[level].pop() else {
            level += 1;
            continue;
        };
        let at = node as usize;
        if done[at] || degree[at] != level {
            continue;
        }
        k = k.max(level);
        core[at] = k as u32;
        done[at] = true;
        for &other in &neighbors[at] {
            let o = other as usize;
            if !done[o] && degree[o] > level {
                degree[o] -= 1;
                buckets[degree[o]].push(other);
            }
        }
        // A neighbor may now sit below the current level.
        level = level.saturating_sub(1);
    }
    core
}

/// Hub and authority scores (HITS): a good hub points to good
/// authorities, a good authority is pointed to by good hubs. Each vector
/// has length 1. Returns the hubs, the authorities and the iterations.
pub fn hits(
    p: &Projection,
    iterations: usize,
    tolerance: f64,
    stop: Stop,
) -> Result<(Vec<f64>, Vec<f64>, usize)> {
    let n = p.len();
    let start = if n == 0 { 0.0 } else { 1.0 / (n as f64).sqrt() };
    let mut hubs = vec![start; n];
    let mut authorities = vec![start; n];
    let normalize = |values: &mut [f64]| {
        let norm = values.iter().map(|v| v * v).sum::<f64>().sqrt();
        if norm > 0.0 {
            values.iter_mut().for_each(|v| *v /= norm);
        }
    };
    let mut rounds = 0;
    for round in 0..iterations {
        if stop() {
            return Err(stopped());
        }
        rounds = round + 1;
        let mut next_authorities = vec![0.0; n];
        for node in 0..n as u32 {
            next_authorities[node as usize] = p
                .neighbors(node, Direction::In)
                .map(|step| step.weight * hubs[step.node as usize])
                .sum();
        }
        normalize(&mut next_authorities);
        let mut next_hubs = vec![0.0; n];
        for node in 0..n as u32 {
            next_hubs[node as usize] = p
                .neighbors(node, Direction::Out)
                .map(|step| step.weight * next_authorities[step.node as usize])
                .sum();
        }
        normalize(&mut next_hubs);
        let change: f64 = next_hubs
            .iter()
            .zip(&hubs)
            .chain(next_authorities.iter().zip(&authorities))
            .map(|(a, b)| (a - b).abs())
            .sum();
        hubs = next_hubs;
        authorities = next_authorities;
        if change < tolerance {
            break;
        }
    }
    Ok((hubs, authorities, rounds))
}

/// Eigenvector centrality: a node scores by the scores of the nodes whose
/// edges reach it (`direction` In: those pointing to it; Both: every
/// neighbor), by power iteration; the vector has length 1.
pub fn eigenvector(
    p: &Projection,
    direction: Direction,
    iterations: usize,
    tolerance: f64,
    stop: Stop,
) -> Result<(Vec<f64>, usize)> {
    let n = p.len();
    let mut scores = vec![if n == 0 { 0.0 } else { 1.0 / (n as f64).sqrt() }; n];
    let mut rounds = 0;
    for round in 0..iterations {
        if stop() {
            return Err(stopped());
        }
        rounds = round + 1;
        // Adding the current score keeps the iteration from oscillating on
        // bipartite graphs (the same eigenvector, shifted eigenvalue).
        let mut next: Vec<f64> = (0..n as u32)
            .map(|node| {
                scores[node as usize]
                    + p.neighbors(node, direction)
                        .map(|step| step.weight * scores[step.node as usize])
                        .sum::<f64>()
            })
            .collect();
        let norm = next.iter().map(|v| v * v).sum::<f64>().sqrt();
        if norm > 0.0 {
            next.iter_mut().for_each(|v| *v /= norm);
        }
        let change: f64 = next.iter().zip(&scores).map(|(a, b)| (a - b).abs()).sum();
        scores = next;
        if change < tolerance {
            break;
        }
    }
    Ok((scores, rounds))
}

/// A node on the depth-first stack: itself, the edge it came by, its
/// neighbors (node, edge), and the next of them to look at.
type Visit = (u32, u32, Vec<(u32, u32)>, usize);

/// Articulation points (nodes whose removal disconnects their component)
/// and bridges (edges whose removal does), edges taken both ways.
pub fn cut_points(p: &Projection) -> (Vec<u32>, Vec<u32>) {
    let n = p.len();
    let mut order = vec![u32::MAX; n];
    let mut low = vec![0u32; n];
    let mut points = vec![false; n];
    let mut bridges = Vec::new();
    let mut counter = 0u32;
    for root in 0..n as u32 {
        if order[root as usize] != u32::MAX {
            continue;
        }
        let mut stack: Vec<Visit> = Vec::new();
        let steps = |node: u32| -> Vec<(u32, u32)> {
            p.neighbors(node, Direction::Both)
                .map(|step| (step.node, step.edge))
                .collect()
        };
        order[root as usize] = counter;
        low[root as usize] = counter;
        counter += 1;
        stack.push((root, u32::MAX, steps(root), 0));
        let mut root_children = 0;
        while let Some(top) = stack.last_mut() {
            let (node, via, list, next) = (top.0, top.1, &top.2, top.3);
            if next < list.len() {
                top.3 += 1;
                let (other, edge) = list[next];
                if edge == via || other == node {
                    continue;
                }
                if order[other as usize] == u32::MAX {
                    order[other as usize] = counter;
                    low[other as usize] = counter;
                    counter += 1;
                    if node == root {
                        root_children += 1;
                    }
                    let list = steps(other);
                    stack.push((other, edge, list, 0));
                } else {
                    low[node as usize] = low[node as usize].min(order[other as usize]);
                }
                continue;
            }
            stack.pop();
            if let Some(parent) = stack.last() {
                let parent = parent.0;
                low[parent as usize] = low[parent as usize].min(low[node as usize]);
                if low[node as usize] > order[parent as usize] {
                    bridges.push(via);
                }
                if parent != root && low[node as usize] >= order[parent as usize] {
                    points[parent as usize] = true;
                }
            }
        }
        if root_children > 1 {
            points[root as usize] = true;
        }
    }
    let points = (0..n as u32).filter(|at| points[*at as usize]).collect();
    bridges.sort_unstable();
    bridges.dedup();
    (points, bridges)
}

fn find(parents: &mut [u32], mut node: u32) -> u32 {
    while parents[node as usize] != node {
        parents[node as usize] = parents[parents[node as usize] as usize];
        node = parents[node as usize];
    }
    node
}

/// A minimum (or, with `maximum`, maximum) spanning forest by edge weight
/// (Kruskal): one tree per component, edges taken both ways. Returns the
/// edges and their total weight.
pub fn spanning_forest(p: &Projection, maximum: bool) -> (Vec<u32>, f64) {
    let mut edges: Vec<(f64, u32, u32, u32)> = Vec::with_capacity(p.edge_count());
    for node in 0..p.len() as u32 {
        for step in p.neighbors(node, Direction::Out) {
            edges.push((step.weight, step.edge, node, step.node));
        }
    }
    edges.sort_by(|a, b| {
        let order = a.0.total_cmp(&b.0);
        (if maximum { order.reverse() } else { order }).then(a.1.cmp(&b.1))
    });
    let mut parents: Vec<u32> = (0..p.len() as u32).collect();
    let mut chosen = Vec::new();
    let mut total = 0.0;
    for (weight, edge, a, b) in edges {
        let (ra, rb) = (find(&mut parents, a), find(&mut parents, b));
        if ra != rb {
            parents[ra as usize] = rb;
            chosen.push(edge);
            total += weight;
        }
    }
    (chosen, total)
}

/// A flow from a source to a sink.
#[derive(Clone, Debug, PartialEq)]
pub struct Flow {
    /// What goes from the source to the sink.
    pub value: f64,
    /// The flow on each edge that carries some.
    pub edges: Vec<(u32, f64)>,
    /// The source's side of a minimum cut: the nodes it still reaches
    /// through edges with room left.
    pub source_side: Vec<u32>,
    /// The edges of the minimum cut, full, from the source side out.
    pub cut: Vec<u32>,
}

/// The arcs of a residual network: forward ones with the edge's capacity,
/// backward ones with none.
pub(crate) struct Residual {
    pub(crate) to: Vec<u32>,
    pub(crate) room: Vec<f64>,
    pub(crate) cost: Vec<f64>,
    /// The projection edge of a forward arc (`u32::MAX` for backward ones
    /// and for arcs added for supplies).
    pub(crate) edge: Vec<u32>,
    pub(crate) first: Vec<Vec<usize>>,
}

impl Residual {
    fn new(nodes: usize) -> Self {
        Self {
            to: Vec::new(),
            room: Vec::new(),
            cost: Vec::new(),
            edge: Vec::new(),
            first: vec![Vec::new(); nodes],
        }
    }

    pub(crate) fn add(&mut self, from: u32, to: u32, room: f64, cost: f64, edge: u32) {
        self.first[from as usize].push(self.to.len());
        self.to.push(to);
        self.room.push(room);
        self.cost.push(cost);
        self.edge.push(edge);
        self.first[to as usize].push(self.to.len());
        self.to.push(from);
        self.room.push(0.0);
        self.cost.push(-cost);
        self.edge.push(u32::MAX);
    }

    pub(crate) fn from_projection(
        p: &Projection,
        capacity: &[f64],
        cost: &[f64],
        extra: usize,
    ) -> Self {
        let mut residual = Residual::new(p.len() + extra);
        for node in 0..p.len() as u32 {
            for step in p.neighbors(node, Direction::Out) {
                let edge = step.edge as usize;
                residual.add(
                    node,
                    step.node,
                    capacity.get(edge).copied().unwrap_or(step.weight).max(0.0),
                    cost.get(edge).copied().unwrap_or(0.0),
                    step.edge,
                );
            }
        }
        residual
    }

    /// The flow on each projection edge.
    pub(crate) fn flows(&self, capacity: impl Fn(usize) -> f64) -> Vec<(u32, f64)> {
        let mut out = Vec::new();
        for arc in (0..self.to.len()).step_by(2) {
            let edge = self.edge[arc];
            if edge == u32::MAX {
                continue;
            }
            let flow = capacity(arc) - self.room[arc];
            if flow > 1e-12 {
                out.push((edge, flow));
            }
        }
        out.sort_by_key(|(edge, _)| *edge);
        out
    }
}

/// The maximum flow from `source` to `sink` along the edges' directions,
/// each edge carrying at most its weight (Dinic), and a minimum cut.
pub fn max_flow(p: &Projection, source: u32, sink: u32, stop: Stop) -> Result<Flow> {
    let capacity: Vec<f64> = Vec::new();
    let mut residual = Residual::from_projection(p, &capacity, &[], 0);
    let initial: Vec<f64> = residual.room.clone();
    let n = p.len();
    let mut value = 0.0;
    let mut rounds = 0;
    if source != sink {
        loop {
            check(stop, rounds)?;
            rounds += 1;
            // Levels by breadth-first search over arcs with room.
            let mut level = vec![u32::MAX; n];
            level[source as usize] = 0;
            let mut queue = VecDeque::from([source]);
            while let Some(node) = queue.pop_front() {
                for &arc in &residual.first[node as usize] {
                    let to = residual.to[arc];
                    if residual.room[arc] > 1e-12 && level[to as usize] == u32::MAX {
                        level[to as usize] = level[node as usize] + 1;
                        queue.push_back(to);
                    }
                }
            }
            if level[sink as usize] == u32::MAX {
                break;
            }
            // Blocking flow by depth-first search, one path at a time.
            let mut next = vec![0usize; n];
            loop {
                let mut path: Vec<usize> = Vec::new();
                let mut node = source;
                let pushed = loop {
                    if node == sink {
                        let room = path
                            .iter()
                            .map(|arc| residual.room[*arc])
                            .fold(f64::INFINITY, f64::min);
                        break Some(room);
                    }
                    let arcs = &residual.first[node as usize];
                    let mut advanced = false;
                    while next[node as usize] < arcs.len() {
                        let arc = arcs[next[node as usize]];
                        let to = residual.to[arc];
                        if residual.room[arc] > 1e-12
                            && level[to as usize] == level[node as usize] + 1
                        {
                            path.push(arc);
                            node = to;
                            advanced = true;
                            break;
                        }
                        next[node as usize] += 1;
                    }
                    if advanced {
                        continue;
                    }
                    // A dead end: back up and skip the arc that led here.
                    let Some(arc) = path.pop() else {
                        break None;
                    };
                    node = residual.to[arc ^ 1];
                    next[node as usize] += 1;
                };
                let Some(room) = pushed else {
                    break;
                };
                for arc in path {
                    residual.room[arc] -= room;
                    residual.room[arc ^ 1] += room;
                }
                value += room;
            }
        }
    }
    // The source side: what it reaches through arcs with room.
    let mut seen = vec![false; n];
    seen[source as usize] = true;
    let mut queue = VecDeque::from([source]);
    while let Some(node) = queue.pop_front() {
        for &arc in &residual.first[node as usize] {
            let to = residual.to[arc];
            if residual.room[arc] > 1e-12 && !seen[to as usize] {
                seen[to as usize] = true;
                queue.push_back(to);
            }
        }
    }
    let mut cut = Vec::new();
    for node in 0..n as u32 {
        if !seen[node as usize] {
            continue;
        }
        for step in p.neighbors(node, Direction::Out) {
            if !seen[step.node as usize] {
                cut.push(step.edge);
            }
        }
    }
    cut.sort_unstable();
    Ok(Flow {
        value,
        edges: residual.flows(|arc| initial[arc]),
        source_side: (0..n as u32).filter(|at| seen[*at as usize]).collect(),
        cut,
    })
}

/// A maximum matching between the nodes marked `left` and the others,
/// over edges with one end on each side (Hopcroft-Karp): `(left, right,
/// edge)` triples.
pub fn bipartite_matching(p: &Projection, left: &[bool]) -> Vec<(u32, u32, u32)> {
    let n = p.len();
    let candidates: Vec<Vec<(u32, u32)>> = (0..n as u32)
        .map(|node| {
            if !left[node as usize] {
                return Vec::new();
            }
            p.neighbors(node, Direction::Both)
                .filter(|step| !left[step.node as usize])
                .map(|step| (step.node, step.edge))
                .collect()
        })
        .collect();
    let mut partner = vec![u32::MAX; n];
    let mut via = vec![u32::MAX; n];
    loop {
        // Layers from the free left nodes.
        let mut distance = vec![u32::MAX; n];
        let mut queue = VecDeque::new();
        for node in 0..n as u32 {
            if left[node as usize] && partner[node as usize] == u32::MAX {
                distance[node as usize] = 0;
                queue.push_back(node);
            }
        }
        let mut found = false;
        while let Some(node) = queue.pop_front() {
            for &(right, _) in &candidates[node as usize] {
                let next = partner[right as usize];
                if next == u32::MAX {
                    found = true;
                } else if distance[next as usize] == u32::MAX {
                    distance[next as usize] = distance[node as usize] + 1;
                    queue.push_back(next);
                }
            }
        }
        if !found {
            break;
        }
        // Augmenting paths along the layers.
        fn augment(
            node: u32,
            candidates: &[Vec<(u32, u32)>],
            partner: &mut [u32],
            via: &mut [u32],
            distance: &mut [u32],
        ) -> bool {
            for &(right, edge) in &candidates[node as usize] {
                let next = partner[right as usize];
                if next == u32::MAX
                    || (distance[next as usize] == distance[node as usize] + 1
                        && augment(next, candidates, partner, via, distance))
                {
                    partner[node as usize] = right;
                    partner[right as usize] = node;
                    via[node as usize] = edge;
                    return true;
                }
            }
            distance[node as usize] = u32::MAX;
            false
        }
        let mut grew = false;
        for node in 0..n as u32 {
            if left[node as usize]
                && partner[node as usize] == u32::MAX
                && augment(node, &candidates, &mut partner, &mut via, &mut distance)
            {
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    (0..n as u32)
        .filter(|node| left[*node as usize] && partner[*node as usize] != u32::MAX)
        .map(|node| (node, partner[node as usize], via[node as usize]))
        .collect()
}

/// The nodes in an order where every edge goes forward (Kahn), or, when
/// the edges make a cycle, one cycle (its nodes in order).
pub fn topological_order(p: &Projection) -> std::result::Result<Vec<u32>, Vec<u32>> {
    let n = p.len();
    let mut incoming: Vec<usize> = (0..n as u32)
        .map(|node| p.degree(node, Direction::In))
        .collect();
    let mut queue: VecDeque<u32> = (0..n as u32)
        .filter(|node| incoming[*node as usize] == 0)
        .collect();
    let mut order = Vec::with_capacity(n);
    while let Some(node) = queue.pop_front() {
        order.push(node);
        for step in p.neighbors(node, Direction::Out) {
            let at = step.node as usize;
            incoming[at] -= 1;
            if incoming[at] == 0 {
                queue.push_back(step.node);
            }
        }
    }
    if order.len() == n {
        return Ok(order);
    }
    // A node left over is on a cycle or below one: walk back along edges
    // between left-over nodes until a node repeats.
    let left: Vec<bool> = {
        let mut left = vec![true; n];
        for node in &order {
            left[*node as usize] = false;
        }
        left
    };
    let start = (0..n as u32)
        .find(|node| left[*node as usize])
        .expect("left over");
    let mut seen: HashMap<u32, usize> = HashMap::new();
    let mut walk = Vec::new();
    let mut node = start;
    loop {
        if let Some(&at) = seen.get(&node) {
            let mut cycle: Vec<u32> = walk[at..].to_vec();
            cycle.reverse();
            return Err(cycle);
        }
        seen.insert(node, walk.len());
        walk.push(node);
        node = p
            .neighbors(node, Direction::In)
            .map(|step| step.node)
            .find(|other| left[*other as usize])
            .expect("a left-over node has a left-over predecessor");
    }
}

/// Walks of `length` steps from each start (`walks` each), choosing the
/// next edge at random with chance proportional to its weight (or evenly);
/// a walk that reaches a node with no way on stops there. The same seed
/// gives the same walks.
pub fn random_walks(
    p: &Projection,
    starts: &[u32],
    length: usize,
    walks: usize,
    direction: Direction,
    seed: u64,
    stop: Stop,
) -> Result<Vec<Vec<u32>>> {
    let mut random = Random::new(seed);
    let mut out = Vec::with_capacity(starts.len() * walks);
    for (i, &start) in starts.iter().enumerate() {
        check(stop, i)?;
        for _ in 0..walks {
            let mut walk = vec![start];
            let mut node = start;
            for _ in 0..length {
                let steps: Vec<(u32, f64)> = p
                    .neighbors(node, direction)
                    .map(|step| (step.node, step.weight.max(0.0)))
                    .collect();
                let total: f64 = steps.iter().map(|(_, w)| w).sum();
                if steps.is_empty() || total <= 0.0 {
                    break;
                }
                let mut pick = (random.next() >> 11) as f64 / (1u64 << 53) as f64 * total;
                let mut chosen = steps[steps.len() - 1].0;
                for (other, weight) in &steps {
                    if pick < *weight {
                        chosen = *other;
                        break;
                    }
                    pick -= weight;
                }
                node = chosen;
                walk.push(node);
            }
            out.push(walk);
        }
    }
    Ok(out)
}

/// The conductance of each group of nodes (`None`: in no group): the
/// weight of the edges leaving it over the smaller of its volume and the
/// rest's (the sum of degrees), from 0 (closed) to 1. Groups in order.
pub fn conductance(p: &Projection, groups: &[Option<i64>]) -> Vec<(i64, f64)> {
    let mut volume: HashMap<i64, f64> = HashMap::new();
    let mut leaving: HashMap<i64, f64> = HashMap::new();
    let mut total = 0.0;
    for node in 0..p.len() as u32 {
        for step in p.neighbors(node, Direction::Both) {
            total += step.weight;
            let Some(group) = groups[node as usize] else {
                continue;
            };
            *volume.entry(group).or_default() += step.weight;
            if groups[step.node as usize] != Some(group) {
                *leaving.entry(group).or_default() += step.weight;
            }
        }
    }
    let mut out: Vec<(i64, f64)> = volume
        .into_iter()
        .map(|(group, inside)| {
            let smaller = inside.min(total - inside);
            let cut = leaving.get(&group).copied().unwrap_or(0.0);
            (group, if smaller > 0.0 { cut / smaller } else { 0.0 })
        })
        .collect();
    out.sort_by_key(|(group, _)| *group);
    out
}

/// Communities by modularity with Leiden's guarantee that each is
/// connected: Louvain's communities, each split into its connected parts
/// within it (a community Louvain merged through a node that later left is
/// split), then renumbered. Returns the communities, their modularity and
/// Louvain's levels.
pub fn leiden(p: &Projection, settings: &Louvain, stop: Stop) -> Result<(Vec<u32>, f64, usize)> {
    let (communities, _, levels) = louvain(p, settings, stop)?;
    let n = p.len();
    let mut refined = vec![u32::MAX; n];
    let mut next = 0u32;
    for start in 0..n as u32 {
        if refined[start as usize] != u32::MAX {
            continue;
        }
        let community = communities[start as usize];
        refined[start as usize] = next;
        let mut queue = VecDeque::from([start]);
        while let Some(node) = queue.pop_front() {
            for step in p.neighbors(node, Direction::Both) {
                let at = step.node as usize;
                if refined[at] == u32::MAX && communities[at] == community {
                    refined[at] = next;
                    queue.push_back(step.node);
                }
            }
        }
        next += 1;
    }
    let score = modularity(p, &refined);
    Ok((refined, score, levels))
}

#[cfg(test)]
mod tests {
    use super::super::never;
    use super::super::tests::{graph, weighted};
    use super::*;

    #[test]
    fn cores_and_cut_points() {
        // A triangle 0-1-2, a tail 2-3-4, and a square 5-6-7-8 apart.
        let p = graph(
            9,
            &[
                (0, 1),
                (1, 2),
                (2, 0),
                (2, 3),
                (3, 4),
                (5, 6),
                (6, 7),
                (7, 8),
                (8, 5),
            ],
        );
        assert_eq!(k_core(&p), vec![2, 2, 2, 1, 1, 2, 2, 2, 2]);
        let (points, bridges) = cut_points(&p);
        assert_eq!(points, vec![2, 3]);
        assert_eq!(bridges.len(), 2);
    }

    #[test]
    fn spanning_tree_flow_and_matching() {
        let p = weighted(
            4,
            &[
                (0, 1, 1.0),
                (1, 2, 2.0),
                (0, 2, 5.0),
                (2, 3, 1.0),
                (1, 3, 4.0),
            ],
            true,
        );
        let (edges, total) = spanning_forest(&p, false);
        assert_eq!(edges.len(), 3);
        assert_eq!(total, 4.0);
        // Capacities: 0→1 1, 0→2 5, 1→2 2, 1→3 4, 2→3 1: at most 1 + 1.
        let flow = max_flow(&p, 0, 3, &never).unwrap();
        assert_eq!(flow.value, 2.0);
        assert_eq!(flow.cut.len(), 2, "{flow:?}");
        // Three left nodes, two right, a perfect matching of two.
        let b = graph(5, &[(0, 3), (1, 3), (1, 4), (2, 4)]);
        let matched = bipartite_matching(&b, &[true, true, true, false, false]);
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn order_cycles_and_scores() {
        let dag = graph(4, &[(0, 1), (1, 2), (0, 3), (3, 2)]);
        let order = topological_order(&dag).unwrap();
        assert_eq!(order[0], 0);
        assert_eq!(*order.last().unwrap(), 2);
        let cyclic = graph(4, &[(0, 1), (1, 2), (2, 1), (2, 3)]);
        let mut cycle = topological_order(&cyclic).unwrap_err();
        cycle.sort_unstable();
        assert_eq!(cycle, vec![1, 2]);
        // A star: the center is the authority and the leaves are hubs.
        let star = graph(4, &[(1, 0), (2, 0), (3, 0)]);
        let (hubs, authorities, _) = hits(&star, 50, 1e-9, &never).unwrap();
        assert!(authorities[0] > 0.99 && hubs[1] > 0.5);
        let (scores, _) = eigenvector(&star, Direction::Both, 100, 1e-12, &never).unwrap();
        assert!(scores[0] > scores[1]);
        let walks = random_walks(&star, &[1], 3, 2, Direction::Both, 7, &never).unwrap();
        assert_eq!(walks.len(), 2);
        assert_eq!(
            walks,
            random_walks(&star, &[1], 3, 2, Direction::Both, 7, &never).unwrap(),
            "the same seed, the same walks"
        );
        let groups = [Some(0), Some(0), Some(1), Some(1)];
        let two = graph(4, &[(0, 1), (2, 3), (1, 2)]);
        let found = conductance(&two, &groups);
        assert_eq!(found.len(), 2);
        assert!((found[0].1 - 1.0 / 3.0).abs() < 1e-9, "{found:?}");
    }
}
