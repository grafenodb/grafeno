//! What each table of a database file takes on disk: entries, bytes
//! stored in leaves, metadata and fragmented space, and pages, largest
//! first. The file is opened read-only (stop the server holding it, or
//! read a copy).
//!
//! `cargo run --release -p grafeno --example table_sizes -- grafeno.redb`

use redb::{ReadableDatabase, ReadableTableMetadata, TableHandle};

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "grafeno.redb".into());
    let file = std::fs::metadata(&path)?.len();
    let db = redb::ReadOnlyDatabase::open(&path)?;
    let read = db.begin_read()?;
    let mut rows = Vec::new();
    for handle in read.list_tables()? {
        let name = handle.name().to_owned();
        let table = read.open_untyped_table(handle)?;
        let stats = table.stats()?;
        rows.push((
            name,
            table.len()?,
            stats.stored_bytes(),
            stats.metadata_bytes(),
            stats.fragmented_bytes(),
            stats.leaf_pages() + stats.branch_pages(),
        ));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.2 + row.3 + row.4));
    println!(
        "{:<28} {:>12} {:>12} {:>10} {:>12} {:>10}",
        "table", "entries", "stored MiB", "meta MiB", "fragment MiB", "pages"
    );
    let (mut stored, mut meta, mut fragmented) = (0, 0, 0);
    for (name, entries, s, m, f, pages) in &rows {
        stored += s;
        meta += m;
        fragmented += f;
        println!(
            "{name:<28} {entries:>12} {:>12.1} {:>10.1} {:>12.1} {pages:>10}",
            mib(*s),
            mib(*m),
            mib(*f)
        );
    }
    println!(
        "{:<28} {:>12} {:>12.1} {:>10.1} {:>12.1}",
        "all tables",
        "",
        mib(stored),
        mib(meta),
        mib(fragmented)
    );
    println!("file: {:.1} MiB", mib(file));
    Ok(())
}
