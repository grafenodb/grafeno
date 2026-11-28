//! Vector links by number (phase 10 of the storage roadmap).
//!
//! In a space whose links are numbered, each vector gets a number the
//! first time its node is inserted there (`next_number` of the space's
//! metadata), kept both ways in `vector_numbers` (space, node -> number)
//! and `vector_ids` (space, number -> node). A node keeps its number while
//! the space lives, even with its vector removed, so a link that dangles
//! reads back as the same node in every process; compacting or clearing
//! the space gives every vector a new number. Neighbor lists are stored as
//! varints of numbers, in their order, instead of 16-byte ids.
//!
//! Numbers are immutable facts within a space's epoch (a counter of the
//! file, taken whenever a space starts over), so a cache shared by every
//! reader maps numbers back to nodes per space and epoch, filled from
//! committed snapshots only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use grafeno_core::{Error, Result};
use redb::{ReadableTable, TableDefinition, TableHandle, WriteTransaction};

use crate::store::storage;

/// `(space, node) -> number`.
pub(crate) const VECTOR_NUMBERS: TableDefinition<(&str, u128), u32> =
    TableDefinition::new("vector_numbers");

/// `(space, number) -> node`.
pub(crate) const VECTOR_IDS: TableDefinition<(&str, u32), u128> =
    TableDefinition::new("vector_ids");

/// The `meta` key of the last epoch given to a space.
const EPOCH_KEY: &str = "vector_epoch";

/// How a space stores its neighbor lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkFormat {
    /// Varints of numbers per space (phase 10).
    #[default]
    Numbers,
    /// 16-byte node ids (before phase 10).
    Ids,
}

impl LinkFormat {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "numbers" => Some(LinkFormat::Numbers),
            "ids" => Some(LinkFormat::Ids),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            LinkFormat::Numbers => "numbers",
            LinkFormat::Ids => "ids",
        }
    }
}

pub(crate) fn put_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn take_varint(bytes: &mut &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        value |= u32::from(byte & 0x7F).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The numbers of a stored list, in order.
pub(crate) fn numbers_of(mut bytes: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(bytes.len() / 2);
    while !bytes.is_empty() {
        let Some(number) = take_varint(&mut bytes) else {
            break;
        };
        out.push(number);
    }
    out
}

/// A list of numbers as stored.
pub(crate) fn list_bytes(numbers: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(numbers.len() * 3);
    for number in numbers {
        put_varint(&mut out, *number);
    }
    out
}

/// The nodes of a space's numbers, as committed: index = number, 0 =
/// not known yet (node ids are never 0).
#[derive(Default)]
pub(crate) struct SpaceNumbers {
    ids: RwLock<Vec<u128>>,
    /// The whole space was read once.
    loaded: std::sync::atomic::AtomicBool,
}

impl SpaceNumbers {
    /// The nodes of `numbers` it knows, in place (0 where it does not):
    /// one lock for the whole list.
    fn fill(&self, numbers: &[u32], out: &mut Vec<u128>) {
        let ids = self.ids.read().unwrap_or_else(|e| e.into_inner());
        out.clear();
        out.extend(
            numbers
                .iter()
                .map(|n| ids.get(*n as usize).copied().unwrap_or(0)),
        );
    }

    fn get(&self, number: u32) -> Option<u128> {
        let ids = self.ids.read().unwrap_or_else(|e| e.into_inner());
        ids.get(number as usize).copied().filter(|id| *id != 0)
    }

    fn put(&self, number: u32, id: u128) {
        let mut ids = self.ids.write().unwrap_or_else(|e| e.into_inner());
        let at = number as usize;
        if ids.len() <= at {
            ids.resize(at + 1, 0);
        }
        ids[at] = id;
    }
}

/// The caches of every space, at its latest epoch, shared by readers.
#[derive(Default)]
pub(crate) struct Cache {
    spaces: Mutex<HashMap<String, (u64, Arc<SpaceNumbers>)>>,
}

/// Spaces and epochs kept at most; the oldest kept are not tracked, any
/// one goes.
const MAX_SPACES: usize = 256;

impl Cache {
    /// The cache of `space` at `epoch`. An older epoch than the one kept
    /// (a reader from before a compaction) gets one of its own, not kept.
    pub(crate) fn space(&self, space: &str, epoch: u64) -> Arc<SpaceNumbers> {
        let mut spaces = self.spaces.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((kept, found)) = spaces.get(space) {
            if *kept == epoch {
                return found.clone();
            }
            if *kept > epoch {
                return Arc::new(SpaceNumbers::default());
            }
        }
        if spaces.len() >= MAX_SPACES {
            spaces.clear();
        }
        let fresh = Arc::new(SpaceNumbers::default());
        spaces.insert(space.to_owned(), (epoch, fresh.clone()));
        fresh
    }
}

/// Maps a space's numbers back to nodes while its lists are read.
pub(crate) trait Resolve {
    fn id(&self, number: u32) -> Result<Option<u128>>;

    /// The nodes of a list's numbers, in place: 0 for a number no node
    /// has.
    fn ids(&self, numbers: &[u32], out: &mut Vec<u128>) -> Result<()> {
        out.clear();
        for number in numbers {
            out.push(self.id(*number)?.unwrap_or(0));
        }
        Ok(())
    }
}

/// From a snapshot: the shared cache, then the table, whose answers go
/// into the cache when the snapshot is committed (not a write
/// transaction's, which may not commit).
pub(crate) struct Snapshot<'a, T> {
    pub(crate) space: &'a str,
    pub(crate) numbers: Arc<SpaceNumbers>,
    pub(crate) table: Option<&'a T>,
    pub(crate) committed: bool,
}

impl<'a, T: ReadableTable<(&'static str, u32), u128>> Snapshot<'a, T> {
    /// The resolver of `space` in its `epoch`, the space read whole into
    /// the cache the first time (cheaper than a lookup per number).
    pub(crate) fn new(
        cache: &Cache,
        space: &'a str,
        epoch: u64,
        table: Option<&'a T>,
        committed: bool,
    ) -> Result<Self> {
        let numbers = cache.space(space, epoch);
        if committed
            && let Some(table) = table
            && !numbers.loaded.load(std::sync::atomic::Ordering::Relaxed)
        {
            let mut found = Vec::new();
            for item in table
                .range((space, 0u32)..=(space, u32::MAX))
                .map_err(storage)?
            {
                let (key, value) = item.map_err(storage)?;
                found.push((key.value().1, value.value()));
            }
            {
                let mut ids = numbers.ids.write().unwrap_or_else(|e| e.into_inner());
                let top = found
                    .iter()
                    .map(|(n, _)| *n as usize + 1)
                    .max()
                    .unwrap_or(0);
                if ids.len() < top {
                    ids.resize(top, 0);
                }
                for (number, id) in found {
                    ids[number as usize] = id;
                }
            }
            numbers
                .loaded
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(Self {
            space,
            numbers,
            table,
            committed,
        })
    }
}

impl<T: ReadableTable<(&'static str, u32), u128>> Resolve for Snapshot<'_, T> {
    fn ids(&self, numbers: &[u32], out: &mut Vec<u128>) -> Result<()> {
        self.numbers.fill(numbers, out);
        for (at, number) in numbers.iter().enumerate() {
            if out[at] == 0 {
                out[at] = self.id(*number)?.unwrap_or(0);
            }
        }
        Ok(())
    }

    fn id(&self, number: u32) -> Result<Option<u128>> {
        if let Some(id) = self.numbers.get(number) {
            return Ok(Some(id));
        }
        let Some(table) = self.table else {
            return Ok(None);
        };
        let found = table
            .get((self.space, number))
            .map_err(storage)?
            .map(|guard| guard.value());
        if let Some(id) = found
            && self.committed
        {
            self.numbers.put(number, id);
        }
        Ok(found)
    }
}

/// Inside a write transaction: the shared cache (committed facts), then
/// the transaction's table, whose answers stay out of the cache (they may
/// not commit).
pub(crate) struct Writing<'a> {
    pub(crate) space: &'a str,
    pub(crate) numbers: Arc<SpaceNumbers>,
    pub(crate) table: Option<redb::Table<'a, (&'static str, u32), u128>>,
}

impl<'a> Writing<'a> {
    pub(crate) fn new(
        tx: &'a WriteTransaction,
        cache: &Cache,
        space: &'a str,
        epoch: u64,
    ) -> Result<Self> {
        // A numbered space's tables exist from its first vector on.
        let table = Some(tx.open_table(VECTOR_IDS).map_err(storage)?);
        Ok(Self {
            space,
            numbers: cache.space(space, epoch),
            table,
        })
    }
}

impl Resolve for Writing<'_> {
    fn ids(&self, numbers: &[u32], out: &mut Vec<u128>) -> Result<()> {
        self.numbers.fill(numbers, out);
        for (at, number) in numbers.iter().enumerate() {
            if out[at] == 0 {
                out[at] = self.id(*number)?.unwrap_or(0);
            }
        }
        Ok(())
    }

    fn id(&self, number: u32) -> Result<Option<u128>> {
        if let Some(id) = self.numbers.get(number) {
            return Ok(Some(id));
        }
        let Some(table) = &self.table else {
            return Ok(None);
        };
        Ok(table
            .get((self.space, number))
            .map_err(storage)?
            .map(|guard| guard.value()))
    }
}

/// The nodes of a stored list of numbers, in order (numbers no node has
/// are left out).
pub(crate) fn ids_of(bytes: &[u8], resolve: &dyn Resolve) -> Result<Vec<u128>> {
    let numbers = numbers_of(bytes);
    let mut out = Vec::with_capacity(numbers.len());
    resolve.ids(&numbers, &mut out)?;
    out.retain(|id| *id != 0);
    Ok(out)
}

/// The number of `id` in `space`, given now (from the space's metadata)
/// if it has none. `known` caches them for the transaction.
pub(crate) fn number_of(
    tx: &WriteTransaction,
    space: &str,
    id: u128,
    next: &mut u32,
    known: &mut HashMap<u128, u32>,
    back: &mut HashMap<u32, u128>,
) -> Result<u32> {
    if let Some(number) = known.get(&id) {
        return Ok(*number);
    }
    let mut numbers = tx.open_table(VECTOR_NUMBERS).map_err(storage)?;
    let found = numbers
        .get((space, id))
        .map_err(storage)?
        .map(|guard| guard.value());
    let number = match found {
        Some(number) => number,
        None => {
            let number = *next;
            *next = next
                .checked_add(1)
                .ok_or_else(|| Error::Storage(format!("{space:?} ran out of vector numbers")))?;
            numbers.insert((space, id), number).map_err(storage)?;
            tx.open_table(VECTOR_IDS)
                .map_err(storage)?
                .insert((space, number), id)
                .map_err(storage)?;
            number
        }
    };
    known.insert(id, number);
    back.insert(number, id);
    Ok(number)
}

/// The number of `id` in `space` if it has one (no number is given).
pub(crate) fn existing_number(
    tx: &WriteTransaction,
    space: &str,
    id: u128,
    known: &mut HashMap<u128, u32>,
) -> Result<Option<u32>> {
    if let Some(number) = known.get(&id) {
        return Ok(Some(*number));
    }
    // (Only numbered spaces ask, and the table exists from the first.)
    let found = tx
        .open_table(VECTOR_NUMBERS)
        .map_err(storage)?
        .get((space, id))
        .map_err(storage)?
        .map(|guard| guard.value());
    if let Some(number) = found {
        known.insert(id, number);
    }
    Ok(found)
}

/// A new epoch for a space starting over.
pub(crate) fn next_epoch(tx: &WriteTransaction) -> Result<u64> {
    let mut meta = tx.open_table(crate::store::META).map_err(storage)?;
    let epoch = meta
        .get(EPOCH_KEY)
        .map_err(storage)?
        .map_or(0, |guard| guard.value())
        + 1;
    meta.insert(EPOCH_KEY, epoch).map_err(storage)?;
    Ok(epoch)
}

/// Removes the numbers of every space from `low` (included) to `high`
/// (excluded), when the file has them.
pub(crate) fn clear_range(tx: &WriteTransaction, low: &str, high: Option<&str>) -> Result<()> {
    if !crate::compress::has_table(tx, redb::TableHandle::name(&VECTOR_NUMBERS))? {
        return Ok(());
    }
    let inside = |space: &str| space >= low && high.is_none_or(|high| space < high);
    match high {
        Some(high) => {
            tx.open_table(VECTOR_NUMBERS)
                .map_err(storage)?
                .retain_in((low, 0u128)..(high, 0u128), |_, _| false)
                .map_err(storage)?;
            tx.open_table(VECTOR_IDS)
                .map_err(storage)?
                .retain_in((low, 0u32)..(high, 0u32), |_, _| false)
                .map_err(storage)?;
        }
        None => {
            tx.open_table(VECTOR_NUMBERS)
                .map_err(storage)?
                .retain_in((low, 0u128).., |key, _| !inside(key.0))
                .map_err(storage)?;
            tx.open_table(VECTOR_IDS)
                .map_err(storage)?
                .retain_in((low, 0u32).., |key, _| !inside(key.0))
                .map_err(storage)?;
        }
    }
    Ok(())
}

/// Removes the numbers of one space.
pub(crate) fn clear_space(tx: &WriteTransaction, space: &str) -> Result<()> {
    if !crate::compress::has_table(tx, redb::TableHandle::name(&VECTOR_NUMBERS))? {
        return Ok(());
    }
    tx.open_table(VECTOR_NUMBERS)
        .map_err(storage)?
        .retain_in((space, 0u128)..=(space, u128::MAX), |_, _| false)
        .map_err(storage)?;
    tx.open_table(VECTOR_IDS)
        .map_err(storage)?
        .retain_in((space, 0u32)..=(space, u32::MAX), |_, _| false)
        .map_err(storage)?;
    Ok(())
}

impl crate::WriteTx {
    /// `vector_ids` as written so far, when the file has it.
    pub(crate) fn numbers_table(
        &self,
    ) -> Result<Option<redb::Table<'_, (&'static str, u32), u128>>> {
        if !crate::compress::has_table(&self.tx, redb::TableHandle::name(&VECTOR_IDS))? {
            return Ok(None);
        }
        Ok(Some(self.tx.open_table(VECTOR_IDS).map_err(storage)?))
    }
}

impl crate::WriteTx {
    /// When no vector space keeps numbered links any more: drops
    /// `vector_numbers` and `vector_ids` and gives the file the layout of
    /// what is left. `Err` says why not.
    pub(crate) fn leave_numbers_layout(&mut self) -> Result<std::result::Result<u64, String>> {
        {
            let metas = self
                .tx
                .open_table(crate::store::VECTOR_META)
                .map_err(storage)?;
            for item in metas.iter().map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                let meta: crate::vector::VectorMeta = crate::store::decode(value.value())?;
                if meta.numbered() {
                    return Ok(Err(format!(
                        "{:?} keeps its vector links by number",
                        key.value().replace('\0', " ")
                    )));
                }
            }
        }
        for table in [VECTOR_NUMBERS.name(), VECTOR_IDS.name()] {
            if crate::compress::has_table(&self.tx, table)? {
                if table == VECTOR_NUMBERS.name() {
                    self.tx.delete_table(VECTOR_NUMBERS).map_err(storage)?;
                } else {
                    self.tx.delete_table(VECTOR_IDS).map_err(storage)?;
                }
            }
        }
        Ok(Ok(crate::db::settle_layout(&self.tx)?))
    }
}

impl crate::ReadTx {
    /// The vector spaces of `ns` (its embeddings, its vector indexes with a
    /// graph of their own) and whether each keeps numbered links.
    pub(crate) fn vector_spaces(&self, ns: &str) -> Result<Vec<(String, bool)>> {
        let mut spaces = vec![ns.to_owned()];
        for def in self.index_defs(ns)? {
            if def.vector.is_some() && !def.on_embedding() {
                spaces.push(crate::vindex::space_of(ns, &def.name));
            }
        }
        let mut out = Vec::new();
        for space in spaces {
            if let Some(meta) = crate::vector::read_meta(&self.vector_meta, &space)? {
                out.push((space, meta.numbered()));
            }
        }
        Ok(out)
    }
}
