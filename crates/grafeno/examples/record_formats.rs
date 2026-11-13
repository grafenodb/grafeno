//! What JSON records cost, and what other encodings would.
//!
//! `cargo run --release -p grafeno --example record_formats`
//!
//! A database file with 50 000 nodes (labels, six properties of mixed
//! types, a name) and 200 000 edges (a label, two properties, a valid
//! time), closed and opened again so nothing is cached in the process.
//!
//! 1. Access paths through the engine: reading every node and every edge
//!    by id, neighbors with their edge records, k-hop over adjacency alone.
//! 2. The same bytes fetched from the file without decoding them (redb
//!    directly): what is left is the decoding.
//! 3. Every record decoded and encoded as JSON (today), as CBOR (binary,
//!    self-describing like JSON: the same serde types, field names kept),
//!    as JSON with the fixed fields only (properties skipped: what a lazy
//!    decode of the properties would pay), and as a hand-written binary
//!    header for the fixed fields with the properties left as JSON; with
//!    their size and allocations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use grafeno::{Direction, Edge, EdgeFilter, Grafeno, NewEdge, NewNode, Node, NodeId, Timestamp};
use redb::{ReadableDatabase, TableDefinition};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::json;

/// Counts allocations, to compare what each decoder asks of the allocator.
struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const NS: &str = "records";
const NODES: TableDefinition<(&str, u128), &[u8]> = TableDefinition::new("nodes");
const EDGES: TableDefinition<(&str, u128), &[u8]> = TableDefinition::new("edges");

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The fixed fields of a node, its properties skipped.
#[derive(Deserialize)]
#[allow(dead_code)]
struct NodeHead {
    id: NodeId,
    labels: Vec<String>,
    #[serde(default)]
    props: IgnoredAny,
    #[serde(default)]
    sources: Vec<NodeId>,
    created_at: Timestamp,
    updated_at: Timestamp,
    #[serde(default)]
    recorded_by: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct EdgeHead {
    id: grafeno::EdgeId,
    label: String,
    from: NodeId,
    to: NodeId,
    #[serde(default)]
    props: IgnoredAny,
    #[serde(default)]
    sources: Vec<NodeId>,
    valid_from: Option<Timestamp>,
    valid_to: Option<Timestamp>,
    recorded_at: Timestamp,
    expired_at: Option<Timestamp>,
    #[serde(default)]
    recorded_by: Option<String>,
}

/// A binary header for a node's fixed fields, then its properties as JSON.
fn head_encode(node: &Node) -> Vec<u8> {
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(&node.id.as_u128().to_le_bytes());
    out.push(node.labels.len() as u8);
    for label in &node.labels {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.extend_from_slice(&node.created_at.as_micros().to_le_bytes());
    out.extend_from_slice(&node.updated_at.as_micros().to_le_bytes());
    serde_json::to_writer(&mut out, &node.props).unwrap();
    out
}

fn head_decode(
    bytes: &[u8],
) -> (
    NodeId,
    Vec<String>,
    Timestamp,
    Timestamp,
    serde_json::Map<String, serde_json::Value>,
) {
    let id = NodeId::from_u128(u128::from_le_bytes(bytes[..16].try_into().unwrap()));
    let mut at = 16;
    let count = bytes[at] as usize;
    at += 1;
    let mut labels = Vec::with_capacity(count);
    for _ in 0..count {
        let len = bytes[at] as usize;
        labels.push(String::from_utf8(bytes[at + 1..at + 1 + len].to_vec()).unwrap());
        at += 1 + len;
    }
    let created = Timestamp::from_micros(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()));
    let updated = Timestamp::from_micros(i64::from_le_bytes(
        bytes[at + 8..at + 16].try_into().unwrap(),
    ));
    let props = serde_json::from_slice(&bytes[at + 16..]).unwrap();
    (id, labels, created, updated, props)
}

/// Runs `f` and returns its time and allocations.
fn measure<T>(f: impl FnOnce() -> T) -> (T, Duration, u64) {
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let started = Instant::now();
    let out = f();
    (
        out,
        started.elapsed(),
        ALLOCATIONS.load(Ordering::Relaxed) - before,
    )
}

fn per(duration: Duration, count: usize) -> String {
    format!("{:.2}", duration.as_secs_f64() * 1e6 / count as f64)
}

fn main() {
    let path = std::env::temp_dir().join(format!("grafeno-records-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut rng = Rng(0x7EC0);
    let cities = [
        "Lima",
        "Quito",
        "Bogotá",
        "Santiago",
        "Montevideo",
        "La Paz",
    ];
    let (nodes, edges) = {
        let db = Grafeno::open(&path).unwrap();
        let mut nodes: Vec<NodeId> = Vec::new();
        for chunk in 0..10 {
            let made = db
                .write(|tx| {
                    (0..5_000)
                        .map(|i| {
                            let n = chunk * 5_000 + i;
                            tx.create_node(
                                NS,
                                NewNode::new(if n % 3 == 0 { "Person" } else { "Note" })
                                    .prop("name", format!("record {n}"))
                                    .prop("age", (n % 90) as i64)
                                    .prop("city", cities[n % cities.len()])
                                    .prop("score", n as f64 / 7.0)
                                    .prop("active", n % 2 == 0)
                                    .prop("tags", json!(["alpha", "beta", format!("t{}", n % 50)]))
                                    .prop("since", "2024-05-01T10:30:00Z"),
                            )
                        })
                        .collect::<grafeno::Result<Vec<_>>>()
                })
                .unwrap();
            nodes.extend(made);
        }
        let mut edges = Vec::new();
        for _ in 0..20 {
            let pairs: Vec<(NodeId, NodeId)> = (0..10_000)
                .map(|_| {
                    let a = nodes[(rng.next() % nodes.len() as u64) as usize];
                    let b = nodes[(rng.next() % nodes.len() as u64) as usize];
                    (a, b)
                })
                .collect();
            let made = db
                .write(|tx| {
                    pairs
                        .iter()
                        .map(|(a, b)| {
                            tx.create_edge(
                                NS,
                                NewEdge::new(*a, "knows", *b)
                                    .prop("weight", 0.5)
                                    .prop("context", "met at work")
                                    .valid_from(Timestamp::from_secs(1_700_000_000)),
                            )
                        })
                        .collect::<grafeno::Result<Vec<_>>>()
                })
                .unwrap();
            edges.extend(made);
        }
        (nodes, edges)
    };

    println!("{} nodes, {} edges\n", nodes.len(), edges.len());
    println!("## Access paths through the engine\n");
    println!("| path | items | µs each | allocations each |");
    println!("| --- | --- | --- | --- |");
    {
        let db = Grafeno::open(&path).unwrap();
        let read = db.begin_read().unwrap();
        // Warm the page cache: the comparison is decoding, not the disk.
        for id in &nodes {
            read.node(NS, *id).unwrap();
        }
        let (_, took, allocs) = measure(|| {
            for id in &nodes {
                read.node(NS, *id).unwrap().unwrap();
            }
        });
        println!(
            "| node by id | {} | {} | {} |",
            nodes.len(),
            per(took, nodes.len()),
            allocs / nodes.len() as u64
        );
        let (_, took, allocs) = measure(|| {
            for id in &edges {
                read.edge(NS, *id).unwrap().unwrap();
            }
        });
        println!(
            "| edge by id | {} | {} | {} |",
            edges.len(),
            per(took, edges.len()),
            allocs / edges.len() as u64
        );
        let starts = &nodes[..5_000];
        let ((count,), took, allocs) = measure(|| {
            let mut count = 0;
            for id in starts {
                count += read
                    .neighbors(NS, *id, Direction::Both, &EdgeFilter::any())
                    .unwrap()
                    .len();
            }
            (count,)
        });
        println!(
            "| neighbors with edge records | {count} | {} | {} |",
            per(took, count),
            allocs / count as u64
        );
        let ((count,), took, allocs) = measure(|| {
            let mut count = 0;
            for id in starts {
                count += read
                    .neighbor_ids(NS, *id, Direction::Both, &EdgeFilter::any())
                    .unwrap()
                    .len();
            }
            (count,)
        });
        println!(
            "| neighbor ids (adjacency only) | {count} | {} | {} |",
            per(took, count),
            allocs / count as u64
        );
        let ((count,), took, _) = measure(|| {
            let mut count = 0;
            for id in &starts[..500] {
                count += read
                    .k_hop(NS, *id, 2, Direction::Both, &EdgeFilter::any())
                    .unwrap()
                    .len();
            }
            (count,)
        });
        println!(
            "| k-hop depth 2 (adjacency only) | {count} | {} | |",
            per(took, count)
        );
        let ((count,), took, allocs) = measure(|| {
            let mut count = 0;
            for id in &starts[..500] {
                for (other, _) in read
                    .k_hop(NS, *id, 2, Direction::Both, &EdgeFilter::any())
                    .unwrap()
                {
                    read.node(NS, other).unwrap().unwrap();
                    count += 1;
                }
            }
            (count,)
        });
        println!(
            "| k-hop depth 2, each node read | {count} | {} | {} |",
            per(took, count),
            allocs / count as u64
        );
    }

    // The raw bytes, straight from the file.
    let db = redb::Database::open(&path).unwrap();
    let read = db.begin_read().unwrap();
    let node_table = read.open_table(NODES).unwrap();
    let edge_table = read.open_table(EDGES).unwrap();
    let fetch = |table: &redb::ReadOnlyTable<(&str, u128), &[u8]>, id: u128| {
        table.get((NS, id)).unwrap().unwrap().value().to_vec()
    };
    let (node_bytes, took, _) = measure(|| {
        nodes
            .iter()
            .map(|id| fetch(&node_table, id.as_u128()))
            .collect::<Vec<_>>()
    });
    let fetch_node = took;
    let (edge_bytes, took, _) = measure(|| {
        edges
            .iter()
            .map(|id| fetch(&edge_table, id.as_u128()))
            .collect::<Vec<_>>()
    });
    let fetch_edge = took;
    println!("\n## The same records fetched without decoding\n");
    println!("| | µs each |");
    println!("| --- | --- |");
    println!(
        "| node bytes by id (and a copy) | {} |",
        per(fetch_node, nodes.len())
    );
    println!(
        "| edge bytes by id (and a copy) | {} |",
        per(fetch_edge, edges.len())
    );

    let decoded_nodes: Vec<Node> = node_bytes
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    let decoded_edges: Vec<Edge> = edge_bytes
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();

    println!("\n## Encodings of the same records\n");
    println!(
        "| records | encoding | bytes each | decode µs each | decode allocations each | encode µs each |"
    );
    println!("| --- | --- | --- | --- | --- | --- |");
    let row = |records: &str,
               encoding: &str,
               sizes: usize,
               count: usize,
               decode: (Duration, u64),
               encode: Duration| {
        println!(
            "| {records} | {encoding} | {:.0} | {} | {} | {} |",
            sizes as f64 / count as f64,
            per(decode.0, count),
            decode.1 / count as u64,
            per(encode, count)
        );
    };
    // Nodes.
    let n = decoded_nodes.len();
    let (json, json_encode, _) = measure(|| {
        decoded_nodes
            .iter()
            .map(|x| serde_json::to_vec(x).unwrap())
            .collect::<Vec<_>>()
    });
    let (_, t, a) = measure(|| {
        json.iter()
            .map(|b| serde_json::from_slice::<Node>(b).unwrap())
            .for_each(|record| drop(std::hint::black_box(record)))
    });
    row(
        "nodes",
        "JSON (today)",
        json.iter().map(Vec::len).sum(),
        n,
        (t, a),
        json_encode,
    );
    let (cbor, cbor_encode, _) = measure(|| {
        decoded_nodes
            .iter()
            .map(|x| {
                let mut out = Vec::new();
                ciborium::into_writer(x, &mut out).unwrap();
                out
            })
            .collect::<Vec<_>>()
    });
    let (_, t, a) = measure(|| {
        cbor.iter()
            .map(|b| ciborium::from_reader::<Node, _>(b.as_slice()).unwrap())
            .for_each(|record| drop(std::hint::black_box(record)))
    });
    row(
        "nodes",
        "CBOR",
        cbor.iter().map(Vec::len).sum(),
        n,
        (t, a),
        cbor_encode,
    );
    let (_, t, a) = measure(|| {
        json.iter()
            .map(|b| serde_json::from_slice::<NodeHead>(b).unwrap())
            .for_each(|record| drop(std::hint::black_box(record)))
    });
    row(
        "nodes",
        "JSON, fixed fields only",
        json.iter().map(Vec::len).sum(),
        n,
        (t, a),
        json_encode,
    );
    let (heads, head_encode_time, _) =
        measure(|| decoded_nodes.iter().map(head_encode).collect::<Vec<_>>());
    let (_, t, a) = measure(|| {
        heads
            .iter()
            .map(|b| head_decode(b))
            .for_each(|record| drop(std::hint::black_box(record)))
    });
    row(
        "nodes",
        "binary header + JSON properties",
        heads.iter().map(Vec::len).sum(),
        n,
        (t, a),
        head_encode_time,
    );
    // Edges.
    let n = decoded_edges.len();
    let (json, json_encode, _) = measure(|| {
        decoded_edges
            .iter()
            .map(|x| serde_json::to_vec(x).unwrap())
            .collect::<Vec<_>>()
    });
    let (_, t, a) = measure(|| {
        json.iter()
            .map(|b| serde_json::from_slice::<Edge>(b).unwrap())
            .for_each(|record| drop(std::hint::black_box(record)))
    });
    row(
        "edges",
        "JSON (today)",
        json.iter().map(Vec::len).sum(),
        n,
        (t, a),
        json_encode,
    );
    let (cbor, cbor_encode, _) = measure(|| {
        decoded_edges
            .iter()
            .map(|x| {
                let mut out = Vec::new();
                ciborium::into_writer(x, &mut out).unwrap();
                out
            })
            .collect::<Vec<_>>()
    });
    let (_, t, a) = measure(|| {
        cbor.iter()
            .map(|b| ciborium::from_reader::<Edge, _>(b.as_slice()).unwrap())
            .for_each(|record| drop(std::hint::black_box(record)))
    });
    row(
        "edges",
        "CBOR",
        cbor.iter().map(Vec::len).sum(),
        n,
        (t, a),
        cbor_encode,
    );
    let (_, t, a) = measure(|| {
        json.iter()
            .map(|b| serde_json::from_slice::<EdgeHead>(b).unwrap())
            .for_each(|record| drop(std::hint::black_box(record)))
    });
    row(
        "edges",
        "JSON, fixed fields only",
        json.iter().map(Vec::len).sum(),
        n,
        (t, a),
        json_encode,
    );

    drop(edge_table);
    drop(node_table);
    drop(read);
    drop(db);
    let _ = std::fs::remove_file(&path);
}
