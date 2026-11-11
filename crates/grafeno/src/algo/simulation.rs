//! Simulation on graphs (16.8): worlds of places, agents and resources.
//! Distance fields from many sources, influence maps, flood fill,
//! diffusion along edges over steps, rules applied to every node from its
//! neighbors, assignment at least cost, and flows at least cost. Random
//! walks for agents are [`super::random_walks`].
//!
//! Edge weights are costs (distances) for fields and influence, shares
//! for diffusion, capacities for flows.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use grafeno_core::{Error, Result};

use super::{Projection, Stop, check};
use crate::traverse::Direction;

/// A distance with its node, ordered for a min-heap.
#[derive(Clone, Copy, PartialEq)]
struct Near(f64, u32);

impl Eq for Near {}

impl PartialOrd for Near {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Near {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

/// From every node, the cost to the nearest of `sources` (going along
/// `direction` from the sources) and which source that is: one search
/// from all of them at once. `None` for nodes no source reaches within
/// `max_cost`.
pub fn distance_field(
    p: &Projection,
    sources: &[u32],
    direction: Direction,
    max_cost: Option<f64>,
    stop: Stop,
) -> Result<Vec<Option<(f64, u32)>>> {
    let mut best: Vec<Option<(f64, u32)>> = vec![None; p.len()];
    let mut heap = BinaryHeap::new();
    for &source in sources {
        best[source as usize] = Some((0.0, source));
        heap.push(Reverse(Near(0.0, source)));
    }
    let mut visited = 0;
    while let Some(Reverse(Near(cost, node))) = heap.pop() {
        if best[node as usize].is_some_and(|(known, _)| cost > known) {
            continue;
        }
        visited += 1;
        check(stop, visited)?;
        let origin = best[node as usize].expect("reached").1;
        for step in p.neighbors(node, direction) {
            let next = cost + step.weight.max(0.0);
            if max_cost.is_some_and(|max| next > max) {
                continue;
            }
            let at = step.node as usize;
            if best[at].is_none_or(|(known, _)| next < known) {
                best[at] = Some((next, origin));
                heap.push(Reverse(Near(next, step.node)));
            }
        }
    }
    Ok(best)
}

/// How influence spreads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Influence {
    /// What is left of a value after a cost of 1: `value × decay^cost`.
    pub decay: f64,
    /// Values below this are not spread further.
    pub threshold: f64,
    /// Nor past this cost.
    pub max_cost: Option<f64>,
    /// Several sources reaching a node add up (`true`) or the strongest
    /// counts.
    pub sum: bool,
}

/// An influence map: each source's value spread through the graph,
/// weakening with the cost of the way (the cheapest way counts), combined
/// at each node.
pub fn influence(
    p: &Projection,
    sources: &[(u32, f64)],
    direction: Direction,
    settings: &Influence,
    stop: Stop,
) -> Result<Vec<f64>> {
    if !(0.0..=1.0).contains(&settings.decay) {
        return Err(Error::Invalid("decay goes from 0 to 1".into()));
    }
    let mut out = vec![0.0f64; p.len()];
    for (i, &(source, value)) in sources.iter().enumerate() {
        check(stop, i)?;
        let field = distance_field(p, &[source], direction, settings.max_cost, stop)?;
        for (node, found) in field.into_iter().enumerate() {
            let Some((cost, _)) = found else {
                continue;
            };
            let here = value * settings.decay.powf(cost);
            if here.abs() < settings.threshold {
                continue;
            }
            out[node] = if settings.sum {
                out[node] + here
            } else if here.abs() > out[node].abs() {
                here
            } else {
                out[node]
            };
        }
    }
    Ok(out)
}

/// The region reachable from `seeds` through nodes for which `inside` is
/// true (seeds outside it reach nothing): for each node, how many steps
/// away it is, `None` outside the region.
pub fn flood_fill(
    p: &Projection,
    seeds: &[u32],
    inside: &[bool],
    direction: Direction,
) -> Vec<Option<u32>> {
    let mut steps = vec![None; p.len()];
    let mut queue = VecDeque::new();
    for &seed in seeds {
        if inside[seed as usize] && steps[seed as usize].is_none() {
            steps[seed as usize] = Some(0);
            queue.push_back(seed);
        }
    }
    while let Some(node) = queue.pop_front() {
        let here = steps[node as usize].expect("queued");
        for step in p.neighbors(node, direction) {
            let at = step.node as usize;
            if inside[at] && steps[at].is_none() {
                steps[at] = Some(here + 1);
                queue.push_back(step.node);
            }
        }
    }
    steps
}

/// How a quantity diffuses.
#[derive(Clone, Debug, PartialEq)]
pub struct Diffusion {
    /// The share of a node's amount that leaves it at each step, split
    /// among its neighbors by edge weight.
    pub rate: f64,
    pub steps: usize,
    /// The most each node may hold (none: no limit); what would go past
    /// it stays where it was.
    pub capacity: Option<Vec<f64>>,
}

/// The amounts after `steps` of diffusion from `initial`: the total stays
/// the same. Returns the amounts at the end.
pub fn diffuse(
    p: &Projection,
    initial: &[f64],
    direction: Direction,
    settings: &Diffusion,
    stop: Stop,
) -> Result<Vec<f64>> {
    if !(0.0..=1.0).contains(&settings.rate) {
        return Err(Error::Invalid("rate goes from 0 to 1".into()));
    }
    let n = p.len();
    let mut amounts = initial.to_vec();
    for round in 0..settings.steps {
        check(stop, round * 256)?;
        let mut incoming = vec![0.0; n];
        let mut leaving = vec![0.0; n];
        // What each node sends, and to whom.
        let mut sends: Vec<(u32, u32, f64)> = Vec::new();
        for node in 0..n as u32 {
            let amount = amounts[node as usize];
            if amount <= 0.0 {
                continue;
            }
            let total: f64 = p
                .neighbors(node, direction)
                .map(|step| step.weight.max(0.0))
                .sum();
            if total <= 0.0 {
                continue;
            }
            let out = amount * settings.rate;
            for step in p.neighbors(node, direction) {
                let share = out * step.weight.max(0.0) / total;
                if share > 0.0 {
                    sends.push((node, step.node, share));
                    incoming[step.node as usize] += share;
                }
            }
        }
        // A full node takes only what fits; the rest goes back.
        let mut scale = vec![1.0; n];
        if let Some(capacity) = &settings.capacity {
            for node in 0..n {
                let room =
                    (capacity.get(node).copied().unwrap_or(f64::INFINITY) - amounts[node]).max(0.0);
                if incoming[node] > room {
                    scale[node] = room / incoming[node];
                }
            }
        }
        let mut arriving = vec![0.0; n];
        for (from, to, share) in sends {
            let taken = share * scale[to as usize];
            leaving[from as usize] += taken;
            arriving[to as usize] += taken;
        }
        for node in 0..n {
            amounts[node] += arriving[node] - leaving[node];
        }
    }
    Ok(amounts)
}

/// A rule applied to every node at once, from its neighbors' states.
#[derive(Clone, Debug, PartialEq)]
pub enum Rule {
    /// A node off turns on with one of `birth` neighbors on; a node on
    /// stays on with one of `survive` neighbors on.
    Life {
        birth: Vec<usize>,
        survive: Vec<usize>,
    },
    /// A node turns on when at least this share of its neighbors is on
    /// (and, without `recover`, never turns off): spreading adoption.
    Threshold { share: f64, recover: bool },
    /// A node takes the state most of its neighbors have (ties keep it).
    Majority,
}

/// The states after running `rule` for up to `steps` (it stops early when
/// nothing changes): each node's final state and the step at which it
/// last changed. Returns those and the steps run.
pub fn automaton(
    p: &Projection,
    initial: &[bool],
    rule: &Rule,
    steps: usize,
    direction: Direction,
    stop: Stop,
) -> Result<(Vec<bool>, Vec<Option<usize>>, usize)> {
    let n = p.len();
    let mut state = initial.to_vec();
    let mut changed: Vec<Option<usize>> = vec![None; n];
    let mut ran = 0;
    for round in 1..=steps {
        check(stop, round * 256)?;
        let next: Vec<bool> = (0..n as u32)
            .map(|node| {
                let (mut on, mut all) = (0usize, 0usize);
                for step in p.neighbors(node, direction) {
                    if step.node != node {
                        all += 1;
                        on += usize::from(state[step.node as usize]);
                    }
                }
                let here = state[node as usize];
                match rule {
                    Rule::Life { birth, survive } => {
                        if here {
                            survive.contains(&on)
                        } else {
                            birth.contains(&on)
                        }
                    }
                    Rule::Threshold { share, recover } => {
                        let reached = all > 0 && on as f64 >= share * all as f64;
                        if *recover { reached } else { here || reached }
                    }
                    Rule::Majority => {
                        let off = all - on;
                        if on > off {
                            true
                        } else if off > on {
                            false
                        } else {
                            here
                        }
                    }
                }
            })
            .collect();
        ran = round;
        let mut any = false;
        for node in 0..n {
            if next[node] != state[node] {
                changed[node] = Some(round);
                any = true;
            }
        }
        state = next;
        if !any {
            break;
        }
    }
    Ok((state, changed, ran))
}

/// The assignment of rows (agents) to columns (tasks) at least total cost
/// (the Hungarian method); `None` where a pair is not allowed. Each row
/// gets at most one column and each column at most one row; as many pairs
/// as can be made. Returns each row's column and the total cost.
pub fn assignment(costs: &[Vec<Option<f64>>]) -> (Vec<Option<usize>>, f64) {
    let rows = costs.len();
    let columns = costs.iter().map(Vec::len).max().unwrap_or(0);
    let size = rows.max(columns);
    if size == 0 {
        return (Vec::new(), 0.0);
    }
    // Pairs not allowed (and the padding) cost more than any allowed set.
    let known: f64 = costs
        .iter()
        .flatten()
        .flatten()
        .map(|c| c.abs())
        .sum::<f64>()
        + 1.0;
    let forbidden = known * (size as f64 + 1.0);
    let cost = |r: usize, c: usize| -> f64 {
        costs
            .get(r)
            .and_then(|row| row.get(c))
            .copied()
            .flatten()
            .unwrap_or(forbidden)
    };
    // Potentials u, v and the matching p (1-based, 0 = none).
    let mut u = vec![0.0; size + 1];
    let mut v = vec![0.0; size + 1];
    let mut matched = vec![0usize; size + 1];
    let mut way = vec![0usize; size + 1];
    for row in 1..=size {
        matched[0] = row;
        let mut column0 = 0;
        let mut least = vec![f64::INFINITY; size + 1];
        let mut used = vec![false; size + 1];
        loop {
            used[column0] = true;
            let row0 = matched[column0];
            let mut delta = f64::INFINITY;
            let mut column1 = 0;
            for column in 1..=size {
                if used[column] {
                    continue;
                }
                let current = cost(row0 - 1, column - 1) - u[row0] - v[column];
                if current < least[column] {
                    least[column] = current;
                    way[column] = column0;
                }
                if least[column] < delta {
                    delta = least[column];
                    column1 = column;
                }
            }
            for column in 0..=size {
                if used[column] {
                    u[matched[column]] += delta;
                    v[column] -= delta;
                } else {
                    least[column] -= delta;
                }
            }
            column0 = column1;
            if matched[column0] == 0 {
                break;
            }
        }
        loop {
            let column1 = way[column0];
            matched[column0] = matched[column1];
            column0 = column1;
            if column0 == 0 {
                break;
            }
        }
    }
    let mut out = vec![None; rows];
    let mut total = 0.0;
    for (column, &row) in matched.iter().enumerate().skip(1) {
        if row == 0 || row > rows || column > columns {
            continue;
        }
        if let Some(Some(c)) = costs[row - 1].get(column - 1) {
            out[row - 1] = Some(column - 1);
            total += c;
        }
    }
    (out, total)
}

/// A flow at least cost.
#[derive(Clone, Debug, PartialEq)]
pub struct CostFlow {
    /// What the supplies sent to the demands.
    pub value: f64,
    pub cost: f64,
    /// The flow on each edge that carries some.
    pub edges: Vec<(u32, f64)>,
    /// Supply left unsent (no way, or no demand for it).
    pub unmet: f64,
}

/// Sends what nodes supply (`supply` > 0) to what others need (< 0) along
/// the edges' directions, each edge carrying at most its `capacity` at
/// `cost` per unit, at least total cost (successive cheapest paths). Costs
/// may not be negative.
pub fn min_cost_flow(
    p: &Projection,
    capacity: &[f64],
    cost: &[f64],
    supply: &[f64],
    stop: Stop,
) -> Result<CostFlow> {
    if cost.iter().any(|c| *c < 0.0) {
        return Err(Error::Invalid("a flow's costs may not be negative".into()));
    }
    let n = p.len();
    let (source, sink) = (n as u32, n as u32 + 1);
    let mut residual = super::analysis::Residual::from_projection(p, capacity, cost, 2);
    let initial = residual.room.clone();
    let mut wanted = 0.0;
    for (node, amount) in supply.iter().enumerate() {
        if *amount > 0.0 {
            residual.add(source, node as u32, *amount, 0.0, u32::MAX);
            wanted += amount;
        } else if *amount < 0.0 {
            residual.add(node as u32, sink, -amount, 0.0, u32::MAX);
        }
    }
    let total_nodes = n + 2;
    let (mut value, mut total_cost) = (0.0, 0.0);
    let mut rounds = 0;
    loop {
        check(stop, rounds)?;
        rounds += 1;
        // The cheapest way with room left (Bellman-Ford with a queue).
        let mut distance = vec![f64::INFINITY; total_nodes];
        let mut by = vec![usize::MAX; total_nodes];
        let mut queued = vec![false; total_nodes];
        distance[source as usize] = 0.0;
        let mut queue = VecDeque::from([source]);
        while let Some(node) = queue.pop_front() {
            queued[node as usize] = false;
            for &arc in &residual.first[node as usize] {
                if residual.room[arc] <= 1e-12 {
                    continue;
                }
                let to = residual.to[arc] as usize;
                let next = distance[node as usize] + residual.cost[arc];
                if next < distance[to] - 1e-12 {
                    distance[to] = next;
                    by[to] = arc;
                    if !queued[to] {
                        queued[to] = true;
                        queue.push_back(to as u32);
                    }
                }
            }
        }
        if !distance[sink as usize].is_finite() {
            break;
        }
        let mut room = f64::INFINITY;
        let mut node = sink as usize;
        while node != source as usize {
            let arc = by[node];
            room = room.min(residual.room[arc]);
            node = residual.to[arc ^ 1] as usize;
        }
        let mut node = sink as usize;
        while node != source as usize {
            let arc = by[node];
            residual.room[arc] -= room;
            residual.room[arc ^ 1] += room;
            node = residual.to[arc ^ 1] as usize;
        }
        value += room;
        total_cost += room * distance[sink as usize];
    }
    Ok(CostFlow {
        value,
        cost: total_cost,
        edges: residual.flows(|arc| initial.get(arc).copied().unwrap_or(0.0)),
        unmet: (wanted - value).max(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::super::never;
    use super::super::tests::{graph, weighted};
    use super::*;

    #[test]
    fn fields_influence_and_fill() {
        // A line 0-1-2-3-4 with two goals at the ends.
        let p = graph(5, &[(0, 1), (1, 2), (2, 3), (3, 4)]);
        let field = distance_field(&p, &[0, 4], Direction::Both, None, &never).unwrap();
        assert_eq!(field[1], Some((1.0, 0)));
        assert_eq!(field[3], Some((1.0, 4)));
        assert_eq!(field[2].unwrap().0, 2.0);
        let map = influence(
            &p,
            &[(0, 8.0)],
            Direction::Both,
            &Influence {
                decay: 0.5,
                threshold: 1.0,
                max_cost: None,
                sum: true,
            },
            &never,
        )
        .unwrap();
        assert_eq!(map, vec![8.0, 4.0, 2.0, 1.0, 0.0]);
        let region = flood_fill(&p, &[0], &[true, true, false, true, true], Direction::Both);
        assert_eq!(region, vec![Some(0), Some(1), None, None, None]);
    }

    #[test]
    fn diffusion_keeps_the_total() {
        let p = graph(3, &[(0, 1), (1, 2)]);
        let settings = Diffusion {
            rate: 0.5,
            steps: 10,
            capacity: None,
        };
        let after = diffuse(&p, &[9.0, 0.0, 0.0], Direction::Both, &settings, &never).unwrap();
        assert!((after.iter().sum::<f64>() - 9.0).abs() < 1e-9);
        assert!(after[2] > 0.0);
        let capped = Diffusion {
            capacity: Some(vec![9.0, 1.0, 9.0]),
            ..settings
        };
        let after = diffuse(&p, &[9.0, 0.0, 0.0], Direction::Both, &capped, &never).unwrap();
        assert!(after[1] <= 1.0 + 1e-9);
        assert!((after.iter().sum::<f64>() - 9.0).abs() < 1e-9);
    }

    #[test]
    fn rules_assignment_and_cheap_flows() {
        // A blinker on a 3x3 grid with diagonals: the life rule.
        let mut edges = Vec::new();
        for a in 0..9u32 {
            for b in (a + 1)..9u32 {
                let (ax, ay, bx, by) = (a % 3, a / 3, b % 3, b / 3);
                if ax.abs_diff(bx) <= 1 && ay.abs_diff(by) <= 1 {
                    edges.push((a, b));
                }
            }
        }
        let grid = graph(9, &edges);
        let row = [false, false, false, true, true, true, false, false, false];
        let life = Rule::Life {
            birth: vec![3],
            survive: vec![2, 3],
        };
        let (after, _, _) = automaton(&grid, &row, &life, 1, Direction::Both, &never).unwrap();
        assert_eq!(
            after,
            vec![false, true, false, false, true, false, false, true, false]
        );
        let spread = Rule::Threshold {
            share: 0.3,
            recover: false,
        };
        let line = graph(4, &[(0, 1), (1, 2), (2, 3)]);
        let (after, when, _) = automaton(
            &line,
            &[true, false, false, false],
            &spread,
            10,
            Direction::Both,
            &never,
        )
        .unwrap();
        assert!(after.iter().all(|on| *on));
        assert_eq!(when[3], Some(3));

        let (pairs, total) = assignment(&[
            vec![Some(4.0), Some(1.0), Some(3.0)],
            vec![Some(2.0), Some(0.0), Some(5.0)],
            vec![Some(3.0), Some(2.0), Some(2.0)],
        ]);
        assert_eq!(total, 5.0);
        assert_eq!(pairs, vec![Some(1), Some(0), Some(2)]);
        let (pairs, _) = assignment(&[vec![None, Some(1.0)], vec![None, Some(2.0)]]);
        assert_eq!(pairs.iter().filter(|p| p.is_some()).count(), 1);

        // Two ways from 0 to 3: cheap and narrow, dear and wide.
        let net = weighted(
            4,
            &[(0, 1, 2.0), (1, 3, 2.0), (0, 2, 5.0), (2, 3, 5.0)],
            true,
        );
        let flow = min_cost_flow(
            &net,
            &[2.0, 2.0, 5.0, 5.0],
            &[1.0, 1.0, 3.0, 3.0],
            &[4.0, 0.0, 0.0, -4.0],
            &never,
        )
        .unwrap();
        assert_eq!(flow.value, 4.0);
        assert_eq!(flow.cost, 2.0 * 2.0 + 2.0 * 6.0);
        assert_eq!(flow.unmet, 0.0);
    }
}
