//! `grafeno storage …`: what a database takes, and bringing its storage up
//! to date (docs/roadmap/STORAGE_AND_INDEXING_OPTIMIZATION.md). The
//! commands open the file itself: the server using it must be stopped
//! (a running server answers `GET /v1/storage` and
//! `/v1/storage/migrations` instead).

use std::path::{Path, PathBuf};
use std::time::Instant;

use grafeno::{Encoding, Grafeno, Migration, MigrationOptions, MigrationState, RunControl};
use serde_json::json;

/// What `plan`, `apply` and `verify` were asked to do.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub namespaces: Vec<String>,
    pub reindex: bool,
    pub encoding: Option<Encoding>,
    pub verify: bool,
    pub compact: bool,
    pub batch_size: usize,
    pub backup: bool,
    pub json: bool,
    pub retrain: bool,
    pub indexing: Option<grafeno::IndexingPolicy>,
}

fn open(path: &Path) -> Result<Grafeno, String> {
    open_as(path, grafeno::Compression::Auto)
}

/// Going back to JSON, nothing the tool writes meanwhile may compress.
fn open_for(path: &Path, request: &Request) -> Result<Grafeno, String> {
    open_as(
        path,
        if request.encoding == Some(Encoding::Json) {
            grafeno::Compression::Off
        } else {
            grafeno::Compression::Auto
        },
    )
}

fn open_as(path: &Path, compression: grafeno::Compression) -> Result<Grafeno, String> {
    Grafeno::open_with(
        path,
        &grafeno::OpenOptions {
            copy_before_upgrade: false,
            compression,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())
}

/// What the file takes closed: redb grows a file without free pages when
/// it opens it and trims that when it closes it, so the length read while
/// it is open (and the free pages in it) would overstate both.
pub fn closed(length: Option<u64>, mut usage: grafeno::StorageUsage) -> grafeno::StorageUsage {
    if let (Some(closed), Some(open)) = (length, usage.file_bytes)
        && closed < open
    {
        usage.file_bytes = Some(closed);
        usage.free_bytes = usage
            .allocated_bytes
            .map(|used| closed.saturating_sub(used));
    }
    usage
}

pub fn length(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

fn options(request: &Request) -> MigrationOptions {
    MigrationOptions {
        namespaces: request.namespaces.clone(),
        encoding: request.encoding,
        reindex: request.reindex,
        verify: request.verify,
        batch_size: request.batch_size,
        retrain: request.retrain,
        indexing: request.indexing.clone(),
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn print_steps(migration: &Migration) {
    println!(
        "migration {} ({:?}), engine {}, layout {}",
        migration.id, migration.state, migration.engine, migration.layout
    );
    for step in &migration.steps {
        let mut line = format!(
            "  {:<9} {:<24} {:>10} {:>10}  {:?}",
            format!("{:?}", step.kind).to_lowercase(),
            step.namespace,
            step.estimate,
            step.done,
            step.state
        );
        if let Some(result) = &step.result {
            line.push_str(&format!("  {result}"));
        }
        if let Some(error) = &step.error {
            line.push_str(&format!("  {error}"));
        }
        println!("{line}");
    }
}

/// `grafeno storage status`: the file, what is pending, the migrations.
pub fn status(path: &Path, json: bool) -> Result<(), String> {
    let closed_length = length(path);
    let db = open(path)?;
    let usage = closed(
        closed_length,
        db.storage_usage().map_err(|e| e.to_string())?,
    );
    let pending = db.pending_work().map_err(|e| e.to_string())?;
    let migrations = db
        .begin_read()
        .and_then(|r| r.migrations())
        .map_err(|e| e.to_string())?;
    let dictionaries = db.dictionaries().map_err(|e| e.to_string())?;
    if json {
        let out = json!({
            "file_bytes": usage.file_bytes,
            "stored_bytes": usage.stored_bytes,
            "fragmented_bytes": usage.fragmented_bytes,
            "free_bytes": usage.free_bytes,
            "layout": usage.layout,
            "pending": pending,
            "compression": db.compression(),
            "dictionaries": dictionaries,
            "migrations": migrations,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!(
        "{}: {:.1} MiB, {:.1} MiB stored, {:.1} MiB lost inside pages, {:.1} MiB free; layout {}",
        path.display(),
        mib(usage.file_bytes.unwrap_or(0)),
        mib(usage.stored_bytes),
        mib(usage.fragmented_bytes),
        mib(usage.free_bytes.unwrap_or(0)),
        usage.layout
    );
    if pending.is_empty() {
        println!("Nothing pending.");
    }
    for item in &pending {
        println!("pending: {}", item.detail);
    }
    println!(
        "compression {}; {} dictionaries ({:.1} KiB)",
        match db.compression() {
            grafeno::Compression::Auto => "auto",
            grafeno::Compression::Off => "off",
        },
        dictionaries.len(),
        dictionaries.iter().map(|d| d.bytes).sum::<usize>() as f64 / 1024.0
    );
    for d in &dictionaries {
        println!(
            "  dictionary {} {:<24} {:<12} {:>6} bytes, {} samples",
            d.id,
            if d.namespace.is_empty() {
                "(file)"
            } else {
                d.namespace.as_str()
            },
            format!("{:?}", d.kind).to_lowercase(),
            d.bytes,
            d.samples
        );
    }
    for migration in &migrations {
        print_steps(migration);
    }
    Ok(())
}

/// `grafeno storage plan`: what `apply` would do; changes nothing.
pub fn plan(path: &Path, request: &Request) -> Result<(), String> {
    let db = open(path)?;
    let plan = db
        .plan_migration(&options(request))
        .map_err(|e| e.to_string())?;
    if request.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!("A dry run: nothing changes. Estimates of records to rewrite come from a sample.");
    print_steps(&plan);
    for before in &plan.before {
        println!(
            "  {} holds {} nodes, {} edges, {} hyperedges (digest {})",
            before.namespace, before.nodes, before.edges, before.hyperedges, before.digest
        );
    }
    if matches!(
        request.encoding,
        Some(Encoding::Envelope | Encoding::Compressed)
    ) {
        println!(
            "Values in envelopes are read by this version and later ones only: the file becomes layout {}, which an older Grafeno refuses (`--encoding json` over the whole file turns it back).",
            grafeno::SCHEMA_VERSION
        );
    }
    if request.compact {
        println!("Then `vacuum --maximum`, with the file closed.");
    }
    Ok(())
}

/// `grafeno storage apply`: a backup first, the migration in batches, a
/// compaction if asked, and a report.
pub fn apply(path: &Path, request: &Request) -> Result<(), String> {
    let started = Instant::now();
    let closed_length = length(path);
    let db = open_for(path, request)?;
    let before = closed(
        closed_length,
        db.storage_usage().map_err(|e| e.to_string())?,
    );
    let plan = db
        .plan_migration(&options(request))
        .map_err(|e| e.to_string())?;
    let backup = if request.backup {
        let copy = PathBuf::from(format!("{}.before-{}.redb", path.display(), plan.id));
        db.begin_read()
            .and_then(|read| read.backup(&copy))
            .map_err(|e| format!("the backup before the migration failed: {e}"))?;
        let check = open(&copy)?;
        for ns in &plan.before {
            let report = check
                .begin_read()
                .and_then(|r| r.check(&ns.namespace))
                .map_err(|e| e.to_string())?;
            if !report.is_ok() {
                return Err(format!(
                    "the backup {} does not check out: nothing was migrated",
                    copy.display()
                ));
            }
        }
        Some(copy)
    } else {
        None
    };
    db.start_migration(&plan).map_err(|e| e.to_string())?;
    let done = db
        .run_migration(
            &plan.id,
            &RunControl {
                exclusive: true,
                ..RunControl::default()
            },
        )
        .map_err(|e| e.to_string())?;
    drop(db);
    let compacted = if request.compact && done.state == MigrationState::Completed {
        Some(Grafeno::vacuum_with(path, true).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let after = open(path)?.storage_usage().map_err(|e| e.to_string())?;
    let after = closed(length(path), after);
    report(
        &done,
        &before,
        &after,
        backup.as_deref(),
        compacted.is_some(),
        started,
        request.json,
    )?;
    if done.state != MigrationState::Completed {
        return Err(format!("the migration ended {:?}", done.state));
    }
    Ok(())
}

/// `grafeno storage resume`: the paused or interrupted migration, to its
/// end.
pub fn resume(path: &Path, json: bool) -> Result<(), String> {
    let started = Instant::now();
    let closed_length = length(path);
    let db = open(path)?;
    let before = closed(
        closed_length,
        db.storage_usage().map_err(|e| e.to_string())?,
    );
    let Some(waiting) = db
        .begin_read()
        .and_then(|r| r.migrations())
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|m| matches!(m.state, MigrationState::Paused | MigrationState::InProgress))
    else {
        println!("No migration waits.");
        return Ok(());
    };
    let done = db
        .run_migration(
            &waiting.id,
            &RunControl {
                exclusive: true,
                ..RunControl::default()
            },
        )
        .map_err(|e| e.to_string())?;
    let after = db.storage_usage().map_err(|e| e.to_string())?;
    drop(db);
    let after = closed(length(path), after);
    report(&done, &before, &after, None, false, started, json)
}

#[allow(clippy::too_many_arguments)]
fn report(
    done: &Migration,
    before: &grafeno::StorageUsage,
    after: &grafeno::StorageUsage,
    backup: Option<&Path>,
    compacted: bool,
    started: Instant,
    json: bool,
) -> Result<(), String> {
    let seconds = started.elapsed().as_secs_f64();
    if json {
        let out = json!({
            "migration": done,
            "before": {"file_bytes": before.file_bytes, "stored_bytes": before.stored_bytes, "fragmented_bytes": before.fragmented_bytes},
            "after": {"file_bytes": after.file_bytes, "stored_bytes": after.stored_bytes, "fragmented_bytes": after.fragmented_bytes},
            "backup": backup.map(|p| p.display().to_string()),
            "compacted": compacted,
            "seconds": seconds,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    print_steps(done);
    println!(
        "file {:.1} MiB → {:.1} MiB, stored {:.1} → {:.1} MiB, lost inside pages {:.1} → {:.1} MiB{}; {:.1} s",
        mib(before.file_bytes.unwrap_or(0)),
        mib(after.file_bytes.unwrap_or(0)),
        mib(before.stored_bytes),
        mib(after.stored_bytes),
        mib(before.fragmented_bytes),
        mib(after.fragmented_bytes),
        if compacted { ", compacted" } else { "" },
        seconds
    );
    if let Some(backup) = backup {
        println!(
            "The file as it was is in {}: restoring it loses what was written after.",
            backup.display()
        );
    }
    Ok(())
}
