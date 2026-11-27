//! Lossless compression of stored values (docs/roadmap/
//! STORAGE_AND_INDEXING_OPTIMIZATION.md, phase 5).
//!
//! Records are small (a catalogue node takes about 500 bytes of JSON, a
//! change log entry 170): compressed one by one, without help, they barely
//! shrink. Zstandard with a dictionary trained on a sample of the same kind
//! of records of the same namespace takes them to 15–40% of their size and
//! is faster than without one (`docs/benchmarks.md`, "Compression").
//!
//! - A value is compressed with the dictionary of its namespace and kind
//!   when there is one, with Zstandard alone when it is large enough, and
//!   kept only if it saves [`MIN_SAVING`]; otherwise it is stored as it is.
//! - Dictionaries live in the `dictionaries` table under an id that the
//!   envelope of each value names: a new dictionary never makes the values
//!   an older one compressed unreadable. They are trained off the write
//!   path (a migration, or the server in the background) and only used for
//!   writes once the transaction that stored them has committed.
//! - Every value compressed is decompressed again and compared before it is
//!   written: a value that would not read back is stored as it is.
//! - Dictionaries are physical, per member, like the rest of the file's
//!   encoding: replication carries records, not their bytes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, RwLock};

use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::store::{ENVELOPE, ENVELOPE_VERSION, put_varint, storage};
use crate::{Error, Result};

/// `id -> [header length: u32 LE][header JSON][dictionary]`.
pub(crate) const DICTIONARIES: TableDefinition<u64, &[u8]> = TableDefinition::new("dictionaries");

/// Values shorter than this are stored as they are.
pub(crate) const MIN_DICTIONARY_VALUE: usize = 32;
/// Without a dictionary, values shorter than this are stored as they are:
/// Zstandard alone takes a 500-byte record to 72% for 7 µs a write (with
/// the check that it reads back), a dictionary to 15% for 2.4.
pub(crate) const MIN_PLAIN_VALUE: usize = 1024;
/// A compressed value is kept when it takes at most this share of the
/// original (in hundredths): it saves at least 12%.
pub(crate) const MIN_SAVING: usize = 88;
/// Zstandard's level: 1 compresses these values as well as 3, faster.
const LEVEL: i32 = 1;
/// A dictionary's size at most.
pub(crate) const DICTIONARY_BYTES: usize = 64 * 1024;

/// Whether writes compress values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    /// Compress the values worth it (the default).
    #[default]
    Auto,
    /// Write values as they are (JSON, or a hyperedge's binary record).
    Off,
}

/// The kinds of values a dictionary is trained on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredKind {
    Node,
    Edge,
    #[serde(rename = "hyperedge")]
    HyperEdge,
    Text,
    Change,
    /// The audit log (one dictionary for the file).
    Audit,
    /// The replication log (one for the file).
    Replication,
}

impl StoredKind {
    pub const ALL: [StoredKind; 7] = [
        StoredKind::Node,
        StoredKind::Edge,
        StoredKind::HyperEdge,
        StoredKind::Text,
        StoredKind::Change,
        StoredKind::Audit,
        StoredKind::Replication,
    ];

    /// Kept for the whole file rather than per namespace.
    pub fn global(self) -> bool {
        matches!(self, StoredKind::Audit | StoredKind::Replication)
    }
}

/// How an envelope's payload is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    /// As it is.
    Plain = 0,
    // 1 is kept for LZ4, measured and not used: no gain at these sizes.
    /// A Zstandard frame.
    Zstd = 2,
    /// A Zstandard frame made with a dictionary; its id follows the length.
    ZstdDictionary = 3,
}

/// A stored dictionary, as listed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryInfo {
    /// Its id, in hexadecimal.
    pub id: String,
    /// The namespace (empty for the file's logs).
    pub namespace: String,
    pub kind: StoredKind,
    pub bytes: usize,
    /// Values it was trained on.
    pub samples: usize,
    /// Microseconds since the epoch.
    pub created_at: i64,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Header {
    pub(crate) namespace: String,
    pub(crate) kind: StoredKind,
    pub(crate) samples: usize,
    pub(crate) created_at: i64,
}

/// A dictionary ready to compress and decompress with.
pub(crate) struct Prepared {
    bytes: Vec<u8>,
    encoder: zstd::dict::EncoderDictionary<'static>,
    decoder: zstd::dict::DecoderDictionary<'static>,
}

/// Every dictionary loaded in this process, by id: decoding needs no
/// transaction, and the ids are hashes of the dictionaries, so files of the
/// same process share one map. Dictionaries stay loaded for the life of
/// the process (a few hundred KiB each, prepared), so that each thread can
/// keep its own handle on them without a lock (`SEEN`).
static LOADED: LazyLock<RwLock<HashMap<u64, &'static Prepared>>> = LazyLock::new(RwLock::default);

thread_local! {
    /// The dictionaries this thread has used, without the lock.
    static SEEN: RefCell<HashMap<u64, &'static Prepared>> = RefCell::new(HashMap::new());
    /// Zstandard's contexts, made once per thread: making one for every
    /// value allocates its buffers each time, which with many threads
    /// scanning made reads tens of times slower.
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
    static DICTIONARY_DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
    static COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
    static DICTIONARY_COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
}

fn register(id: u64, bytes: &[u8]) -> Result<&'static Prepared> {
    let mut loaded = LOADED.write().expect("dictionaries");
    if let Some(found) = loaded.get(&id) {
        if found.bytes != bytes {
            return Err(Error::Invalid(format!(
                "two different dictionaries have the id {id:016x}"
            )));
        }
        return Ok(found);
    }
    let prepared: &'static Prepared = Box::leak(Box::new(Prepared {
        bytes: bytes.to_vec(),
        encoder: zstd::dict::EncoderDictionary::copy(bytes, LEVEL),
        decoder: zstd::dict::DecoderDictionary::copy(bytes),
    }));
    loaded.insert(id, prepared);
    Ok(prepared)
}

fn loaded(id: u64) -> Option<&'static Prepared> {
    if let Some(found) = SEEN.with(|seen| seen.borrow().get(&id).copied()) {
        return Some(found);
    }
    let found = *LOADED.read().expect("dictionaries").get(&id)?;
    SEEN.with(|seen| seen.borrow_mut().insert(id, found));
    Some(found)
}

/// FNV-1a over the dictionary: the same dictionary, the same id.
fn id_of(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash.max(1)
}

/// What a database's writes compress with.
#[derive(Default)]
pub(crate) struct State {
    off: AtomicBool,
    /// The dictionary each namespace and kind writes with.
    current: RwLock<HashMap<(String, StoredKind), u64>>,
    /// Whether the committed layout is 20 (values may be in envelopes).
    pub(crate) layout_marked: AtomicBool,
}

impl State {
    pub(crate) fn set(&self, compression: Compression) {
        self.off
            .store(compression == Compression::Off, Ordering::Relaxed);
    }

    pub(crate) fn mode(&self) -> Compression {
        if self.off.load(Ordering::Relaxed) {
            Compression::Off
        } else {
            Compression::Auto
        }
    }

    fn dictionary(&self, ns: &str, kind: StoredKind) -> Option<(u64, &'static Prepared)> {
        let ns = if kind.global() { "" } else { ns };
        let id = *self
            .current
            .read()
            .expect("dictionaries")
            .get(&(ns.to_owned(), kind))?;
        loaded(id).map(|prepared| (id, prepared))
    }

    pub(crate) fn has_dictionary(&self, ns: &str, kind: StoredKind) -> bool {
        self.dictionary(ns, kind).is_some()
    }

    /// The file has no dictionaries and no envelopes any more.
    pub(crate) fn forget(&self) {
        self.current.write().expect("dictionaries").clear();
        self.layout_marked.store(false, Ordering::Relaxed);
    }

    /// Writes use `id` for its namespace and kind from now on.
    pub(crate) fn adopt(&self, ns: &str, kind: StoredKind, id: u64) {
        self.current
            .write()
            .expect("dictionaries")
            .insert((ns.to_owned(), kind), id);
    }

    /// Loads every dictionary of the file; each namespace and kind writes
    /// with its newest one.
    pub(crate) fn load(&self, tx: &impl TableSource) -> Result<()> {
        let mut current: HashMap<(String, StoredKind), (i64, u64)> = HashMap::new();
        for (id, header, bytes) in tx.dictionaries()? {
            register(id, &bytes)?;
            let slot = current
                .entry((header.namespace, header.kind))
                .or_insert((header.created_at, id));
            if header.created_at >= slot.0 {
                *slot = (header.created_at, id);
            }
        }
        *self.current.write().expect("dictionaries") = current
            .into_iter()
            .map(|(key, (_, id))| (key, id))
            .collect();
        Ok(())
    }

    /// `raw` (JSON, or a hyperedge's record) as it should be stored: in an
    /// envelope when compressing it pays, as it is otherwise (`always`:
    /// in an envelope even then, for tables where every value has one).
    /// Returns whether the result is an envelope.
    pub(crate) fn pack(
        &self,
        ns: &str,
        kind: StoredKind,
        raw: Vec<u8>,
        always: bool,
    ) -> (Vec<u8>, bool) {
        self.pack_as(ns, kind, raw, always, !self.off.load(Ordering::Relaxed))
    }

    /// [`State::pack`], compressing or not as `compress` says.
    pub(crate) fn pack_as(
        &self,
        ns: &str,
        kind: StoredKind,
        raw: Vec<u8>,
        always: bool,
        compress_it: bool,
    ) -> (Vec<u8>, bool) {
        if !compress_it {
            return if always {
                (envelope(&raw, Codec::Plain, None, &raw), true)
            } else {
                (raw, false)
            };
        }
        if raw.len() >= MIN_DICTIONARY_VALUE
            && let Some((id, prepared)) = self.dictionary(ns, kind)
            && let Some(packed) = compress(&raw, Some(prepared))
        {
            return (
                envelope(&raw, Codec::ZstdDictionary, Some(id), &packed),
                true,
            );
        }
        if raw.len() >= MIN_PLAIN_VALUE
            && let Some(packed) = compress(&raw, None)
        {
            return (envelope(&raw, Codec::Zstd, None, &packed), true);
        }
        if always {
            (envelope(&raw, Codec::Plain, None, &raw), true)
        } else {
            (raw, false)
        }
    }
}

/// The tables dictionaries are read from: a read or a write transaction.
pub(crate) trait TableSource {
    fn dictionaries(&self) -> Result<Vec<(u64, Header, Vec<u8>)>>;
}

fn parse(value: &[u8]) -> Result<(Header, &[u8])> {
    let broken = || Error::Serialization("a stored dictionary is broken".into());
    let length = u32::from_le_bytes(
        value
            .get(..4)
            .ok_or_else(broken)?
            .try_into()
            .map_err(|_| broken())?,
    ) as usize;
    let header = value.get(4..4 + length).ok_or_else(broken)?;
    let header: Header = serde_json::from_slice(header).map_err(|_| broken())?;
    Ok((header, &value[4 + length..]))
}

fn read_all(table: &impl ReadableTable<u64, &'static [u8]>) -> Result<Vec<(u64, Header, Vec<u8>)>> {
    let mut out = Vec::new();
    for item in table.iter().map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let (header, bytes) = parse(value.value())?;
        out.push((key.value(), header, bytes.to_vec()));
    }
    Ok(out)
}

impl TableSource for redb::ReadTransaction {
    fn dictionaries(&self) -> Result<Vec<(u64, Header, Vec<u8>)>> {
        match self.open_table(DICTIONARIES) {
            Ok(table) => read_all(&table),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(Vec::new()),
            Err(error) => Err(storage(error)),
        }
    }
}

impl TableSource for redb::WriteTransaction {
    fn dictionaries(&self) -> Result<Vec<(u64, Header, Vec<u8>)>> {
        // Opening it would create it: a file of an earlier layout has none.
        if !has_table(self, redb::TableHandle::name(&DICTIONARIES))? {
            return Ok(Vec::new());
        }
        read_all(&self.open_table(DICTIONARIES).map_err(storage)?)
    }
}

/// Whether `tx`'s file has the table `name` (opening one in a write
/// transaction creates it, which an earlier Grafeno's backup refuses).
pub(crate) fn has_table(tx: &redb::WriteTransaction, name: &str) -> Result<bool> {
    use redb::TableHandle;
    Ok(tx
        .list_tables()
        .map_err(storage)?
        .any(|table| table.name() == name))
}

/// Runs `f` with this thread's context in `slot`, made by `make` the
/// first time.
fn with_context<C, T>(
    slot: &'static std::thread::LocalKey<RefCell<Option<C>>>,
    make: impl FnOnce() -> std::io::Result<C>,
    f: impl FnOnce(&mut C) -> std::io::Result<T>,
) -> std::io::Result<T> {
    slot.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            *cell = Some(make()?);
        }
        f(cell.as_mut().expect("a context"))
    })
}

/// Compresses `raw`, and checks it reads back: `None` when it does not pay
/// or would not read back the same.
fn compress(raw: &[u8], dictionary: Option<&'static Prepared>) -> Option<Vec<u8>> {
    let packed = match dictionary {
        Some(prepared) => with_context(
            &DICTIONARY_COMPRESSOR,
            || zstd::bulk::Compressor::new(LEVEL),
            |c| {
                c.set_prepared_dictionary(&prepared.encoder)?;
                c.compress(raw)
            },
        )
        .ok()?,
        None => with_context(
            &COMPRESSOR,
            || zstd::bulk::Compressor::new(LEVEL),
            |c| c.compress(raw),
        )
        .ok()?,
    };
    // The envelope's header costs a few bytes more.
    if (packed.len() + 8) * 100 > raw.len() * MIN_SAVING {
        return None;
    }
    let back = decompress(&packed, raw.len(), dictionary).ok()?;
    (back == raw).then_some(packed)
}

fn decompress(
    packed: &[u8],
    length: usize,
    dictionary: Option<&'static Prepared>,
) -> Result<Vec<u8>> {
    let corrupt = |e: std::io::Error| {
        Error::Serialization(format!("a compressed value does not decompress: {e}"))
    };
    let out = match dictionary {
        Some(prepared) => with_context(
            &DICTIONARY_DECOMPRESSOR,
            zstd::bulk::Decompressor::new,
            |d| {
                d.set_prepared_dictionary(&prepared.decoder)?;
                d.decompress(packed, length)
            },
        )
        .map_err(corrupt)?,
        None => with_context(&DECOMPRESSOR, zstd::bulk::Decompressor::new, |d| {
            d.decompress(packed, length)
        })
        .map_err(corrupt)?,
    };
    if out.len() != length {
        return Err(Error::Serialization(
            "a compressed value does not hold the length its envelope claims".into(),
        ));
    }
    Ok(out)
}

/// An envelope: tag, version, codec, the decoded length (varint), the
/// dictionary id (varint, codec 3 only), then the payload.
pub(crate) fn envelope(
    raw: &[u8],
    codec: Codec,
    dictionary: Option<u64>,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 16);
    out.extend_from_slice(&[ENVELOPE, ENVELOPE_VERSION, codec as u8]);
    put_varint(&mut out, raw.len() as i64);
    if let Some(id) = dictionary {
        put_unsigned(&mut out, id);
    }
    out.extend_from_slice(payload);
    out
}

fn put_unsigned(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub(crate) fn take_unsigned(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The payload of an envelope with codec `codec`, decompressed.
pub(crate) fn unpack(codec: u8, length: usize, rest: &[u8]) -> Result<Vec<u8>> {
    match codec {
        2 => decompress(rest, length, None),
        3 => {
            let mut at = 0;
            let id = take_unsigned(rest, &mut at).ok_or_else(|| {
                Error::Serialization("a stored value's envelope is cut short".into())
            })?;
            let prepared = loaded(id).ok_or_else(|| {
                Error::Serialization(format!(
                    "a stored value names dictionary {id:016x}, which the file does not hold"
                ))
            })?;
            decompress(&rest[at..], length, Some(prepared))
        }
        other => Err(Error::Invalid(format!(
            "a stored value is compressed with codec {other}, which this Grafeno does not read: upgrade Grafeno"
        ))),
    }
}

/// A dictionary trained, not yet stored.
#[derive(Clone)]
pub(crate) struct Trained {
    pub(crate) id: u64,
    pub(crate) namespace: String,
    pub(crate) kind: StoredKind,
    pub(crate) samples: usize,
    pub(crate) bytes: Vec<u8>,
}

/// A dictionary for `samples` (decoded values of one namespace and kind),
/// or `None` when they are too few or too alike to train one.
pub(crate) fn train(ns: &str, kind: StoredKind, samples: &[Vec<u8>]) -> Option<Trained> {
    let total: usize = samples.iter().map(Vec::len).sum();
    if samples.len() < 64 || total < 16 * 1024 {
        return None;
    }
    // Zstandard wants about a hundred times the dictionary in samples.
    let size = DICTIONARY_BYTES.min(total / 16).max(4 * 1024);
    let bytes = zstd::dict::from_samples(samples, size).ok()?;
    Some(Trained {
        id: id_of(&bytes),
        namespace: if kind.global() {
            String::new()
        } else {
            ns.to_owned()
        },
        kind,
        samples: samples.len(),
        bytes,
    })
}

/// Stores `trained` in `tx` (not replicated, not logged: it is physical).
pub(crate) fn store(tx: &redb::WriteTransaction, trained: &Trained) -> Result<()> {
    register(trained.id, &trained.bytes)?;
    let header = serde_json::to_vec(&Header {
        namespace: trained.namespace.clone(),
        kind: trained.kind,
        samples: trained.samples,
        created_at: crate::Timestamp::now().as_micros(),
    })
    .map_err(|e| Error::Serialization(e.to_string()))?;
    let mut value = Vec::with_capacity(4 + header.len() + trained.bytes.len());
    value.extend_from_slice(&(header.len() as u32).to_le_bytes());
    value.extend_from_slice(&header);
    value.extend_from_slice(&trained.bytes);
    tx.open_table(DICTIONARIES)
        .map_err(storage)?
        .insert(trained.id, value.as_slice())
        .map_err(storage)?;
    Ok(())
}

/// The dictionaries stored in a database.
pub(crate) fn list(db: &redb::Database) -> Result<Vec<DictionaryInfo>> {
    let read = db.begin_read().map_err(storage)?;
    let mut out: Vec<DictionaryInfo> = read
        .dictionaries()?
        .into_iter()
        .map(|(id, header, bytes)| DictionaryInfo {
            id: format!("{id:016x}"),
            namespace: header.namespace,
            kind: header.kind,
            bytes: bytes.len(),
            samples: header.samples,
            created_at: header.created_at,
        })
        .collect();
    out.sort_by(|a, b| {
        (&a.namespace, a.kind, a.created_at).cmp(&(&b.namespace, b.kind, b.created_at))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                format!(
                    r#"{{"id":"0190a1b2-{i:04x}-7000-8000-00000000{i:04x}","labels":["Item"],"properties":{{"code":"C{i}","name":"item {i} of the catalogue","price":{}}}}}"#,
                    i * 7 % 1000
                )
                .into_bytes()
            })
            .collect()
    }

    #[test]
    fn values_compress_with_a_dictionary_and_read_back() {
        let samples = records(2_000);
        let trained = train("ns", StoredKind::Node, &samples).unwrap();
        register(trained.id, &trained.bytes).unwrap();
        let state = State::default();
        state.adopt("ns", StoredKind::Node, trained.id);
        let raw = records(2_001).pop().unwrap();
        let (packed, enveloped) = state.pack("ns", StoredKind::Node, raw.clone(), false);
        assert!(enveloped);
        assert!(
            packed.len() * 2 < raw.len(),
            "{} of {}",
            packed.len(),
            raw.len()
        );
        assert_eq!(
            &*crate::store::unwrap_envelope(&packed).unwrap(),
            raw.as_slice()
        );
        // Another namespace has no dictionary: too short to compress alone.
        let (plain, enveloped) = state.pack("other", StoredKind::Node, raw.clone(), false);
        assert!(!enveloped && plain == raw);
        // Off: as it is, or in a plain envelope where one is required.
        state.set(Compression::Off);
        assert_eq!(
            state.pack("ns", StoredKind::Node, raw.clone(), false).0,
            raw
        );
        let (wrapped, _) = state.pack("ns", StoredKind::Text, raw.clone(), true);
        assert_eq!(
            &*crate::store::unwrap_envelope(&wrapped).unwrap(),
            raw.as_slice()
        );
    }

    #[test]
    fn corrupt_or_unknown_compressed_values_are_errors() {
        let raw = vec![b'a'; 4_000];
        let packed = compress(&raw, None).unwrap();
        let good = envelope(&raw, Codec::Zstd, None, &packed);
        assert_eq!(
            &*crate::store::unwrap_envelope(&good).unwrap(),
            raw.as_slice()
        );
        // A wrong length.
        let mut wrong = envelope(&raw[..3_999], Codec::Zstd, None, &packed);
        assert!(crate::store::unwrap_envelope(&wrong).is_err());
        // Damaged bytes.
        let at = wrong.len() - 3;
        wrong[at] ^= 0xFF;
        assert!(crate::store::unwrap_envelope(&wrong).is_err());
        // A dictionary the file does not hold.
        let missing = envelope(&raw, Codec::ZstdDictionary, Some(42), &packed);
        assert!(
            crate::store::unwrap_envelope(&missing)
                .unwrap_err()
                .to_string()
                .contains("does not hold")
        );
    }
}

impl crate::WriteTx {
    /// `raw` as it is stored for `ns` and `kind`: compressed when it pays.
    pub(crate) fn pack(&self, ns: &str, kind: StoredKind, raw: Vec<u8>) -> Vec<u8> {
        let (out, enveloped) = self.shared.compression.pack(ns, kind, raw, false);
        if enveloped {
            self.wrote_envelope.set(true);
        }
        out
    }

    /// Whether the file may hold `text_values` and `dictionaries` (layout
    /// 20, or this transaction made it so).
    pub(crate) fn values_exist(&self) -> bool {
        self.wrote_envelope.get()
            || self
                .shared
                .compression
                .layout_marked
                .load(Ordering::Relaxed)
    }

    /// Notes that this transaction wrote a value in an envelope.
    pub(crate) fn wrote_envelope(&self) {
        self.wrote_envelope.set(true);
    }

    /// Before the commit of a transaction that wrote an envelope: marks the
    /// file layout 20, keeping the layout it had. Returns whether it did.
    pub(crate) fn mark_envelope_layout(&self) -> Result<bool> {
        if !self.wrote_envelope.get()
            || self
                .shared
                .compression
                .layout_marked
                .load(Ordering::Relaxed)
        {
            return Ok(false);
        }
        let mut meta = self.tx.open_table(crate::store::META).map_err(storage)?;
        let stored = meta
            .get(crate::db::SCHEMA_KEY)
            .map_err(storage)?
            .map_or(crate::BASE_LAYOUT, |guard| guard.value());
        if stored < crate::SCHEMA_VERSION {
            meta.insert(crate::db::LAYOUT_BEFORE_ENVELOPES, stored)
                .map_err(storage)?;
            meta.insert(crate::db::SCHEMA_KEY, crate::SCHEMA_VERSION)
                .map_err(storage)?;
        }
        Ok(true)
    }
}

/// What the text functions, which take redb's transaction, write texts
/// with.
pub(crate) struct Packer<'a> {
    state: &'a State,
    wrote: &'a std::cell::Cell<bool>,
}

impl Packer<'_> {
    /// Whether the file may hold texts in `text_values` (layout 20, or this
    /// transaction wrote an envelope): otherwise the table is not opened.
    pub(crate) fn values_exist(&self) -> bool {
        self.wrote.get() || self.state.layout_marked.load(Ordering::Relaxed)
    }

    /// `text` compressed, as `text_values` holds it, or `None` when it is
    /// not worth it or compression is off (it goes to `texts`, as before
    /// layout 20).
    pub(crate) fn text(&self, ns: &str, text: &str) -> Option<Vec<u8>> {
        let (out, enveloped) =
            self.state
                .pack(ns, StoredKind::Text, text.as_bytes().to_vec(), false);
        if !enveloped {
            return None;
        }
        self.wrote.set(true);
        Some(out)
    }
}

impl crate::WriteTx {
    pub(crate) fn packer(&self) -> Packer<'_> {
        Packer {
            state: &self.shared.compression,
            wrote: &self.wrote_envelope,
        }
    }
}

/// A text as `text_values` holds it, decoded.
pub(crate) fn text_of(bytes: &[u8]) -> Result<String> {
    if !crate::store::is_enveloped(bytes) {
        return Err(Error::Serialization("a stored text has no envelope".into()));
    }
    String::from_utf8(crate::store::unwrap_envelope(bytes)?.into_owned())
        .map_err(|_| Error::Serialization("a stored text is not UTF-8".into()))
}

/// What [`crate::Grafeno::train_dictionaries`] trains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainOptions {
    /// Only these namespaces (all when empty; the file's logs only then).
    #[serde(default)]
    pub namespaces: Vec<String>,
    /// A namespace and kind with fewer values gets no dictionary yet.
    #[serde(default = "default_min_values")]
    pub min_values: u64,
    /// Train a new dictionary even where there is one (the old ones stay,
    /// for the values they compressed).
    #[serde(default)]
    pub retrain: bool,
}

fn default_min_values() -> u64 {
    1_000
}

impl Default for TrainOptions {
    fn default() -> Self {
        Self {
            namespaces: Vec::new(),
            min_values: default_min_values(),
            retrain: false,
        }
    }
}

/// Values sampled for a dictionary at most.
const MAX_SAMPLES: u64 = 4_000;

/// The decoded values of one namespace and kind to train on, evenly
/// spread, and how many values there are.
fn samples_of(read: &crate::ReadTx, ns: &str, kind: StoredKind) -> Result<(u64, Vec<Vec<u8>>)> {
    use crate::store::unwrap_envelope;
    let end = format!("{ns}\0");
    let mut values: Vec<Vec<u8>> = Vec::new();
    let mut count = 0u64;
    // A pass over the keys counts them, a second reads every step-th value.
    macro_rules! pass {
        ($table:expr, $range:expr, $decode:expr) => {{
            let table = $table;
            let total = table.range($range).map_err(storage)?.count() as u64;
            count += total;
            let step = (total / MAX_SAMPLES).max(1);
            for (at, item) in table.range($range).map_err(storage)?.enumerate() {
                if at as u64 % step == 0 {
                    let (_, value) = item.map_err(storage)?;
                    values.push($decode(value.value())?);
                }
            }
        }};
    }
    let raw = |bytes: &[u8]| -> Result<Vec<u8>> { Ok(unwrap_envelope(bytes)?.into_owned()) };
    let text = |text: &str| -> Result<Vec<u8>> { Ok(text.as_bytes().to_vec()) };
    let records = (ns, 0u128)..(end.as_str(), 0u128);
    match kind {
        StoredKind::Node => pass!(&read.nodes, records.clone(), raw),
        StoredKind::Edge => pass!(&read.edges, records.clone(), raw),
        StoredKind::HyperEdge => {
            let table = read
                .tx
                .open_table(crate::hyperedge::HYPEREDGES)
                .map_err(storage)?;
            pass!(&table, records.clone(), raw)
        }
        StoredKind::Text => {
            if let Some(values) = &read.text_values {
                pass!(values, records.clone(), raw);
            }
            pass!(&read.texts, records.clone(), text);
        }
        StoredKind::Change => {
            let table = read
                .tx
                .open_table(crate::changes::CHANGES)
                .map_err(storage)?;
            pass!(&table, (ns, 0u64)..=(ns, u64::MAX), raw)
        }
        StoredKind::Audit => {
            let table = read.tx.open_table(crate::audit::AUDIT).map_err(storage)?;
            pass!(&table, 0u64.., raw)
        }
        StoredKind::Replication => {
            let table = read
                .tx
                .open_table(crate::replication::REPLICATION_LOG)
                .map_err(storage)?;
            pass!(&table, 0u64.., raw)
        }
    }
    Ok((count, values))
}

impl crate::Grafeno {
    /// The dictionaries stored in this database.
    pub fn dictionaries(&self) -> Result<Vec<DictionaryInfo>> {
        list(self.raw())
    }

    /// Trains the dictionaries `options` asks for on a snapshot, without
    /// holding the writer: nothing is stored yet.
    pub(crate) fn dictionaries_to_train(&self, options: &TrainOptions) -> Result<Vec<Trained>> {
        let read = self.begin_read()?;
        let mut namespaces: Vec<String> = if options.namespaces.is_empty() {
            read.namespaces()?
                .into_iter()
                .map(|info| info.name)
                .collect()
        } else {
            options.namespaces.clone()
        };
        namespaces.sort();
        let mut targets: Vec<(String, StoredKind)> = Vec::new();
        for ns in &namespaces {
            for kind in StoredKind::ALL.into_iter().filter(|k| !k.global()) {
                targets.push((ns.clone(), kind));
            }
        }
        if options.namespaces.is_empty() {
            targets.push((String::new(), StoredKind::Audit));
            targets.push((String::new(), StoredKind::Replication));
        }
        self.train_targets(&read, targets, options.min_values, options.retrain)
    }

    /// The dictionaries a migration's train step trains for `ns` (the
    /// file's logs for "").
    pub(crate) fn train_for(&self, ns: &str, retrain: bool) -> Result<Vec<Trained>> {
        let read = self.begin_read()?;
        let targets = StoredKind::ALL
            .into_iter()
            .filter(|kind| kind.global() == ns.is_empty())
            .map(|kind| (ns.to_owned(), kind))
            .collect();
        self.train_targets(&read, targets, default_min_values(), retrain)
    }

    fn train_targets(
        &self,
        read: &crate::ReadTx,
        targets: Vec<(String, StoredKind)>,
        min_values: u64,
        retrain: bool,
    ) -> Result<Vec<Trained>> {
        let mut out = Vec::new();
        for (ns, kind) in targets {
            if !retrain && self.shared.compression.has_dictionary(&ns, kind) {
                continue;
            }
            let (count, samples) = samples_of(read, &ns, kind)?;
            if count < min_values.max(64) {
                continue;
            }
            if let Some(trained) = train(&ns, kind, &samples) {
                out.push(trained);
            }
        }
        Ok(out)
    }

    /// Writes use `trained` from now on (once stored and committed).
    pub(crate) fn adopt_dictionaries(&self, trained: &[Trained]) {
        for t in trained {
            self.shared.compression.adopt(&t.namespace, t.kind, t.id);
        }
    }

    fn trained_infos(&self, trained: &[Trained]) -> Result<Vec<DictionaryInfo>> {
        let ids: Vec<String> = trained.iter().map(|t| format!("{:016x}", t.id)).collect();
        Ok(self
            .dictionaries()?
            .into_iter()
            .filter(|d| ids.contains(&d.id))
            .collect())
    }

    /// Trains a dictionary for each namespace and kind of value with
    /// enough values and none yet (see [`TrainOptions`]), stores them, and
    /// has later writes use them. Values already written keep their
    /// encoding: a migration with [`crate::Encoding::Compressed`] rewrites
    /// them.
    pub fn train_dictionaries(&self, options: &TrainOptions) -> Result<Vec<DictionaryInfo>> {
        let trained = self.dictionaries_to_train(options)?;
        if trained.is_empty() {
            return Ok(Vec::new());
        }
        let saving = trained.clone();
        self.write(move |tx| tx.save_dictionaries(&saving))?;
        self.adopt_dictionaries(&trained);
        self.trained_infos(&trained)
    }
}

impl crate::WriteTx {
    /// Stores trained dictionaries (physical: not replicated, not logged).
    /// The file becomes layout 20: an earlier Grafeno does not know the
    /// table.
    pub(crate) fn save_dictionaries(&self, trained: &[Trained]) -> Result<()> {
        if !trained.is_empty() {
            self.wrote_envelope.set(true);
        }
        for t in trained {
            store(&self.tx, t)?;
        }
        Ok(())
    }
}

impl crate::Writer {
    /// [`crate::Grafeno::train_dictionaries`] with the store through this
    /// writer's bulk lane: the training itself holds no lock.
    pub fn train_dictionaries(&self, options: &TrainOptions) -> Result<Vec<DictionaryInfo>> {
        let db = self.database();
        let trained = db.dictionaries_to_train(options)?;
        if trained.is_empty() {
            return Ok(Vec::new());
        }
        let saving = trained.clone();
        self.write_bulk(move |tx| tx.save_dictionaries(&saving))?;
        self.flush()?;
        db.adopt_dictionaries(&trained);
        db.trained_infos(&trained)
    }
}
