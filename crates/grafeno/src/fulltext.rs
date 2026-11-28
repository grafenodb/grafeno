//! Full-text indexes (16.6): chosen string properties of the nodes with a
//! label, each with a weight, analyzed per index (accents, stop words and
//! stemming for English and Spanish), searched with a small query language
//! (phrases, required and excluded terms, `AND`, `OR`, prefixes, terms near
//! each other, fuzzy terms, one field), ranked with BM25 per field and
//! returned with the fragments that matched. [`ReadTx::search`] fuses it,
//! or the node text, with vector search as the caller says.
//!
//! A full-text index is an [`IndexDef`] with `fulltext` set, so it is
//! declared, replicated, exported, rebuilt and dropped as any index. Its
//! postings are stored in one of two formats, chosen when it is built and
//! kept with its statistics (a member's own, never exported or
//! replicated):
//!
//! - **postings** (phase 9 of the storage roadmap, the default): one entry
//!   per term and node in `fulltext_postings`, keyed by namespace, index,
//!   term and node; the value holds, per field with the term, the field's
//!   length, the term's frequency and its first positions, as varints
//!   (positions as gaps). The table exists from the first such index on,
//!   which makes the file layout 21.
//! - **entries** (until phase 9): one entry per term, field and node in the
//!   index table, the key holding all of it, fixed-width.
//!
//! Both score alike (BM25 per field, the same sums in the same order); a
//! rebuild writes an index in the database's format
//! ([`crate::OpenOptions::fulltext`]). The documents and terms per field,
//! which BM25 needs, are kept in `fulltext_stats`, derived on every member
//! as the postings are.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use grafeno_core::{Error, NodeId, Props, Result};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::ReadTx;
use crate::index::{IndexDef, IndexKey};
use crate::store::storage;

/// `(ns, index) -> FieldStats` (JSON): documents and terms per field.
pub(crate) const FULLTEXT_STATS: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("fulltext_stats");

/// `(ns, index, term, node) -> postings of the term in the node's fields`
/// (the postings format).
pub(crate) const FULLTEXT_POSTINGS: TableDefinition<PostingsKey, &[u8]> =
    TableDefinition::new("fulltext_postings");

pub(crate) type PostingsKey = (&'static str, &'static str, &'static str, u128);

/// How full-text indexes keep their postings (see the module's notes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FulltextFormat {
    /// One entry per term and node, the fields in a varint value.
    #[default]
    Postings,
    /// One key per term, field and node (before phase 9).
    Entries,
}

impl FulltextFormat {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "postings" => Some(FulltextFormat::Postings),
            "entries" => Some(FulltextFormat::Entries),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            FulltextFormat::Postings => "postings",
            FulltextFormat::Entries => "entries",
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u32) {
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

/// One field of a postings value: field, its length, the term's frequency
/// and its first positions (gaps, as varints).
struct FieldPosting<'a> {
    field: usize,
    length: u32,
    frequency: u32,
    positions: &'a [u8],
}

impl FieldPosting<'_> {
    fn positions(&self) -> Vec<u32> {
        let mut bytes = self.positions;
        let mut at = 0u32;
        let mut out = Vec::new();
        while !bytes.is_empty() {
            let Some(gap) = take_varint(&mut bytes) else {
                break;
            };
            at = at.wrapping_add(gap);
            out.push(at);
        }
        out
    }
}

/// Calls `each` with every field of a postings value, in field order,
/// without allocating; stops at a damaged value.
fn each_field<'a>(value: &'a [u8], mut each: impl FnMut(FieldPosting<'a>)) {
    let mut rest = value;
    while let Some((&field, after)) = rest.split_first() {
        rest = after;
        let (Some(length), Some(frequency), Some(bytes)) = (
            take_varint(&mut rest),
            take_varint(&mut rest),
            take_varint(&mut rest),
        ) else {
            return;
        };
        let Some((positions, after)) = rest.split_at_checked(bytes as usize) else {
            return;
        };
        rest = after;
        each(FieldPosting {
            field: field as usize,
            length,
            frequency,
            positions,
        });
    }
}

/// The tag of full-text entries in the index table.
const TERM: u8 = 5;
/// Positions kept per term and field: phrases and nearness look no
/// further into a long field.
const MAX_POSITIONS: usize = 64;
/// Terms longer than this are left out.
const MAX_TERM_CHARS: usize = 64;
/// Values of a list are this many positions apart, so a phrase does not
/// run from one into the next.
const ITEM_GAP: u32 = 100;
/// Terms a prefix or a fuzzy term may stand for.
const MAX_EXPANSIONS: usize = 64;
const K1: f32 = 1.2;
const B: f32 = 0.75;

/// How a full-text index turns text into terms. Every analyzer lowercases
/// and removes accents (`Canción` is `cancion`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Analyzer {
    /// Nothing more: every word is a term.
    Simple,
    /// Leaves out common English and Spanish words.
    #[default]
    Standard,
    /// Leaves out common English words and reduces words to their stem
    /// (`graphs`, `graphing` are `graph`).
    English,
    /// Leaves out common Spanish words and reduces words to their stem
    /// (`grafos`, `grafo` are `graf`).
    Spanish,
}

impl Analyzer {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "simple" => Analyzer::Simple,
            "standard" => Analyzer::Standard,
            "english" | "en" => Analyzer::English,
            "spanish" | "es" => Analyzer::Spanish,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Analyzer::Simple => "simple",
            Analyzer::Standard => "standard",
            Analyzer::English => "english",
            Analyzer::Spanish => "spanish",
        }
    }
}

/// What makes an index a full-text one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Fulltext {
    #[serde(default)]
    pub analyzer: Analyzer,
    /// A weight per property, in the order of the index's properties (1
    /// for each when empty or short).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weights: Vec<f32>,
}

impl Fulltext {
    fn weight(&self, field: usize) -> f32 {
        self.weights.get(field).copied().unwrap_or(1.0)
    }
}

const ENGLISH: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has", "have", "he",
    "her", "his", "if", "in", "into", "is", "it", "its", "of", "on", "or", "our", "she", "so",
    "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "to", "was",
    "we", "were", "what", "when", "where", "which", "who", "will", "with", "you", "your",
];

const SPANISH: &[&str] = &[
    "a", "al", "algo", "como", "con", "de", "del", "el", "ella", "ellos", "en", "entre", "era",
    "es", "esa", "ese", "eso", "esta", "este", "esto", "fue", "ha", "han", "hay", "la", "las",
    "le", "les", "lo", "los", "mas", "me", "mi", "muy", "no", "nos", "o", "para", "pero", "por",
    "que", "se", "si", "sin", "sobre", "son", "su", "sus", "te", "tu", "un", "una", "uno", "unos",
    "unas", "y", "ya", "yo",
];

/// A term and where its word is in the text, in characters.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Token {
    pub term: String,
    pub start: usize,
    pub end: usize,
}

/// Words of `text`, lowercased and without accents, with where each is.
fn words(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    for (at, c) in text.chars().enumerate() {
        let mut letters = c.nfd().filter(|d| !is_combining_mark(*d)).peekable();
        if letters.peek().is_some_and(|d| d.is_alphanumeric()) {
            if current.is_empty() {
                start = at;
            }
            for d in letters.filter(|d| d.is_alphanumeric()) {
                current.extend(d.to_lowercase());
            }
        } else if !current.is_empty() {
            out.push(Token {
                term: std::mem::take(&mut current),
                start,
                end: at,
            });
        }
    }
    if !current.is_empty() {
        out.push(Token {
            term: current,
            start,
            end: text.chars().count(),
        });
    }
    out
}

/// The terms of `text` under `analyzer`, with where each word is.
pub(crate) fn analyze(analyzer: Analyzer, text: &str) -> Vec<Token> {
    words(text)
        .into_iter()
        .filter(|token| token.term.chars().count() <= MAX_TERM_CHARS)
        .filter(|token| !stopword(analyzer, &token.term))
        .map(|token| Token {
            term: stem(analyzer, &token.term),
            ..token
        })
        .collect()
}

fn stopword(analyzer: Analyzer, word: &str) -> bool {
    match analyzer {
        Analyzer::Simple => false,
        Analyzer::Standard => {
            word.chars().count() < 2 || ENGLISH.contains(&word) || SPANISH.contains(&word)
        }
        Analyzer::English => ENGLISH.contains(&word),
        Analyzer::Spanish => SPANISH.contains(&word),
    }
}

/// A word reduced to its stem in `language` (a node text's terms), by the
/// Snowball stemmers: verbs too ("decidimos", "decidir" → "decid"). The
/// word comes without accents, as [`crate::text::tokenize`] leaves it, in
/// the index and in the question alike.
pub(crate) fn stem_in(language: crate::text::Language, word: &str) -> String {
    use std::sync::OnceLock;
    static ENGLISH: OnceLock<rust_stemmers::Stemmer> = OnceLock::new();
    static SPANISH: OnceLock<rust_stemmers::Stemmer> = OnceLock::new();
    let stemmer = match language {
        crate::text::Language::English => ENGLISH
            .get_or_init(|| rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::English)),
        crate::text::Language::Spanish => SPANISH
            .get_or_init(|| rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::Spanish)),
    };
    stemmer.stem(word).into_owned()
}

/// A word as the analyzer keeps it.
fn stem(analyzer: Analyzer, word: &str) -> String {
    match analyzer {
        Analyzer::English => stem_english(word),
        Analyzer::Spanish => stem_spanish(word),
        _ => word.to_owned(),
    }
}

fn is_vowel(c: u8) -> bool {
    matches!(c, b'a' | b'e' | b'i' | b'o' | b'u')
}

/// A light English stemmer: plurals, `-ing`, `-ed` and `-ly`.
fn stem_english(word: &str) -> String {
    if !word.is_ascii() || word.len() <= 3 {
        return word.to_owned();
    }
    let mut w = word.to_owned();
    if w.ends_with("ies") && w.len() > 4 {
        w.truncate(w.len() - 3);
        w.push('y');
    } else if w.ends_with("sses") {
        w.truncate(w.len() - 2);
    } else if w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") && !w.ends_with("is") {
        w.truncate(w.len() - 1);
    }
    for suffix in ["ing", "ed"] {
        if w.ends_with(suffix) && w.len() - suffix.len() >= 3 {
            let stem = &w[..w.len() - suffix.len()];
            // A stem needs a vowel (`sing`, `bed` stay as they are).
            if !stem.bytes().any(is_vowel) {
                break;
            }
            let bytes = stem.as_bytes();
            let n = bytes.len();
            let doubled = n >= 2
                && bytes[n - 1] == bytes[n - 2]
                && !is_vowel(bytes[n - 1])
                && !matches!(bytes[n - 1], b'l' | b's' | b'z');
            w.truncate(if doubled { n - 1 } else { n });
            break;
        }
    }
    if w.ends_with("ly") && w.len() >= 6 {
        w.truncate(w.len() - 2);
    }
    w
}

/// A light Spanish stemmer: plurals, `-mente`, and a final vowel.
fn stem_spanish(word: &str) -> String {
    if !word.is_ascii() || word.len() <= 4 {
        return word.to_owned();
    }
    let mut w = word.to_owned();
    if w.ends_with("mente") && w.len() > 8 {
        w.truncate(w.len() - 5);
    }
    // `canciones` loses its `es` here, `casas` its `s` below.
    if w.ends_with("es") && w.len() > 5 && !is_vowel(w.as_bytes()[w.len() - 3]) {
        w.truncate(w.len() - 2);
    } else if w.ends_with('s') {
        w.truncate(w.len() - 1);
    }
    if w.len() > 4 && matches!(w.as_bytes()[w.len() - 1], b'a' | b'o' | b'e') {
        w.truncate(w.len() - 1);
    }
    w
}

/// The text of a property: a string, or the strings of a list.
fn texts_of(value: &Json) -> Vec<&str> {
    match value {
        Json::String(s) => vec![s.as_str()],
        Json::Array(items) => items.iter().filter_map(Json::as_str).collect(),
        _ => Vec::new(),
    }
}

/// A node as a full-text index holds it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Doc {
    /// Terms per field.
    pub lengths: Vec<u32>,
    /// Its entries (the entries format).
    pub entries: Vec<Vec<u8>>,
    /// Its postings by term, sorted (the postings format).
    pub postings: Vec<(String, Vec<u8>)>,
}

/// What `def` holds of a node with these properties (`None`: no terms), in
/// the index's format.
pub(crate) fn doc_of(def: &IndexDef, fulltext: &Fulltext, props: &Props) -> Option<Doc> {
    let keys = def.keys();
    let mut lengths = vec![0u32; keys.len()];
    let mut positions: HashMap<(String, usize), Vec<u32>> = HashMap::new();
    for (field, key) in keys.iter().enumerate() {
        let Some(value) = props.get(*key) else {
            continue;
        };
        let mut at = 0u32;
        for (item, text) in texts_of(value).into_iter().enumerate() {
            if item > 0 {
                at += ITEM_GAP;
            }
            for token in analyze(fulltext.analyzer, text) {
                positions.entry((token.term, field)).or_default().push(at);
                at += 1;
                lengths[field] += 1;
            }
        }
    }
    if lengths.iter().all(|length| *length == 0) {
        return None;
    }
    if def.postings() {
        let mut by_term: std::collections::BTreeMap<String, Vec<(usize, Vec<u32>)>> =
            std::collections::BTreeMap::new();
        for ((term, field), at) in positions {
            by_term.entry(term).or_default().push((field, at));
        }
        let postings = by_term
            .into_iter()
            .map(|(term, mut fields)| {
                fields.sort_by_key(|(field, _)| *field);
                let mut value = Vec::new();
                for (field, at) in fields {
                    value.push(field as u8);
                    put_varint(&mut value, lengths[field]);
                    put_varint(&mut value, at.len() as u32);
                    // The positions' bytes first, so that scoring skips them.
                    let mut gaps = Vec::new();
                    let mut last = 0u32;
                    for p in at.iter().take(MAX_POSITIONS) {
                        put_varint(&mut gaps, p - last);
                        last = *p;
                    }
                    put_varint(&mut value, gaps.len() as u32);
                    value.extend_from_slice(&gaps);
                }
                (term, value)
            })
            .collect();
        return Some(Doc {
            lengths,
            entries: Vec::new(),
            postings,
        });
    }
    let mut entries: Vec<Vec<u8>> = positions
        .into_iter()
        .map(|((term, field), at)| {
            let mut bytes = vec![TERM];
            bytes.extend_from_slice(term.as_bytes());
            bytes.push(0);
            bytes.push(field as u8);
            bytes.extend_from_slice(&lengths[field].to_be_bytes());
            bytes.extend_from_slice(&(at.len() as u32).to_be_bytes());
            for p in at.iter().take(MAX_POSITIONS) {
                bytes.extend_from_slice(&p.to_be_bytes());
            }
            bytes
        })
        .collect();
    entries.sort();
    Some(Doc {
        lengths,
        entries,
        postings: Vec::new(),
    })
}

/// One entry, read back.
struct Posting<'a> {
    term: &'a str,
    field: usize,
    length: u32,
    frequency: u32,
    positions: Vec<u32>,
}

fn parse(bytes: &[u8]) -> Option<Posting<'_>> {
    let rest = bytes.strip_prefix(&[TERM])?;
    let end = rest.iter().position(|b| *b == 0)?;
    let term = std::str::from_utf8(&rest[..end]).ok()?;
    let rest = &rest[end + 1..];
    let field = *rest.first()? as usize;
    let number = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(rest.get(at..at + 4)?.try_into().ok()?))
    };
    let length = number(1)?;
    let frequency = number(5)?;
    let positions = rest[9..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_be_bytes(*chunk))
        .collect();
    Some(Posting {
        term,
        field,
        length,
        frequency,
        positions,
    })
}

/// An entry's field, the field's length and the term's frequency there,
/// read without its positions (all BM25 needs).
fn parse_counts(bytes: &[u8]) -> Option<(usize, u32, u32)> {
    let rest = bytes.strip_prefix(&[TERM])?;
    let end = rest.iter().position(|b| *b == 0)?;
    let rest = &rest[end + 1..];
    let field = *rest.first()? as usize;
    let number = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(rest.get(at..at + 4)?.try_into().ok()?))
    };
    Some((field, number(1)?, number(5)?))
}

/// Documents and terms per field of one index, and the format of its
/// postings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct FieldStats {
    pub documents: u64,
    pub terms: Vec<u64>,
    /// In `fulltext_postings` (the postings format), not the index table.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub postings: bool,
}

/// Whether index `name` keeps the postings format.
pub(crate) fn is_postings(
    table: &impl ReadableTable<(&'static str, &'static str), &'static [u8]>,
    ns: &str,
    name: &str,
) -> Result<bool> {
    Ok(read_stats(table, ns, name)?.postings)
}

fn read_stats(
    table: &impl ReadableTable<(&'static str, &'static str), &'static [u8]>,
    ns: &str,
    name: &str,
) -> Result<FieldStats> {
    match table.get((ns, name)).map_err(storage)? {
        Some(bytes) => {
            serde_json::from_slice(bytes.value()).map_err(|e| Error::Serialization(e.to_string()))
        }
        None => Ok(FieldStats::default()),
    }
}

/// Moves the statistics of index `name` from what `old` held to `new`.
pub(crate) fn update_stats(
    tx: &redb::WriteTransaction,
    ns: &str,
    name: &str,
    old: Option<&Doc>,
    new: Option<&Doc>,
) -> Result<()> {
    if old.map(|doc| &doc.lengths) == new.map(|doc| &doc.lengths) {
        return Ok(());
    }
    let mut table = tx.open_table(FULLTEXT_STATS).map_err(storage)?;
    let mut stats = read_stats(&table, ns, name)?;
    let mut apply = |doc: &Doc, sign: i64| {
        stats.documents = stats.documents.saturating_add_signed(sign);
        if stats.terms.len() < doc.lengths.len() {
            stats.terms.resize(doc.lengths.len(), 0);
        }
        for (total, length) in stats.terms.iter_mut().zip(&doc.lengths) {
            *total = total.saturating_add_signed(sign * i64::from(*length));
        }
    };
    if let Some(doc) = old {
        apply(doc, -1);
    }
    if let Some(doc) = new {
        apply(doc, 1);
    }
    let bytes = serde_json::to_vec(&stats).map_err(|e| Error::Serialization(e.to_string()))?;
    table
        .insert((ns, name), bytes.as_slice())
        .map_err(storage)?;
    Ok(())
}

/// The postings of index `name` that differ between `old` and `new` (both
/// in the postings format): removed, then written.
pub(crate) fn update_postings(
    tx: &redb::WriteTransaction,
    ns: &str,
    name: &str,
    id: u128,
    old: Option<&Doc>,
    new: Option<&Doc>,
) -> Result<()> {
    let empty = Vec::new();
    let before = old.map_or(&empty, |doc| &doc.postings);
    let after = new.map_or(&empty, |doc| &doc.postings);
    if before == after {
        return Ok(());
    }
    let mut table = tx.open_table(FULLTEXT_POSTINGS).map_err(storage)?;
    let wanted: HashMap<&str, &[u8]> = after
        .iter()
        .map(|(term, value)| (term.as_str(), value.as_slice()))
        .collect();
    for (term, _) in before {
        if !wanted.contains_key(term.as_str()) {
            table
                .remove((ns, name, term.as_str(), id))
                .map_err(storage)?;
        }
    }
    let had: HashMap<&str, &[u8]> = before
        .iter()
        .map(|(term, value)| (term.as_str(), value.as_slice()))
        .collect();
    for (term, value) in after {
        if had.get(term.as_str()) != Some(&value.as_slice()) {
            table
                .insert((ns, name, term.as_str(), id), value.as_slice())
                .map_err(storage)?;
        }
    }
    Ok(())
}

/// Removes every posting of index `name` (dropped, or rebuilt).
pub(crate) fn drop_postings(tx: &redb::WriteTransaction, ns: &str, name: &str) -> Result<()> {
    if !crate::compress::has_table(tx, redb::TableHandle::name(&FULLTEXT_POSTINGS))? {
        return Ok(());
    }
    tx.open_table(FULLTEXT_POSTINGS)
        .map_err(storage)?
        .retain_in((ns, name, "", 0u128).., |key, _| {
            key.0 != ns || key.1 != name
        })
        .map_err(storage)?;
    Ok(())
}

/// Removes the statistics of index `name` (dropped).
pub(crate) fn drop_stats(tx: &redb::WriteTransaction, ns: &str, name: &str) -> Result<()> {
    tx.open_table(FULLTEXT_STATS)
        .map_err(storage)?
        .remove((ns, name))
        .map_err(storage)?;
    Ok(())
}

/// Writes the statistics of a new index, and its format.
pub(crate) fn set_stats(
    tx: &redb::WriteTransaction,
    ns: &str,
    name: &str,
    docs: &[Doc],
    postings: bool,
) -> Result<()> {
    let mut stats = FieldStats {
        postings,
        ..FieldStats::default()
    };
    for doc in docs {
        stats.documents += 1;
        stats
            .terms
            .resize(stats.terms.len().max(doc.lengths.len()), 0);
        for (total, length) in stats.terms.iter_mut().zip(&doc.lengths) {
            *total += u64::from(*length);
        }
    }
    let bytes = serde_json::to_vec(&stats).map_err(|e| Error::Serialization(e.to_string()))?;
    tx.open_table(FULLTEXT_STATS)
        .map_err(storage)?
        .insert((ns, name), bytes.as_slice())
        .map_err(storage)?;
    Ok(())
}

// The query language.

#[derive(Clone, Debug, PartialEq)]
enum How {
    Exact,
    Prefix,
    Fuzzy(usize),
}

#[derive(Clone, Debug, PartialEq)]
enum Query {
    Term {
        field: Option<usize>,
        term: String,
        how: How,
    },
    /// Terms in order, next to each other; with a distance, all of them
    /// within it, in any order.
    Phrase {
        field: Option<usize>,
        terms: Vec<String>,
        near: Option<u32>,
    },
    All {
        must: Vec<Query>,
        should: Vec<Query>,
        not: Vec<Query>,
    },
    /// A word the analyzer leaves out.
    Nothing,
}

#[derive(Clone, Debug, PartialEq)]
enum Lexeme {
    Open,
    Close,
    Plus,
    Minus,
    And,
    Or,
    Not,
    Quoted(String, Option<u32>),
    Word(String),
}

fn lex(query: &str) -> Result<Vec<Lexeme>> {
    let chars: Vec<char> = query.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    let distance = |at: &mut usize| -> Option<u32> {
        if chars.get(*at) != Some(&'~') {
            return None;
        }
        let start = *at + 1;
        let mut end = start;
        while chars.get(end).is_some_and(char::is_ascii_digit) {
            end += 1;
        }
        *at = end;
        let digits: String = chars[start..end].iter().collect();
        Some(digits.parse().unwrap_or(2))
    };
    while at < chars.len() {
        let c = chars[at];
        match c {
            _ if c.is_whitespace() => at += 1,
            '(' => {
                out.push(Lexeme::Open);
                at += 1;
            }
            ')' => {
                out.push(Lexeme::Close);
                at += 1;
            }
            '+' | '-' if chars.get(at + 1).is_some_and(|n| !n.is_whitespace()) => {
                out.push(if c == '+' {
                    Lexeme::Plus
                } else {
                    Lexeme::Minus
                });
                at += 1;
            }
            '"' => {
                let start = at + 1;
                let Some(close) = chars[start..].iter().position(|c| *c == '"') else {
                    return Err(Error::Invalid(
                        "a phrase is not closed: a \" is missing".into(),
                    ));
                };
                let phrase: String = chars[start..start + close].iter().collect();
                at = start + close + 1;
                let near = distance(&mut at);
                out.push(Lexeme::Quoted(phrase, near));
            }
            _ => {
                let start = at;
                while at < chars.len()
                    && !chars[at].is_whitespace()
                    && !matches!(chars[at], '(' | ')' | '"')
                {
                    at += 1;
                }
                let word: String = chars[start..at].iter().collect();
                out.push(match word.as_str() {
                    "AND" | "&&" => Lexeme::And,
                    "OR" | "||" => Lexeme::Or,
                    "NOT" => Lexeme::Not,
                    _ => Lexeme::Word(word),
                });
            }
        }
    }
    Ok(out)
}

struct Parser<'a> {
    lexemes: Vec<Lexeme>,
    at: usize,
    fields: Vec<&'a str>,
    analyzer: Analyzer,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Lexeme> {
        self.lexemes.get(self.at)
    }

    fn eat(&mut self, lexeme: &Lexeme) -> bool {
        if self.peek() == Some(lexeme) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn any(&mut self) -> Result<Query> {
        let mut parts = vec![self.all()?];
        while self.eat(&Lexeme::Or) {
            parts.push(self.all()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Query::All {
                must: Vec::new(),
                should: parts,
                not: Vec::new(),
            }
        })
    }

    fn all(&mut self) -> Result<Query> {
        let mut parts = vec![self.clauses()?];
        while self.eat(&Lexeme::And) {
            parts.push(self.clauses()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Query::All {
                must: parts,
                should: Vec::new(),
                not: Vec::new(),
            }
        })
    }

    /// Clauses side by side: plain ones may match, `+` ones must, `-` and
    /// `NOT` ones must not.
    fn clauses(&mut self) -> Result<Query> {
        let (mut must, mut should, mut not) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            match self.peek() {
                None | Some(Lexeme::Close | Lexeme::And | Lexeme::Or) => break,
                _ => {}
            }
            if self.eat(&Lexeme::Plus) {
                must.push(self.primary()?);
            } else if self.eat(&Lexeme::Minus) || self.eat(&Lexeme::Not) {
                not.push(self.primary()?);
            } else {
                should.push(self.primary()?);
            }
        }
        if must.is_empty() && not.is_empty() && should.len() == 1 {
            return Ok(should.remove(0));
        }
        if must.is_empty() && should.is_empty() && !not.is_empty() {
            return Err(Error::Invalid(
                "a full-text query needs a term to find, not only terms to leave out".into(),
            ));
        }
        Ok(Query::All { must, should, not })
    }

    fn primary(&mut self) -> Result<Query> {
        match self.lexemes.get(self.at).cloned() {
            Some(Lexeme::Open) => {
                self.at += 1;
                let inner = self.any()?;
                if !self.eat(&Lexeme::Close) {
                    return Err(Error::Invalid("a `(` is not closed".into()));
                }
                Ok(inner)
            }
            Some(Lexeme::Quoted(text, near)) => {
                self.at += 1;
                Ok(self.phrase(None, &text, near))
            }
            Some(Lexeme::Word(word)) => {
                self.at += 1;
                // `title:word` or `title:"a phrase"`.
                if let Some((field, rest)) = word.split_once(':')
                    && let Some(index) = self.fields.iter().position(|f| *f == field)
                {
                    if rest.is_empty() {
                        return match self.lexemes.get(self.at).cloned() {
                            Some(Lexeme::Quoted(text, near)) => {
                                self.at += 1;
                                Ok(self.phrase(Some(index), &text, near))
                            }
                            _ => Err(Error::Invalid(format!(
                                "`{field}:` needs a word or a phrase after it"
                            ))),
                        };
                    }
                    return Ok(self.word(Some(index), rest));
                }
                Ok(self.word(None, &word))
            }
            Some(other) => Err(Error::Invalid(format!(
                "unexpected {other:?} in the full-text query"
            ))),
            None => Err(Error::Invalid(
                "the full-text query ends where a term was expected".into(),
            )),
        }
    }

    fn phrase(&self, field: Option<usize>, text: &str, near: Option<u32>) -> Query {
        let terms: Vec<String> = analyze(self.analyzer, text)
            .into_iter()
            .map(|token| token.term)
            .collect();
        match terms.len() {
            0 => Query::Nothing,
            1 if near.is_none() => Query::Term {
                field,
                term: terms[0].clone(),
                how: How::Exact,
            },
            _ => Query::Phrase { field, terms, near },
        }
    }

    fn word(&self, field: Option<usize>, word: &str) -> Query {
        if let Some(prefix) = word.strip_suffix('*') {
            let term: String = words(prefix).into_iter().map(|token| token.term).collect();
            return if term.is_empty() {
                Query::Nothing
            } else {
                Query::Term {
                    field,
                    term,
                    how: How::Prefix,
                }
            };
        }
        if let Some((word, distance)) = word.rsplit_once('~')
            && distance.chars().all(|c| c.is_ascii_digit())
        {
            let distance = distance.parse().unwrap_or(2).min(2);
            let term: String = words(word)
                .into_iter()
                .map(|token| stem(self.analyzer, &token.term))
                .collect();
            return if term.is_empty() {
                Query::Nothing
            } else {
                Query::Term {
                    field,
                    term,
                    how: How::Fuzzy(distance),
                }
            };
        }
        self.phrase(field, word, None)
    }
}

/// Parses a full-text query for an index on `fields`.
fn parse_query(query: &str, fields: Vec<&str>, analyzer: Analyzer) -> Result<Query> {
    let mut parser = Parser {
        lexemes: lex(query)?,
        at: 0,
        fields,
        analyzer,
    };
    if parser.lexemes.is_empty() {
        return Err(Error::Invalid("the full-text query is empty".into()));
    }
    let query = parser.any()?;
    if parser.at < parser.lexemes.len() {
        return Err(Error::Invalid(format!(
            "unexpected {:?} in the full-text query",
            parser.lexemes[parser.at]
        )));
    }
    Ok(query)
}

// Searching.

/// A node a full-text search found.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FulltextHit {
    pub node: NodeId,
    pub score: f32,
    /// Where it matched, when asked for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fragments: Vec<Fragment>,
}

/// A piece of a property around the words that matched.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Fragment {
    pub field: String,
    pub text: String,
    /// The words that matched, as `[start, end)` offsets in characters of
    /// `text`.
    pub matches: Vec<(usize, usize)>,
}

/// One index's postings and statistics, while a query is evaluated.
struct Searcher<'a, T: ReadableTable<IndexKey, ()>> {
    table: &'a T,
    /// `fulltext_postings`, for an index in the postings format.
    postings: Option<&'a redb::ReadOnlyTable<PostingsKey, &'static [u8]>>,
    ns: &'a str,
    def: &'a IndexDef,
    fulltext: &'a Fulltext,
    documents: f32,
    average: Vec<f32>,
    /// The terms that matched, for fragments.
    matched: HashSet<String>,
    /// For the cache of the index's terms.
    shared: &'a crate::algo::Shared,
    generation: Option<u64>,
    /// The index's distinct terms, sorted, once read.
    dictionary: Option<Arc<Vec<String>>>,
    /// Scores already computed in this query, by field and term: a word,
    /// its fuzzy variants and a phrase often ask for the same term.
    scored: HashMap<(Option<usize>, String), Arc<Scores>>,
}

type Scores = HashMap<u128, f32>;

/// An entry read back: node, field, the field's length, the term's
/// frequency there, and its first positions.
type Entry = (u128, usize, u32, u32, Vec<u32>);

impl<T: ReadableTable<IndexKey, ()>> Searcher<'_, T> {
    fn range(&self, low: &[u8], high: &[u8]) -> Result<Vec<(Vec<u8>, u128)>> {
        let mut out = Vec::new();
        for item in self
            .table
            .range(
                (self.ns, self.def.name.as_str(), low, 0u128)
                    ..=(self.ns, self.def.name.as_str(), high, u128::MAX),
            )
            .map_err(storage)?
        {
            let (key, _) = item.map_err(storage)?;
            let (_, _, bytes, id) = key.value();
            out.push((bytes.to_vec(), id));
        }
        Ok(out)
    }

    /// Every entry of `term`: node, field, its length, frequency, positions.
    fn postings(&self, term: &str) -> Result<Vec<Entry>> {
        if let Some(table) = self.postings {
            let mut out = Vec::new();
            for item in table
                .range(
                    (self.ns, self.def.name.as_str(), term, 0u128)
                        ..=(self.ns, self.def.name.as_str(), term, u128::MAX),
                )
                .map_err(storage)?
            {
                let (key, value) = item.map_err(storage)?;
                let id = key.value().3;
                each_field(value.value(), |field| {
                    out.push((
                        id,
                        field.field,
                        field.length,
                        field.frequency,
                        field.positions(),
                    ));
                });
            }
            return Ok(out);
        }
        let mut low = vec![TERM];
        low.extend_from_slice(term.as_bytes());
        low.push(0);
        let mut high = low.clone();
        high.push(0xFF);
        Ok(self
            .range(&low, &high)?
            .into_iter()
            .filter_map(|(bytes, id)| {
                let posting = parse(&bytes)?;
                Some((
                    id,
                    posting.field,
                    posting.length,
                    posting.frequency,
                    posting.positions,
                ))
            })
            .collect())
    }

    /// The distinct terms that start with `prefix`, in order, at most
    /// `limit`: one range read each, skipping past every term's entries.
    fn terms_from(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        if let Some(table) = self.postings {
            let (ns, name) = (self.ns, self.def.name.as_str());
            let mut out: Vec<String> = Vec::new();
            while out.len() < limit {
                // Past the last term's postings, or from the prefix.
                let low = match out.last() {
                    Some(last) => std::ops::Bound::Excluded((ns, name, last.as_str(), u128::MAX)),
                    None => std::ops::Bound::Included((ns, name, prefix, 0u128)),
                };
                let high: std::ops::Bound<(&str, &str, &str, u128)> = std::ops::Bound::Unbounded;
                let mut range = table.range((low, high)).map_err(storage)?;
                let Some(item) = range.next() else {
                    break;
                };
                let (key, _) = item.map_err(storage)?;
                let (key_ns, key_name, term, _) = key.value();
                if key_ns != ns || key_name != name || !term.starts_with(prefix) {
                    break;
                }
                out.push(term.to_owned());
            }
            return Ok(out);
        }
        let mut out = Vec::new();
        let mut low = vec![TERM];
        low.extend_from_slice(prefix.as_bytes());
        let mut high = low.clone();
        high.push(0xFF);
        while out.len() < limit {
            let mut range = self
                .table
                .range(
                    (self.ns, self.def.name.as_str(), low.as_slice(), 0u128)
                        ..=(self.ns, self.def.name.as_str(), high.as_slice(), u128::MAX),
                )
                .map_err(storage)?;
            let Some(item) = range.next() else {
                break;
            };
            let (key, _) = item.map_err(storage)?;
            let Some(posting) = parse(key.value().2) else {
                break;
            };
            let term = posting.term.to_owned();
            // Past this term's entries: its bytes, then 1 (after its 0).
            low = vec![TERM];
            low.extend_from_slice(term.as_bytes());
            low.push(1);
            out.push(term);
        }
        Ok(out)
    }

    /// The index's distinct terms, sorted: cached per commit, so that a
    /// fuzzy or prefix word reads a list in memory instead of seeking every
    /// term that could match.
    fn dictionary(&mut self) -> Result<Arc<Vec<String>>> {
        if let Some(terms) = &self.dictionary {
            return Ok(terms.clone());
        }
        let cached = self.generation.and_then(|generation| {
            self.shared
                .fulltext_terms(generation, self.ns, &self.def.name)
        });
        let terms = match cached {
            Some(terms) => terms,
            None => {
                let terms = Arc::new(self.terms_from("", usize::MAX)?);
                if let Some(generation) = self.generation {
                    self.shared.keep_fulltext_terms(
                        generation,
                        self.ns,
                        &self.def.name,
                        terms.clone(),
                    );
                }
                terms
            }
        };
        self.dictionary = Some(terms.clone());
        Ok(terms)
    }

    /// BM25 of `term`, per field and weighted, for every node that has it.
    /// Its entries are read once per query, without copying them or their
    /// positions.
    fn term_scores(&mut self, field: Option<usize>, term: &str) -> Result<Arc<Scores>> {
        if let Some(scores) = self.scored.get(&(field, term.to_owned())) {
            return Ok(scores.clone());
        }
        let mut entries: Vec<(u128, usize, u32, u32)> = Vec::new();
        if let Some(table) = self.postings {
            for item in table
                .range(
                    (self.ns, self.def.name.as_str(), term, 0u128)
                        ..=(self.ns, self.def.name.as_str(), term, u128::MAX),
                )
                .map_err(storage)?
            {
                let (key, value) = item.map_err(storage)?;
                let id = key.value().3;
                each_field(value.value(), |posting| {
                    if field.is_none_or(|field| field == posting.field) {
                        entries.push((id, posting.field, posting.length, posting.frequency));
                    }
                });
            }
        }
        let mut low = vec![TERM];
        low.extend_from_slice(term.as_bytes());
        low.push(0);
        let mut high = low.clone();
        high.push(0xFF);
        if self.postings.is_none() {
            for item in self
                .table
                .range(
                    (self.ns, self.def.name.as_str(), low.as_slice(), 0u128)
                        ..=(self.ns, self.def.name.as_str(), high.as_slice(), u128::MAX),
                )
                .map_err(storage)?
            {
                let (key, _) = item.map_err(storage)?;
                let (_, _, bytes, id) = key.value();
                if let Some((entry_field, length, frequency)) = parse_counts(bytes)
                    && field.is_none_or(|field| field == entry_field)
                {
                    entries.push((id, entry_field, length, frequency));
                }
            }
        }
        let mut documents: Vec<u128> = entries.iter().map(|entry| entry.0).collect();
        documents.sort_unstable();
        documents.dedup();
        let found = documents.len() as f32;
        let idf = (1.0 + (self.documents - found + 0.5) / (found + 0.5)).ln();
        let mut scores = Scores::with_capacity(documents.len());
        for (id, field, length, frequency) in entries {
            let average = self.average.get(field).copied().unwrap_or(1.0).max(1.0);
            let frequency = frequency as f32;
            let norm = K1 * (1.0 - B + B * length as f32 / average);
            *scores.entry(id).or_default() +=
                self.fulltext.weight(field) * idf * frequency * (K1 + 1.0) / (frequency + norm);
        }
        let scores = Arc::new(scores);
        self.scored.insert((field, term.to_owned()), scores.clone());
        Ok(scores)
    }

    fn eval(&mut self, query: &Query, positive: bool) -> Result<Scores> {
        match query {
            Query::Nothing => Ok(Scores::new()),
            Query::Term {
                field,
                term,
                how: How::Exact,
            } => {
                let scores = self.term_scores(*field, term)?;
                if positive && !scores.is_empty() {
                    self.matched.insert(term.clone());
                }
                Ok(Scores::clone(&scores))
            }
            Query::Term { field, term, how } => {
                let dictionary = self.dictionary()?;
                let candidates: Vec<(String, f32)> = match how {
                    How::Prefix => {
                        let start =
                            dictionary.partition_point(|found| found.as_str() < term.as_str());
                        dictionary[start..]
                            .iter()
                            .take_while(|found| found.starts_with(term.as_str()))
                            .take(MAX_EXPANSIONS)
                            .map(|found| (found.clone(), 1.0))
                            .collect()
                    }
                    How::Fuzzy(distance) => {
                        // Terms that start with the same letter, at most
                        // `distance` edits away; the nearest are kept.
                        let target: Vec<char> = term.chars().collect();
                        let first: String = target.iter().take(1).collect();
                        let start =
                            dictionary.partition_point(|found| found.as_str() < first.as_str());
                        let mut near: Vec<(usize, &String)> = Vec::new();
                        for found in dictionary[start..]
                            .iter()
                            .take_while(|found| found.starts_with(first.as_str()))
                        {
                            // A character is at most four bytes.
                            if found.len().abs_diff(term.len()) > 4 * *distance {
                                continue;
                            }
                            let chars: Vec<char> = found.chars().collect();
                            if let Some(apart) =
                                crate::textsim::levenshtein_within(&target, &chars, *distance)
                            {
                                near.push((apart, found));
                            }
                        }
                        near.sort();
                        near.truncate(MAX_EXPANSIONS);
                        near.into_iter()
                            .map(|(apart, found)| (found.clone(), 1.0 - 0.2 * apart as f32))
                            .collect()
                    }
                    How::Exact => unreachable!("handled above"),
                };
                let mut scores = Scores::new();
                for (found, factor) in candidates {
                    let found_scores = self.term_scores(*field, &found)?;
                    if positive && !found_scores.is_empty() {
                        self.matched.insert(found.clone());
                    }
                    for (&id, &score) in found_scores.iter() {
                        let best = scores.entry(id).or_default();
                        *best = best.max(score * factor);
                    }
                }
                Ok(scores)
            }
            Query::Phrase { field, terms, near } => self.phrase(*field, terms, *near, positive),
            Query::All { must, should, not } => {
                let mut scores: Option<Scores> = None;
                for part in must {
                    let found = self.eval(part, positive)?;
                    scores = Some(match scores {
                        None => found,
                        Some(mut so_far) => {
                            so_far.retain(|id, _| found.contains_key(id));
                            for (id, score) in so_far.iter_mut() {
                                *score += found[id];
                            }
                            so_far
                        }
                    });
                }
                let required = scores.is_some();
                let mut scores = scores.unwrap_or_default();
                for part in should {
                    for (id, score) in self.eval(part, positive)? {
                        if required {
                            if let Some(so_far) = scores.get_mut(&id) {
                                *so_far += score;
                            }
                        } else {
                            *scores.entry(id).or_default() += score;
                        }
                    }
                }
                for part in not {
                    for id in self.eval(part, false)?.keys() {
                        scores.remove(id);
                    }
                }
                Ok(scores)
            }
        }
    }

    fn phrase(
        &mut self,
        field: Option<usize>,
        terms: &[String],
        near: Option<u32>,
        positive: bool,
    ) -> Result<Scores> {
        // Positions per node and field, for each term.
        let mut per_term: Vec<HashMap<(u128, usize), Vec<u32>>> = Vec::new();
        for term in terms {
            let mut found: HashMap<(u128, usize), Vec<u32>> = HashMap::new();
            for (id, posting_field, _, _, positions) in self.postings(term)? {
                if field.is_none_or(|field| field == posting_field) {
                    found.insert((id, posting_field), positions);
                }
            }
            per_term.push(found);
        }
        let mut places: BTreeSet<(u128, usize)> = per_term[0].keys().copied().collect();
        for found in &per_term[1..] {
            places.retain(|place| found.contains_key(place));
        }
        let mut hits = HashSet::new();
        for place in places {
            let lists: Vec<&Vec<u32>> = per_term.iter().map(|found| &found[&place]).collect();
            let matches = match near {
                None => lists[0].iter().any(|start| {
                    lists
                        .iter()
                        .enumerate()
                        .all(|(offset, list)| list.contains(&(start + offset as u32)))
                }),
                Some(distance) => {
                    let width = terms.len() as u32 - 1 + distance;
                    lists[0].iter().any(|start| {
                        lists[1..]
                            .iter()
                            .all(|list| list.iter().any(|p| p.abs_diff(*start) <= width))
                    })
                }
            };
            if matches {
                hits.insert(place.0);
            }
        }
        let mut scores = Scores::new();
        if hits.is_empty() {
            return Ok(scores);
        }
        for term in terms {
            for (&id, &score) in self.term_scores(field, term)?.iter() {
                if hits.contains(&id) {
                    *scores.entry(id).or_default() += score;
                }
            }
            if positive {
                self.matched.insert(term.clone());
            }
        }
        Ok(scores)
    }
}

/// Fragments of a node's properties around the words in `matched`.
fn fragments(
    def: &IndexDef,
    fulltext: &Fulltext,
    props: &Props,
    matched: &HashSet<String>,
) -> Vec<Fragment> {
    let mut out = Vec::new();
    let mut fields: Vec<(usize, &str)> = def.keys().into_iter().enumerate().collect();
    fields.sort_by(|a, b| fulltext.weight(b.0).total_cmp(&fulltext.weight(a.0)));
    for (_, key) in fields {
        let Some(value) = props.get(key) else {
            continue;
        };
        let text = texts_of(value).join(" \u{2014} ");
        let tokens = analyze(fulltext.analyzer, &text);
        let Some(first) = tokens
            .iter()
            .position(|token| matched.contains(&token.term))
        else {
            continue;
        };
        let from = first.saturating_sub(8);
        let to = (first + 16).min(tokens.len() - 1);
        let (start, end) = (tokens[from].start, tokens[to].end);
        let chars: Vec<char> = text.chars().collect();
        // Whole text when short; otherwise the words around the first match.
        let (start, end) = if chars.len() <= 200 {
            (0, chars.len())
        } else {
            (start, end)
        };
        let piece: String = chars[start..end].iter().collect();
        let matches = tokens
            .iter()
            .filter(|token| {
                matched.contains(&token.term) && token.start >= start && token.end <= end
            })
            .map(|token| (token.start - start, token.end - start))
            .collect();
        out.push(Fragment {
            field: key.to_owned(),
            text: piece,
            matches,
        });
        if out.len() == 3 {
            break;
        }
    }
    out
}

impl crate::WriteTx {
    /// Marks the file layout 21 before it writes its first postings,
    /// keeping the layout it had.
    pub(crate) fn postings_layout(&mut self) -> Result<()> {
        mark_postings_layout(&self.tx)
    }

    /// When no full-text index keeps the postings format any more: drops
    /// `fulltext_postings` and gives the file the layout it had without
    /// them. `Err` says why not.
    pub(crate) fn leave_postings_layout(&mut self) -> Result<std::result::Result<u64, String>> {
        use redb::ReadableTableMetadata;
        {
            let stats = self.tx.open_table(FULLTEXT_STATS).map_err(storage)?;
            for item in stats.iter().map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                let found: FieldStats = serde_json::from_slice(value.value())
                    .map_err(|e| Error::Serialization(e.to_string()))?;
                if found.postings {
                    let (ns, name) = key.value();
                    return Ok(Err(format!(
                        "index {name} of {ns} keeps the postings format"
                    )));
                }
            }
        }
        if crate::compress::has_table(&self.tx, redb::TableHandle::name(&FULLTEXT_POSTINGS))? {
            self.tx.delete_table(FULLTEXT_POSTINGS).map_err(storage)?;
        }
        let hyperedges = self
            .tx
            .open_table(crate::hyperedge::HYPEREDGES)
            .map_err(storage)?
            .len()
            .map_err(storage)?
            > 0;
        let mut meta = self.tx.open_table(crate::store::META).map_err(storage)?;
        let stored = meta
            .get(crate::db::SCHEMA_KEY)
            .map_err(storage)?
            .map_or(crate::BASE_LAYOUT, |guard| guard.value());
        if stored != crate::POSTINGS_LAYOUT {
            return Ok(Ok(stored));
        }
        let before = meta
            .remove(crate::db::LAYOUT_BEFORE_POSTINGS)
            .map_err(storage)?
            .map_or(crate::BASE_LAYOUT, |guard| guard.value());
        let layout = if hyperedges {
            before.max(crate::HYPEREDGE_LAYOUT)
        } else {
            before
        };
        meta.insert(crate::db::SCHEMA_KEY, layout)
            .map_err(storage)?;
        Ok(Ok(layout))
    }
}

/// Marks the file layout 21, keeping the layout it had.
pub(crate) fn mark_postings_layout(tx: &redb::WriteTransaction) -> Result<()> {
    let mut meta = tx.open_table(crate::store::META).map_err(storage)?;
    let stored = meta
        .get(crate::db::SCHEMA_KEY)
        .map_err(storage)?
        .map_or(crate::BASE_LAYOUT, |guard| guard.value());
    if stored < crate::POSTINGS_LAYOUT {
        meta.insert(crate::db::LAYOUT_BEFORE_POSTINGS, stored)
            .map_err(storage)?;
        meta.insert(crate::db::SCHEMA_KEY, crate::POSTINGS_LAYOUT)
            .map_err(storage)?;
    }
    Ok(())
}

/// How many postings index `name` holds (the postings format).
pub(crate) fn count_postings(
    table: &impl ReadableTable<PostingsKey, &'static [u8]>,
    ns: &str,
    name: &str,
) -> Result<u64> {
    let mut count = 0;
    for item in table.range((ns, name, "", 0u128)..).map_err(storage)? {
        let (key, _) = item.map_err(storage)?;
        let (key_ns, key_name, _, _) = key.value();
        if key_ns != ns || key_name != name {
            break;
        }
        count += 1;
    }
    Ok(count)
}

impl ReadTx {
    /// Up to `limit` nodes of full-text index `index` that match `query`,
    /// best first (BM25 per field, weighted), with the fragments that
    /// matched when `with_fragments`.
    ///
    /// A query is words (any may match, the more the better), `"a
    /// phrase"`, `"terms near"~3` (all within three words of each other),
    /// `+required`, `-excluded` (or `NOT`), `a AND b`, `a OR b`,
    /// parentheses, `prefix*`, `fuzzy~1` (one edit away, at most two), and
    /// `field:word` or `field:"a phrase"` for one of the index's
    /// properties. Words go through the index's analyzer; prefixes are
    /// matched against its terms (stems, for a stemming analyzer).
    pub fn fulltext_search(
        &self,
        ns: &str,
        index: &str,
        query: &str,
        limit: usize,
        with_fragments: bool,
    ) -> Result<Vec<FulltextHit>> {
        self.fulltext_search_filtered(ns, index, query, limit, with_fragments, None)
    }

    /// [`ReadTx::fulltext_search`] keeping only the nodes `filter` passes
    /// (any of its labels, each of its property values), before `limit`
    /// is counted: as the `labels` and `where` of a vector search.
    pub fn fulltext_search_filtered(
        &self,
        ns: &str,
        index: &str,
        query: &str,
        limit: usize,
        with_fragments: bool,
        filter: Option<&crate::VectorFilter>,
    ) -> Result<Vec<FulltextHit>> {
        self.shared.index_uses.note(ns, index);
        let def = self
            .index_defs(ns)?
            .into_iter()
            .find(|def| def.name == index)
            .ok_or_else(|| Error::Invalid(format!("there is no index named {index:?}")))?;
        let Some(fulltext) = def.fulltext.clone() else {
            return Err(Error::Invalid(format!(
                "{index} is not a full-text index (CREATE FULLTEXT INDEX makes one)"
            )));
        };
        let parsed = parse_query(query, def.keys(), fulltext.analyzer)?;
        let stats = read_stats(&self.fulltext_stats, ns, &def.name)?;
        if stats.documents == 0 || limit == 0 {
            return Ok(Vec::new());
        }
        let average = stats
            .terms
            .iter()
            .map(|terms| *terms as f32 / stats.documents as f32)
            .collect();
        let mut searcher = Searcher {
            table: &self.index_entries,
            postings: if stats.postings {
                self.fulltext_postings.as_ref()
            } else {
                None
            },
            ns,
            def: &def,
            fulltext: &fulltext,
            documents: stats.documents as f32,
            average,
            matched: HashSet::new(),
            shared: &self.shared,
            generation: self.generation,
            dictionary: None,
            scored: HashMap::new(),
        };
        let scores = searcher.eval(&parsed, true)?;
        let matched = searcher.matched;
        let mut ranked: Vec<(u128, f32)> = scores.into_iter().collect();
        let order = |a: &(u128, f32), b: &(u128, f32)| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0));
        // The best first, without sorting every match: a few more than
        // `limit` (a hidden node is skipped), the rest only if they run out.
        let head = limit.saturating_mul(2).saturating_add(16);
        if ranked.len() > head {
            ranked.select_nth_unstable_by(head, order);
            ranked[..head].sort_unstable_by(order);
        } else {
            ranked.sort_unstable_by(order);
        }
        let mut out = Vec::new();
        let mut sorted_rest = ranked.len() <= head;
        let mut at = 0;
        while at < ranked.len() {
            if at == head && !sorted_rest {
                ranked[head..].sort_unstable_by(order);
                sorted_rest = true;
            }
            let (id, score) = ranked[at];
            at += 1;
            let node = NodeId::from_u128(id);
            // `node` leaves out what the view hides.
            let Some(found) = self.node(ns, node)? else {
                continue;
            };
            if let Some(filter) = filter
                && !((filter.labels.is_empty() || filter.labels.iter().any(|l| found.has_label(l)))
                    && filter
                        .props
                        .iter()
                        .all(|(key, value)| found.props.get(key) == Some(value)))
            {
                continue;
            }
            out.push(FulltextHit {
                node,
                score,
                fragments: if with_fragments {
                    fragments(&def, &fulltext, &found.props, &matched)
                } else {
                    Vec::new()
                },
            });
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }

    /// Text search, vector search, or both fused, as `request` says.
    pub fn search(&self, ns: &str, request: &SearchRequest) -> Result<Vec<SearchHit>> {
        let wants_text = request.mode != SearchMode::Vector;
        let wants_vector = request.mode != SearchMode::Text;
        let text = match (&request.text, wants_text) {
            (Some(text), true) => Some(text),
            (None, true) => {
                return Err(Error::Invalid(format!(
                    "a {} search needs the text to search",
                    request.mode.name()
                )));
            }
            _ => None,
        };
        let embedding = match (&request.embedding, wants_vector) {
            (Some(embedding), true) => Some(embedding),
            (None, true) => {
                return Err(Error::Invalid(format!(
                    "a {} search needs an embedding",
                    request.mode.name()
                )));
            }
            _ => None,
        };
        let pool = if request.mode == SearchMode::Hybrid {
            request.candidates.max(request.limit)
        } else {
            request.limit
        };
        let mut hits: HashMap<NodeId, SearchHit> = HashMap::new();
        let mut text_hits: Vec<(NodeId, f32)> = Vec::new();
        if let Some(text) = text {
            match &request.index {
                Some(index) => {
                    for (rank, hit) in self
                        .fulltext_search(ns, index, text, pool, request.fragments)?
                        .into_iter()
                        .enumerate()
                    {
                        text_hits.push((hit.node, hit.score));
                        let entry = hits
                            .entry(hit.node)
                            .or_insert_with(|| SearchHit::new(hit.node));
                        entry.text_rank = Some(rank + 1);
                        entry.text_score = Some(hit.score);
                        entry.fragments = hit.fragments;
                    }
                }
                None => {
                    for (rank, hit) in self.search_text(ns, text, pool)?.into_iter().enumerate() {
                        text_hits.push((hit.node, hit.score));
                        let entry = hits
                            .entry(hit.node)
                            .or_insert_with(|| SearchHit::new(hit.node));
                        entry.text_rank = Some(rank + 1);
                        entry.text_score = Some(hit.score);
                    }
                }
            }
        }
        let mut vector_hits: Vec<(NodeId, f32)> = Vec::new();
        if let Some(embedding) = embedding {
            let found = match &request.vector_index {
                Some(index) => self.vector_search(
                    ns,
                    embedding,
                    &crate::VectorSearch {
                        index: Some(index.clone()),
                        limit: pool,
                        ..crate::VectorSearch::default()
                    },
                )?,
                None => self.search_vector(ns, embedding, pool)?,
            };
            for (rank, hit) in found.into_iter().enumerate() {
                vector_hits.push((hit.node, hit.score));
                let entry = hits
                    .entry(hit.node)
                    .or_insert_with(|| SearchHit::new(hit.node));
                entry.vector_rank = Some(rank + 1);
                entry.similarity = Some(hit.score);
            }
        }
        let fused = fuse(&text_hits, &vector_hits, request.mode, request.fusion);
        let mut out: Vec<SearchHit> = fused
            .into_iter()
            .filter_map(|(node, score)| {
                hits.remove(&node).map(|mut hit| {
                    hit.score = score;
                    hit
                })
            })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        out.truncate(request.limit);
        Ok(out)
    }
}

/// Text, vector, or both: how [`ReadTx::search`] finds nodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    #[default]
    Text,
    Vector,
    Hybrid,
}

impl SearchMode {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "text" => SearchMode::Text,
            "vector" => SearchMode::Vector,
            "hybrid" => SearchMode::Hybrid,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            SearchMode::Text => "text",
            SearchMode::Vector => "vector",
            SearchMode::Hybrid => "hybrid",
        }
    }
}

/// How a hybrid search puts its two lists together.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fusion {
    /// Each list adds `1 / (k + rank)`: only ranks count, so scores of
    /// different kinds need no scaling.
    Rank { k: f32 },
    /// Each list's scores scaled to 0..1 over the candidates, then
    /// `text × weight + vector × (1 − weight)`.
    Weighted { text: f32 },
}

impl Default for Fusion {
    fn default() -> Self {
        Fusion::Rank { k: 60.0 }
    }
}

/// What [`ReadTx::search`] looks for.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchRequest {
    pub text: Option<String>,
    /// A full-text index for the text (its query language); without one,
    /// the node text, word by word.
    pub index: Option<String>,
    pub embedding: Option<Vec<f32>>,
    /// A vector index for the embedding; without one, the node embeddings.
    pub vector_index: Option<String>,
    pub mode: SearchMode,
    pub fusion: Fusion,
    pub limit: usize,
    /// Taken from each list before fusing them.
    pub candidates: usize,
    /// Fragments of what matched (with an index).
    pub fragments: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            text: None,
            index: None,
            embedding: None,
            vector_index: None,
            mode: SearchMode::Text,
            fusion: Fusion::default(),
            limit: 10,
            candidates: 50,
            fragments: false,
        }
    }
}

/// A node [`ReadTx::search`] found, and how.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchHit {
    pub node: NodeId,
    /// The text score, the similarity, or the fused score, as the mode is.
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fragments: Vec<Fragment>,
}

impl SearchHit {
    fn new(node: NodeId) -> Self {
        Self {
            node,
            score: 0.0,
            text_rank: None,
            text_score: None,
            vector_rank: None,
            similarity: None,
            fragments: Vec::new(),
        }
    }
}

/// How much lower a node one list lacks counts in it than in the list that
/// has it: absent from the text list (no word in common: another language,
/// a synonym) is weak evidence against a node the vector list ranks high,
/// and a node only one list finds must not fall below every node both
/// lists find a little.
const MISSING_FACTOR: f32 = 4.0;

/// The same for weighted fusion, on scores scaled to 0..1, where a list's
/// weakest matches are stretched over the whole range.
const MISSING_SHARE: f32 = 0.5;

/// Scores of two ranked lists put together (`mode` text or vector: that
/// list's own scores). In a hybrid, a node one list lacks counts there as
/// if that list ranked it [`MISSING_FACTOR`] times lower than the other
/// one does (by rank), or gave it [`MISSING_SHARE`] of the other's scaled
/// score (weighted); when
/// one list is empty, the other's order is kept.
pub fn fuse(
    text: &[(NodeId, f32)],
    vector: &[(NodeId, f32)],
    mode: SearchMode,
    fusion: Fusion,
) -> Vec<(NodeId, f32)> {
    match mode {
        SearchMode::Text => return text.to_vec(),
        SearchMode::Vector => return vector.to_vec(),
        SearchMode::Hybrid => {}
    }
    let both = !text.is_empty() && !vector.is_empty();
    let mut scores: HashMap<NodeId, f32> = HashMap::new();
    match fusion {
        Fusion::Rank { k } => {
            let rank_in = |list: &[(NodeId, f32)]| -> HashMap<NodeId, f32> {
                list.iter()
                    .enumerate()
                    .map(|(rank, (node, _))| (*node, rank as f32 + 1.0))
                    .collect()
            };
            let (in_text, in_vector) = (rank_in(text), rank_in(vector));
            for node in in_text.keys().chain(in_vector.keys()) {
                if scores.contains_key(node) {
                    continue;
                }
                let score = match (in_text.get(node), in_vector.get(node)) {
                    (Some(t), Some(v)) => 1.0 / (k + t) + 1.0 / (k + v),
                    (Some(r), None) | (None, Some(r)) if both => {
                        1.0 / (k + r) + 1.0 / (k + MISSING_FACTOR * r)
                    }
                    (Some(r), None) | (None, Some(r)) => 1.0 / (k + r),
                    (None, None) => continue,
                };
                scores.insert(*node, score);
            }
        }
        Fusion::Weighted { text: weight } => {
            let weight = weight.clamp(0.0, 1.0);
            let scaled = |list: &[(NodeId, f32)]| -> HashMap<NodeId, f32> {
                let low = list.iter().map(|(_, s)| *s).fold(f32::INFINITY, f32::min);
                let high = list
                    .iter()
                    .map(|(_, s)| *s)
                    .fold(f32::NEG_INFINITY, f32::max);
                list.iter()
                    .map(|(node, score)| {
                        let scaled = if high > low {
                            (score - low) / (high - low)
                        } else {
                            1.0
                        };
                        (*node, scaled)
                    })
                    .collect()
            };
            let (in_text, in_vector) = (scaled(text), scaled(vector));
            for node in in_text.keys().chain(in_vector.keys()) {
                if scores.contains_key(node) {
                    continue;
                }
                let (t, v) = match (in_text.get(node), in_vector.get(node)) {
                    (Some(t), Some(v)) => (*t, *v),
                    (Some(t), None) if both => (*t, t * MISSING_SHARE),
                    (None, Some(v)) if both => (v * MISSING_SHARE, *v),
                    (Some(t), None) => (*t, 0.0),
                    (None, Some(v)) => (0.0, *v),
                    (None, None) => continue,
                };
                scores.insert(*node, weight * t + (1.0 - weight) * v);
            }
        }
    }
    let mut out: Vec<(NodeId, f32)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzers_and_offsets() {
        let tokens = analyze(Analyzer::Standard, "La Canción de Ana");
        assert_eq!(
            tokens,
            vec![
                Token {
                    term: "cancion".into(),
                    start: 3,
                    end: 10
                },
                Token {
                    term: "ana".into(),
                    start: 14,
                    end: 17
                }
            ]
        );
        let stems = |analyzer, text| -> Vec<String> {
            analyze(analyzer, text)
                .into_iter()
                .map(|t| t.term)
                .collect()
        };
        assert_eq!(
            stems(
                Analyzer::English,
                "graphs graphing studies running stopped quickly"
            ),
            vec!["graph", "graph", "study", "run", "stop", "quick"]
        );
        assert_eq!(
            stems(
                Analyzer::Spanish,
                "los grafos y el grafo, canciones rápidamente"
            ),
            vec!["graf", "graf", "cancion", "rapid"]
        );
        assert_eq!(stems(Analyzer::Simple, "a B"), vec!["a", "b"]);
    }

    #[test]
    fn queries_parse() {
        let parse = |q: &str| parse_query(q, vec!["title", "body"], Analyzer::Standard).unwrap();
        assert_eq!(
            parse("graph*"),
            Query::Term {
                field: None,
                term: "graph".into(),
                how: How::Prefix
            }
        );
        assert_eq!(
            parse("title:\"Graph Memory\"~2"),
            Query::Phrase {
                field: Some(0),
                terms: vec!["graph".into(), "memory".into()],
                near: Some(2)
            }
        );
        assert!(
            matches!(parse("+a1 -b2 c3"), Query::All { must, should, not }
            if must.len() == 1 && should.len() == 1 && not.len() == 1)
        );
        assert!(parse_query("-only", vec![], Analyzer::Standard).is_err());
        assert!(parse_query("\"open", vec![], Analyzer::Standard).is_err());
        assert!(parse_query("(a", vec![], Analyzer::Standard).is_err());
    }

    #[test]
    fn rank_fusion() {
        let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
        let fused = fuse(
            &[(a, 9.0), (b, 3.0)],
            &[(b, 0.9), (c, 0.8)],
            SearchMode::Hybrid,
            Fusion::default(),
        );
        assert_eq!(fused[0].0, b, "in both lists");
        let weighted = fuse(
            &[(a, 9.0), (b, 3.0)],
            &[(b, 0.9), (c, 0.8)],
            SearchMode::Hybrid,
            Fusion::Weighted { text: 1.0 },
        );
        assert_eq!(weighted[0].0, a, "text alone");
    }

    /// A node the vector list ranks near the top and the text list lacks
    /// (another language) stays among the first, ahead of nodes both lists
    /// find only further down: by rank and weighted.
    #[test]
    fn a_strong_match_in_one_list_is_not_lost() {
        let nodes: Vec<NodeId> = (0..30).map(|_| NodeId::new()).collect();
        let target = nodes[0];
        // The text list: 20 weak matches of a common word, not the target.
        let text: Vec<(NodeId, f32)> = nodes[1..21]
            .iter()
            .enumerate()
            .map(|(i, n)| (*n, 2.0 - i as f32 * 0.05))
            .collect();
        // The vector list: everything, the target second.
        let mut order: Vec<NodeId> = nodes[1..].to_vec();
        order.insert(1, target);
        let vector: Vec<(NodeId, f32)> = order
            .iter()
            .enumerate()
            .map(|(i, n)| (*n, 0.9 - i as f32 * 0.02))
            .collect();
        for fusion in [Fusion::default(), Fusion::Weighted { text: 0.5 }] {
            let fused = fuse(&text, &vector, SearchMode::Hybrid, fusion);
            // Before, it fell to 20th by rank and 14th weighted, below
            // nodes of the text list's tail: now only nodes both lists rank
            // high come first.
            let at = fused.iter().position(|(n, _)| *n == target).unwrap();
            let within = if fusion == Fusion::default() { 5 } else { 8 };
            assert!(at < within, "{fusion:?}: at {at}");
        }
    }
}
