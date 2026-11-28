//! Lossless compression of stored vectors (docs/roadmap/
//! STORAGE_AND_INDEXING_OPTIMIZATION.md, phase 10.2): `f32` vectors as
//! stored (normalized), each compressed on its own with Zstandard, as is
//! and with its bytes shuffled (the four bytes of every value in four
//! planes, so that signs and exponents sit together), against what reading
//! one back costs next to the distance a search computes with it.
//!
//! `cargo run --release -p grafeno --example vector_compression` (`CSV=path`)

use std::hint::black_box;
use std::time::Instant;

/// A normal sample (Box-Muller) from a small xorshift generator.
struct Normal(u64);

impl Normal {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn sample(&mut self) -> f32 {
        let (a, b) = (self.uniform(), self.uniform());
        ((-2.0 * a.ln()).sqrt() * (2.0 * std::f64::consts::PI * b).cos()) as f32
    }
}

/// `count` normalized vectors of `dims` values: near-normal values, as
/// embedding models give.
fn vectors(dims: usize, count: usize) -> Vec<Vec<f32>> {
    let mut normal = Normal(0x9E37_79B9_7F4A_7C15 ^ dims as u64);
    (0..count)
        .map(|_| {
            let v: Vec<f32> = (0..dims).map(|_| normal.sample()).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.into_iter().map(|x| x / norm).collect()
        })
        .collect()
}

fn bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// The bytes in planes: every value's first byte, then every second byte…
fn shuffled(v: &[f32]) -> Vec<u8> {
    let raw = bytes(v);
    let n = v.len();
    let mut out = vec![0u8; raw.len()];
    for (i, chunk) in raw.as_chunks::<4>().0.iter().enumerate() {
        for (plane, byte) in chunk.iter().enumerate() {
            out[plane * n + i] = *byte;
        }
    }
    out
}

fn unshuffled(planes: &[u8]) -> Vec<f32> {
    let n = planes.len() / 4;
    (0..n)
        .map(|i| {
            f32::from_le_bytes([
                planes[i],
                planes[n + i],
                planes[2 * n + i],
                planes[3 * n + i],
            ])
        })
        .collect()
}

fn main() {
    let count = 20_000;
    let mut csv = String::from(
        "dims,vectors,raw_bytes,zstd_bytes,shuffled_zstd_bytes,decode_us,distance_us\n",
    );
    println!(
        "{:>5} {:>10} {:>12} {:>14} {:>11} {:>12}",
        "dims", "raw bytes", "zstd", "shuffled+zstd", "decode µs", "distance µs"
    );
    for dims in [64, 384, 768] {
        let all = vectors(dims, count);
        let raw: usize = all.iter().map(|v| v.len() * 4).sum();
        let mut compressor = zstd::bulk::Compressor::new(3).unwrap();
        let plain: usize = all
            .iter()
            .map(|v| compressor.compress(&bytes(v)).unwrap().len())
            .sum();
        let packed: Vec<Vec<u8>> = all
            .iter()
            .map(|v| compressor.compress(&shuffled(v)).unwrap())
            .collect();
        let shuffled_total: usize = packed.iter().map(Vec::len).sum();
        // Reading one back: decompress and unshuffle.
        let mut decompressor = zstd::bulk::Decompressor::new().unwrap();
        let started = Instant::now();
        for frame in &packed {
            let planes = decompressor.decompress(frame, dims * 4).unwrap();
            black_box(unshuffled(&planes));
        }
        let decode_us = started.elapsed().as_secs_f64() * 1e6 / count as f64;
        // What a search does with one: a dot product.
        let query = &all[0];
        let started = Instant::now();
        let mut sum = 0f32;
        for v in &all {
            sum += v.iter().zip(query).map(|(a, b)| a * b).sum::<f32>();
        }
        black_box(sum);
        let distance_us = started.elapsed().as_secs_f64() * 1e6 / count as f64;
        // Lossless: every vector reads back the same.
        for (v, frame) in all.iter().zip(&packed).take(100) {
            let planes = decompressor.decompress(frame, dims * 4).unwrap();
            assert_eq!(&unshuffled(&planes), v);
        }
        println!(
            "{dims:>5} {raw:>10} {:>11.1}% {:>13.1}% {decode_us:>11.2} {distance_us:>12.3}",
            100.0 * plain as f64 / raw as f64,
            100.0 * shuffled_total as f64 / raw as f64
        );
        csv.push_str(&format!(
            "{dims},{count},{raw},{plain},{shuffled_total},{decode_us:.3},{distance_us:.4}\n"
        ));
    }
    if let Ok(path) = std::env::var("CSV") {
        std::fs::write(path, csv).unwrap();
    }
}
