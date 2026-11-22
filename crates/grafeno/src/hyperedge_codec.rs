//! How a hyperedge record is stored: binary for what every read needs
//! (its id, label, participants, sources and times), JSON for its
//! properties, which are free-form.
//!
//! ```text
//! 0x01                      format
//! id                        16 bytes, little-endian
//! label                     varint length, UTF-8
//! participants              varint count; each: node (16 bytes) and role
//!                           (varint length + 1, 0 for none, then UTF-8)
//! sources                   varint count; each 16 bytes
//! flags                     1 byte: valid_from, valid_to, expired_at,
//!                           recorded_by present
//! recorded_at               varint (microseconds)
//! valid_from, valid_to,
//! expired_at                varint each, when its flag is set
//! recorded_by               varint length, UTF-8, when its flag is set
//! props                     the rest: a JSON object (nothing for none)
//! ```
//!
//! Varints are zigzag LEB128, as in the adjacency tables. Records written
//! as JSON (before this format, in development builds of layout 19) start
//! with `{` and are still read.

use std::ops::Bound;

use grafeno_core::{Error, HyperEdge, HyperEdgeId, NodeId, Participant, Props, Result, Timestamp};
use redb::ReadableTable;

use crate::store::{RecordKey, put_varint, storage, take_varint};

const FORMAT: u8 = 1;
const VALID_FROM: u8 = 1;
const VALID_TO: u8 = 2;
const EXPIRED: u8 = 4;
const RECORDED_BY: u8 = 8;

fn put_text(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len() as i64);
    out.extend_from_slice(text.as_bytes());
}

/// The bytes stored for `edge`.
pub(crate) fn encode(edge: &HyperEdge) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(64 + 24 * edge.participants.len());
    out.push(FORMAT);
    out.extend_from_slice(&edge.id.as_u128().to_le_bytes());
    put_text(&mut out, &edge.label);
    put_varint(&mut out, edge.participants.len() as i64);
    for participant in &edge.participants {
        out.extend_from_slice(&participant.node.as_u128().to_le_bytes());
        match &participant.role {
            Some(role) => {
                put_varint(&mut out, role.len() as i64 + 1);
                out.extend_from_slice(role.as_bytes());
            }
            None => put_varint(&mut out, 0),
        }
    }
    put_varint(&mut out, edge.sources.len() as i64);
    for source in &edge.sources {
        out.extend_from_slice(&source.as_u128().to_le_bytes());
    }
    let mut flags = 0;
    for (flag, present) in [
        (VALID_FROM, edge.valid_from.is_some()),
        (VALID_TO, edge.valid_to.is_some()),
        (EXPIRED, edge.expired_at.is_some()),
        (RECORDED_BY, edge.recorded_by.is_some()),
    ] {
        if present {
            flags |= flag;
        }
    }
    out.push(flags);
    put_varint(&mut out, edge.recorded_at.as_micros());
    for time in [edge.valid_from, edge.valid_to, edge.expired_at]
        .into_iter()
        .flatten()
    {
        put_varint(&mut out, time.as_micros());
    }
    if let Some(by) = &edge.recorded_by {
        put_text(&mut out, by);
    }
    if !edge.props.is_empty() {
        serde_json::to_writer(&mut out, &edge.props)
            .map_err(|e| Error::Serialization(e.to_string()))?;
    }
    Ok(out)
}

/// Reads the bytes in order, refusing what runs short.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn broken() -> Error {
        Error::Serialization("a hyperedge record is cut short or malformed".into())
    }

    fn bytes(&mut self, count: usize) -> Result<&[u8]> {
        let end = self.at.checked_add(count).ok_or_else(Self::broken)?;
        let slice = self.data.get(self.at..end).ok_or_else(Self::broken)?;
        self.at = end;
        Ok(slice)
    }

    fn id(&mut self) -> Result<u128> {
        let bytes: [u8; 16] = self.bytes(16)?.try_into().map_err(|_| Self::broken())?;
        Ok(u128::from_le_bytes(bytes))
    }

    fn varint(&mut self) -> Result<i64> {
        if self.at >= self.data.len() {
            return Err(Self::broken());
        }
        Ok(take_varint(self.data, &mut self.at))
    }

    fn count(&mut self) -> Result<usize> {
        let value = self.varint()?;
        usize::try_from(value).map_err(|_| Self::broken())
    }

    fn text(&mut self, length: usize) -> Result<String> {
        let bytes = self.bytes(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| Self::broken())
    }
}

/// The hyperedge `bytes` hold (binary, or JSON from before the format).
pub(crate) fn decode(bytes: &[u8]) -> Result<HyperEdge> {
    match bytes.first() {
        Some(b'{') => {
            return serde_json::from_slice(bytes).map_err(|e| Error::Serialization(e.to_string()));
        }
        Some(&FORMAT) => {}
        _ => return Err(Reader::broken()),
    }
    let mut r = Reader { data: bytes, at: 1 };
    let id = HyperEdgeId::from_u128(r.id()?);
    let length = r.count()?;
    let label = r.text(length)?;
    let count = r.count()?;
    let mut participants = Vec::with_capacity(count.min(4_096));
    for _ in 0..count {
        let node = NodeId::from_u128(r.id()?);
        let role = match r.count()? {
            0 => None,
            length => Some(r.text(length - 1)?),
        };
        participants.push(Participant { node, role });
    }
    let count = r.count()?;
    let mut sources = Vec::with_capacity(count.min(4_096));
    for _ in 0..count {
        sources.push(NodeId::from_u128(r.id()?));
    }
    let flags = r.bytes(1)?[0];
    let recorded_at = Timestamp::from_micros(r.varint()?);
    let time = |flag: u8, r: &mut Reader| -> Result<Option<Timestamp>> {
        Ok(if flags & flag != 0 {
            Some(Timestamp::from_micros(r.varint()?))
        } else {
            None
        })
    };
    let valid_from = time(VALID_FROM, &mut r)?;
    let valid_to = time(VALID_TO, &mut r)?;
    let expired_at = time(EXPIRED, &mut r)?;
    let recorded_by = if flags & RECORDED_BY != 0 {
        let length = r.count()?;
        Some(r.text(length)?)
    } else {
        None
    };
    let rest = &bytes[r.at..];
    let props: Props = if rest.is_empty() {
        Props::new()
    } else {
        serde_json::from_slice(rest).map_err(|e| Error::Serialization(e.to_string()))?
    };
    Ok(HyperEdge {
        id,
        label,
        participants,
        props,
        sources,
        valid_from,
        valid_to,
        recorded_at,
        expired_at,
        recorded_by,
    })
}

/// The hyperedge with this id in `table`.
pub(crate) fn read(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    id: u128,
) -> Result<Option<HyperEdge>> {
    match table.get((ns, id)).map_err(storage)? {
        Some(guard) => decode(guard.value()).map(Some),
        None => Ok(None),
    }
}

/// Every hyperedge of `ns` in `table`, in id order.
pub(crate) fn scan(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
) -> Result<Vec<HyperEdge>> {
    page(table, ns, None, usize::MAX)
}

/// Up to `limit` hyperedges of `ns` with an id greater than `after`.
pub(crate) fn page(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    after: Option<u128>,
    limit: usize,
) -> Result<Vec<HyperEdge>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    let lower = match after {
        Some(id) => Bound::Excluded((ns, id)),
        None => Bound::Included((ns, 0u128)),
    };
    let bounds: crate::store::Bounds<(&str, u128)> = (lower, Bound::Unbounded);
    for item in table.range(bounds).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        if key.value().0 != ns {
            break;
        }
        out.push(decode(value.value())?);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> HyperEdge {
        let mut props = Props::new();
        props.insert("status".into(), json!("approved"));
        props.insert("tags".into(), json!(["a", "b"]));
        props.insert("empty".into(), json!(null));
        HyperEdge {
            id: HyperEdgeId::new(),
            label: "décision".into(),
            participants: vec![
                Participant::with_role(NodeId::new(), "proposer"),
                Participant::new(NodeId::new()),
                Participant::with_role(NodeId::new(), ""),
            ],
            props,
            sources: vec![NodeId::new()],
            valid_from: Some(Timestamp::from_micros(-5)),
            valid_to: Some(Timestamp::from_micros(1_893_456_000_000_000)),
            recorded_at: Timestamp::now(),
            expired_at: None,
            recorded_by: Some("agent".into()),
        }
    }

    #[test]
    fn a_record_comes_back_as_it_was() {
        let edge = sample();
        assert_eq!(decode(&encode(&edge).unwrap()).unwrap(), edge);
        // The fewest fields.
        let bare = HyperEdge {
            props: Props::new(),
            sources: Vec::new(),
            valid_from: None,
            valid_to: None,
            expired_at: Some(Timestamp::from_micros(7)),
            recorded_by: None,
            ..sample()
        };
        assert_eq!(decode(&encode(&bare).unwrap()).unwrap(), bare);
    }

    #[test]
    fn json_records_are_still_read() {
        let edge = sample();
        let json = serde_json::to_vec(&edge).unwrap();
        assert_eq!(decode(&json).unwrap(), edge);
    }

    #[test]
    fn it_is_smaller_than_json() {
        let edge = sample();
        assert!(encode(&edge).unwrap().len() * 2 < serde_json::to_vec(&edge).unwrap().len());
    }

    #[test]
    fn short_or_strange_bytes_are_refused() {
        let bytes = encode(&sample()).unwrap();
        for end in [0, 1, 10, 17, 30] {
            assert!(decode(&bytes[..end]).is_err(), "cut at {end}");
        }
        assert!(decode(&[9, 9, 9]).is_err());
        let mut huge = vec![FORMAT];
        huge.extend_from_slice(&0u128.to_le_bytes());
        put_varint(&mut huge, i64::MAX);
        assert!(decode(&huge).is_err());
    }
}
