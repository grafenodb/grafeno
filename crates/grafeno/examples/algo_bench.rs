//! Times graph algorithms on a random graph with communities.
//!
//! ```bash
//! cargo run --release -p grafeno --example algo_bench -- 100000 5
//! ```
//!
//! Arguments: nodes, and edges per node (default 100 000 and 5). Nodes fall
//! in communities of 100; 90% of edges stay inside one. Each step reports
//! its time and the most memory it allocated beyond what was held before
//! (counted by the allocator).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// Counts the bytes allocated now and the most since the last reset.
struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let now = NOW.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(now, Ordering::Relaxed);
        // SAFETY: the same layout, passed on to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        NOW.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: allocated above by the system allocator with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

use grafeno::algo::{self, FastRp, Louvain, PageRank, ProjectionSpec};
use grafeno::{Direction, Grafeno, NewEdge, NewNode, NodeId};

const NS: &str = "bench";

fn main() -> grafeno::Result<()> {
    let mut args = std::env::args().skip(1);
    let nodes: usize = args.next().map_or(100_000, |n| n.parse().expect("nodes"));
    let per_node: usize = args
        .next()
        .map_or(5, |n| n.parse().expect("edges per node"));
    let db = Grafeno::in_memory()?;
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut random = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    let started = Instant::now();
    let ids: Vec<NodeId> = db.write(|tx| {
        (0..nodes)
            .map(|i| tx.create_node(NS, NewNode::new("Node").prop("i", i as i64)))
            .collect()
    })?;
    db.write(|tx| {
        for (i, &from) in ids.iter().enumerate() {
            for _ in 0..per_node {
                let to = if random(10) < 9 {
                    (i / 100) * 100 + random(100)
                } else {
                    random(nodes)
                };
                let to = ids[to.min(nodes - 1)];
                tx.create_edge(
                    NS,
                    NewEdge::new(from, "link", to).prop("w", 1 + random(9) as i64),
                )?;
            }
        }
        Ok(())
    })?;
    println!(
        "{nodes} nodes, {} edges: built in {:.1} s\n",
        nodes * per_node,
        started.elapsed().as_secs_f64()
    );
    println!("| Step | Time | Memory |\n| --- | --- | --- |");
    let time =
        |name: &str, f: &mut dyn FnMut() -> grafeno::Result<String>| -> grafeno::Result<()> {
            let before = NOW.load(Ordering::Relaxed);
            PEAK.store(before, Ordering::Relaxed);
            let started = Instant::now();
            let note = f()?;
            let extra = PEAK.load(Ordering::Relaxed).saturating_sub(before);
            println!(
                "| {name} | {:.0} ms {note} | {:.1} MiB |",
                started.elapsed().as_secs_f64() * 1e3,
                extra as f64 / (1024.0 * 1024.0)
            );
            Ok(())
        };
    let spec = ProjectionSpec::default();
    let weighted = ProjectionSpec {
        weight: Some("w".into()),
        ..ProjectionSpec::default()
    };
    let mut p = None;
    time("Projection", &mut || {
        p = Some(db.begin_read()?.projection(NS, &spec)?);
        Ok(String::new())
    })?;
    time("Projection again (cached)", &mut || {
        db.begin_read()?.projection(NS, &spec)?;
        Ok(String::new())
    })?;
    let mut w = None;
    time("Projection with weights", &mut || {
        w = Some(db.begin_read()?.projection(NS, &weighted)?);
        Ok(String::new())
    })?;
    let (p, w) = (p.expect("built"), w.expect("built"));
    let never = &algo::never;
    time("Dijkstra, one pair", &mut || {
        let path = algo::shortest_path(&w, 0, (nodes - 1) as u32, Direction::Out, never)?;
        Ok(format!("({} hops)", path.map_or(0, |p| p.edges.len())))
    })?;
    time("Yen, 5 paths", &mut || {
        let paths = algo::k_shortest_paths(&w, 0, (nodes - 1) as u32, 5, Direction::Out, never)?;
        Ok(format!("({} found)", paths.len()))
    })?;
    time("PageRank, 20 rounds", &mut || {
        algo::page_rank(&p, &PageRank::default(), never)?;
        Ok(String::new())
    })?;
    time("Weak components", &mut || {
        let groups = algo::weakly_connected(&p);
        Ok(format!("({})", groups.iter().max().map_or(0, |m| m + 1)))
    })?;
    time("Label propagation", &mut || {
        let (groups, _) = algo::label_propagation(&p, 10, 42, never)?;
        Ok(format!(
            "({} communities)",
            groups.iter().max().map_or(0, |m| m + 1)
        ))
    })?;
    time("Louvain", &mut || {
        let (groups, q, _) = algo::louvain(&p, &Louvain::default(), never)?;
        Ok(format!(
            "({} communities, modularity {q:.3})",
            groups.iter().max().map_or(0, |m| m + 1)
        ))
    })?;
    time("Triangles", &mut || {
        Ok(format!("({})", algo::triangles(&p, never)?.total))
    })?;
    time("Betweenness, 100 sampled sources", &mut || {
        algo::betweenness(&p, Direction::Both, Some(100), 42, never)?;
        Ok(String::new())
    })?;
    time("FastRP, 64 dimensions", &mut || {
        let settings = FastRp {
            dimension: 64,
            ..FastRp::default()
        };
        algo::fast_rp(&p, &settings, never)?;
        Ok(String::new())
    })?;
    time("Node similarity, top 10", &mut || {
        let pairs = algo::similar_neighbors(
            &p,
            algo::SimilarityMetric::Jaccard,
            Direction::Both,
            10,
            0.0,
            None,
            never,
        )?;
        Ok(format!("({} pairs)", pairs.len()))
    })?;
    // 16.8: analysis.
    time("k-core", &mut || {
        let cores = algo::k_core(&p);
        Ok(format!("(max core {})", cores.iter().max().unwrap_or(&0)))
    })?;
    time("Leiden", &mut || {
        let (groups, q, _) = algo::leiden(&p, &Louvain::default(), never)?;
        Ok(format!(
            "({} communities, modularity {q:.3})",
            groups.iter().max().map_or(0, |m| m + 1)
        ))
    })?;
    time("HITS, 20 rounds", &mut || {
        algo::hits(&p, 20, 0.0, never)?;
        Ok(String::new())
    })?;
    time("Eigenvector, 100 rounds", &mut || {
        let (_, rounds) = algo::eigenvector(&p, Direction::Both, 100, 1e-6, never)?;
        Ok(format!("({rounds} rounds)"))
    })?;
    time("Articulation points and bridges", &mut || {
        let (points, bridges) = algo::cut_points(&p);
        Ok(format!(
            "({} points, {} bridges)",
            points.len(),
            bridges.len()
        ))
    })?;
    time("Minimum spanning forest", &mut || {
        let (edges, total) = algo::spanning_forest(&w, false);
        Ok(format!("({} edges, weight {total})", edges.len()))
    })?;
    time("Maximum flow, one pair", &mut || {
        let flow = algo::max_flow(&w, 0, (nodes - 1) as u32, never)?;
        Ok(format!("(value {})", flow.value))
    })?;
    time("Bipartite matching (even and odd)", &mut || {
        let left: Vec<bool> = (0..p.len()).map(|at| at % 2 == 0).collect();
        Ok(format!(
            "({} pairs)",
            algo::bipartite_matching(&p, &left).len()
        ))
    })?;
    time("Topological order (finds a cycle)", &mut || {
        Ok(match algo::topological_order(&p) {
            Ok(order) => format!("({} nodes)", order.len()),
            Err(cycle) => format!("(a cycle of {})", cycle.len()),
        })
    })?;
    time("Random walks, 10 000 of 20 steps", &mut || {
        let starts: Vec<u32> = (0..10_000u32).collect();
        algo::random_walks(&w, &starts, 20, 1, Direction::Out, 42, never)?;
        Ok(String::new())
    })?;
    // 16.8: simulation.
    time("Distance field from 100 sources", &mut || {
        let sources: Vec<u32> = (0..100u32).map(|i| i * (nodes as u32 / 100)).collect();
        let field = algo::distance_field(&w, &sources, Direction::Both, None, never)?;
        Ok(format!("({} reached)", field.iter().flatten().count()))
    })?;
    time("Influence of 10 sources, decay 0.5", &mut || {
        let sources: Vec<(u32, f64)> = (0..10u32).map(|i| (i * 1_000, 100.0)).collect();
        let settings = algo::Influence {
            decay: 0.5,
            threshold: 0.01,
            max_cost: None,
            sum: true,
        };
        let map = algo::influence(&p, &sources, Direction::Both, &settings, never)?;
        Ok(format!(
            "({} touched)",
            map.iter().filter(|v| **v > 0.0).count()
        ))
    })?;
    time("Flood fill, half the nodes open", &mut || {
        let open: Vec<bool> = (0..p.len()).map(|at| at % 2 == 0).collect();
        let region = algo::flood_fill(&p, &[0], &open, Direction::Both);
        Ok(format!("({} filled)", region.iter().flatten().count()))
    })?;
    time("Diffusion, 20 steps", &mut || {
        let initial: Vec<f64> = (0..p.len()).map(|at| (at % 7) as f64).collect();
        let settings = algo::Diffusion {
            rate: 0.3,
            steps: 20,
            capacity: None,
        };
        algo::diffuse(&p, &initial, Direction::Both, &settings, never)?;
        Ok(String::new())
    })?;
    time("Life rule, 20 steps", &mut || {
        let initial: Vec<bool> = (0..p.len()).map(|at| at % 3 == 0).collect();
        let rule = algo::Rule::Life {
            birth: vec![3],
            survive: vec![2, 3],
        };
        let (_, _, steps) = algo::automaton(&p, &initial, &rule, 20, Direction::Both, never)?;
        Ok(format!("({steps} steps)"))
    })?;
    time("Assignment, 300 agents to 300 tasks", &mut || {
        let costs: Vec<Vec<Option<f64>>> = (0..300)
            .map(|a| {
                (0..300)
                    .map(|t| Some(((a * 31 + t * 17) % 97) as f64))
                    .collect()
            })
            .collect();
        let (_, total) = algo::assignment(&costs);
        Ok(format!("(cost {total})"))
    })?;
    time("Min-cost flow, 50 units across", &mut || {
        let capacities: Vec<f64> = vec![10.0; w.edge_count()];
        let costs: Vec<f64> = (0..w.edge_count()).map(|e| (e % 9 + 1) as f64).collect();
        let mut supply = vec![0.0; w.len()];
        supply[0] = 50.0;
        supply[nodes - 1] = -50.0;
        let flow = algo::min_cost_flow(&w, &capacities, &costs, &supply, never)?;
        Ok(format!("(sent {}, cost {})", flow.value, flow.cost))
    })?;
    Ok(())
}
