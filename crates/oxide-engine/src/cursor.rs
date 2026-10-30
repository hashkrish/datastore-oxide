//! Query cursors, encoded exactly like the Java emulator's: an App Engine v3 `CompiledCursor`
//! whose `postfix_position` (field 1) is an `IndexPostfix`:
//!
//! ```text
//! IndexPostfix { repeated IndexValue index_value = 1 { string property_name = 1; PropertyValue value = 2; }
//!                Reference key = 2; bool before = 3; bool before_ascending = 4; }
//! ```
//!
//! An empty position encodes as `0a 00` (`CgA=`). The v3 protos declare these byte fields as
//! `string` but store raw bytes, so they are encoded by hand rather than with generated types.

use bytes::{Buf, BufMut};
use oxide_model::{IdOrName, Key, PartitionId, PathElement};
use prost::encoding::{WireType, decode_key, decode_varint, encode_key, encode_varint};

use crate::index::IndexValue;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Position {
    /// Values of the index row's properties, in index order.
    pub index_values: Vec<(String, IndexValue)>,
    pub key: Option<Key>,
    pub before: bool,
    pub before_ascending: bool,
}

impl Position {
    pub fn is_empty(&self) -> bool {
        self.index_values.is_empty() && self.key.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError;

pub fn encode(position: &Position) -> Vec<u8> {
    let mut postfix = Vec::new();
    if !position.is_empty() {
        for (name, value) in &position.index_values {
            let mut iv = Vec::new();
            put_bytes(&mut iv, 1, name.as_bytes());
            put_bytes(&mut iv, 2, &encode_property_value(value));
            put_bytes(&mut postfix, 1, &iv);
        }
        if let Some(key) = &position.key {
            put_bytes(&mut postfix, 2, &encode_reference(key));
        }
        put_varint(&mut postfix, 3, u64::from(position.before));
        put_varint(&mut postfix, 4, u64::from(position.before_ascending));
    }
    let mut out = Vec::new();
    put_bytes(&mut out, 1, &postfix);
    out
}

fn put_varint(buf: &mut Vec<u8>, tag: u32, v: u64) {
    encode_key(tag, WireType::Varint, buf);
    encode_varint(v, buf);
}

fn put_bytes(buf: &mut Vec<u8>, tag: u32, b: &[u8]) {
    encode_key(tag, WireType::LengthDelimited, buf);
    encode_varint(b.len() as u64, buf);
    buf.put_slice(b);
}

fn put_double(buf: &mut Vec<u8>, tag: u32, d: f64) {
    encode_key(tag, WireType::SixtyFourBit, buf);
    buf.put_f64_le(d);
}

fn encode_reference(key: &Key) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, 13, key.partition.project_id.as_bytes());
    let mut path = Vec::new();
    for element in &key.path {
        encode_key(1, WireType::StartGroup, &mut path);
        put_bytes(&mut path, 2, element.kind.as_bytes());
        match &element.id {
            Some(IdOrName::Id(id)) => put_varint(&mut path, 3, *id as u64),
            Some(IdOrName::Name(name)) => put_bytes(&mut path, 4, name.as_bytes()),
            None => {}
        }
        encode_key(1, WireType::EndGroup, &mut path);
    }
    put_bytes(&mut out, 14, &path);
    if !key.partition.namespace_id.is_empty() {
        put_bytes(&mut out, 20, key.partition.namespace_id.as_bytes());
    }
    if !key.partition.database_id.is_empty() {
        put_bytes(&mut out, 23, key.partition.database_id.as_bytes());
    }
    out
}

fn encode_property_value(value: &IndexValue) -> Vec<u8> {
    let mut out = Vec::new();
    match value {
        IndexValue::Null => {}
        IndexValue::Int(i) => put_varint(&mut out, 1, *i as u64),
        IndexValue::Bool(b) => put_varint(&mut out, 2, u64::from(*b)),
        IndexValue::Bytes(b) => put_bytes(&mut out, 3, b),
        IndexValue::Double(d) => put_double(&mut out, 4, *d),
        IndexValue::Point {
            latitude,
            longitude,
        } => {
            encode_key(5, WireType::StartGroup, &mut out);
            put_double(&mut out, 6, *latitude);
            put_double(&mut out, 7, *longitude);
            encode_key(5, WireType::EndGroup, &mut out);
        }
        IndexValue::Key(key) => {
            encode_key(12, WireType::StartGroup, &mut out);
            put_bytes(&mut out, 13, key.partition.project_id.as_bytes());
            for element in &key.path {
                encode_key(14, WireType::StartGroup, &mut out);
                put_bytes(&mut out, 15, element.kind.as_bytes());
                match &element.id {
                    Some(IdOrName::Id(id)) => put_varint(&mut out, 16, *id as u64),
                    Some(IdOrName::Name(name)) => put_bytes(&mut out, 17, name.as_bytes()),
                    None => {}
                }
                encode_key(14, WireType::EndGroup, &mut out);
            }
            if !key.partition.namespace_id.is_empty() {
                put_bytes(&mut out, 20, key.partition.namespace_id.as_bytes());
            }
            if !key.partition.database_id.is_empty() {
                put_bytes(&mut out, 23, key.partition.database_id.as_bytes());
            }
            encode_key(12, WireType::EndGroup, &mut out);
        }
    }
    out
}

/// A minimal protobuf reader for the cursor messages.
struct Reader<'a> {
    buf: &'a [u8],
}

enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64(f64),
    StartGroup,
    EndGroup,
}

impl<'a> Reader<'a> {
    fn next(&mut self) -> Result<Option<(u32, Field<'a>)>, DecodeError> {
        if !self.buf.has_remaining() {
            return Ok(None);
        }
        let (tag, wire) = decode_key(&mut self.buf).map_err(|_| DecodeError)?;
        let field = match wire {
            WireType::Varint => {
                Field::Varint(decode_varint(&mut self.buf).map_err(|_| DecodeError)?)
            }
            WireType::LengthDelimited => {
                let len = decode_varint(&mut self.buf).map_err(|_| DecodeError)? as usize;
                if len > self.buf.len() {
                    return Err(DecodeError);
                }
                let (bytes, rest) = self.buf.split_at(len);
                self.buf = rest;
                Field::Bytes(bytes)
            }
            WireType::SixtyFourBit => {
                if self.buf.len() < 8 {
                    return Err(DecodeError);
                }
                Field::Fixed64(self.buf.get_f64_le())
            }
            WireType::StartGroup => Field::StartGroup,
            WireType::EndGroup => Field::EndGroup,
            WireType::ThirtyTwoBit => {
                if self.buf.len() < 4 {
                    return Err(DecodeError);
                }
                self.buf.advance(4);
                Field::Varint(0)
            }
        };
        Ok(Some((tag, field)))
    }
}

fn utf8(b: &[u8]) -> Result<String, DecodeError> {
    String::from_utf8(b.to_vec()).map_err(|_| DecodeError)
}

/// Decodes a cursor. `partition` supplies the database for keys (cursors don't carry one when
/// it is the default).
pub fn decode(bytes: &[u8]) -> Result<Position, DecodeError> {
    let mut position = Position::default();
    let mut r = Reader { buf: bytes };
    while let Some((tag, field)) = r.next()? {
        if let (1, Field::Bytes(postfix)) = (tag, field) {
            position = decode_postfix(postfix)?;
        }
    }
    Ok(position)
}

fn decode_postfix(bytes: &[u8]) -> Result<Position, DecodeError> {
    let mut p = Position::default();
    let mut r = Reader { buf: bytes };
    while let Some((tag, field)) = r.next()? {
        match (tag, field) {
            (1, Field::Bytes(iv)) => {
                let mut ir = Reader { buf: iv };
                let (mut name, mut value) = (String::new(), IndexValue::Null);
                while let Some((t, f)) = ir.next()? {
                    match (t, f) {
                        (1, Field::Bytes(n)) => name = utf8(n)?,
                        (2, Field::Bytes(v)) => value = decode_property_value(v)?,
                        _ => {}
                    }
                }
                p.index_values.push((name, value));
            }
            (2, Field::Bytes(k)) => p.key = Some(decode_reference(k)?),
            (3, Field::Varint(v)) => p.before = v != 0,
            (4, Field::Varint(v)) => p.before_ascending = v != 0,
            _ => {}
        }
    }
    Ok(p)
}

fn decode_reference(bytes: &[u8]) -> Result<Key, DecodeError> {
    let mut key = Key {
        partition: PartitionId::default(),
        path: Vec::new(),
    };
    let mut r = Reader { buf: bytes };
    while let Some((tag, field)) = r.next()? {
        match (tag, field) {
            (13, Field::Bytes(b)) => key.partition.project_id = utf8(b)?.into(),
            (20, Field::Bytes(b)) => key.partition.namespace_id = utf8(b)?.into(),
            (23, Field::Bytes(b)) => key.partition.database_id = utf8(b)?.into(),
            (14, Field::Bytes(path)) => {
                let mut pr = Reader { buf: path };
                let mut current: Option<PathElement> = None;
                while let Some((t, f)) = pr.next()? {
                    match (t, f) {
                        (1, Field::StartGroup) => current = Some(PathElement::new("", None)),
                        (1, Field::EndGroup) => key.path.push(current.take().ok_or(DecodeError)?),
                        (2, Field::Bytes(b)) => {
                            current.as_mut().ok_or(DecodeError)?.kind = utf8(b)?.into()
                        }
                        (3, Field::Varint(v)) => {
                            current.as_mut().ok_or(DecodeError)?.id = Some(IdOrName::Id(v as i64))
                        }
                        (4, Field::Bytes(b)) => {
                            current.as_mut().ok_or(DecodeError)?.id = Some(IdOrName::Name(utf8(b)?))
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(key)
}

fn decode_property_value(bytes: &[u8]) -> Result<IndexValue, DecodeError> {
    let mut r = Reader { buf: bytes };
    let mut value = IndexValue::Null;
    let (mut lat, mut lng) = (0.0, 0.0);
    let mut key: Option<Key> = None;
    let mut element: Option<PathElement> = None;
    while let Some((tag, field)) = r.next()? {
        match (tag, field) {
            (1, Field::Varint(v)) => value = IndexValue::Int(v as i64),
            (2, Field::Varint(v)) => value = IndexValue::Bool(v != 0),
            (3, Field::Bytes(b)) => value = IndexValue::Bytes(b.to_vec()),
            (4, Field::Fixed64(d)) => value = IndexValue::Double(d),
            (6, Field::Fixed64(d)) => lat = d,
            (7, Field::Fixed64(d)) => lng = d,
            (5, Field::EndGroup) => {
                value = IndexValue::Point {
                    latitude: lat,
                    longitude: lng,
                }
            }
            (12, Field::StartGroup) => {
                key = Some(Key {
                    partition: PartitionId::default(),
                    path: Vec::new(),
                })
            }
            (13, Field::Bytes(b)) => {
                key.as_mut().ok_or(DecodeError)?.partition.project_id = utf8(b)?.into()
            }
            (20, Field::Bytes(b)) => {
                key.as_mut().ok_or(DecodeError)?.partition.namespace_id = utf8(b)?.into()
            }
            (23, Field::Bytes(b)) => {
                key.as_mut().ok_or(DecodeError)?.partition.database_id = utf8(b)?.into()
            }
            (14, Field::StartGroup) => element = Some(PathElement::new("", None)),
            (15, Field::Bytes(b)) => element.as_mut().ok_or(DecodeError)?.kind = utf8(b)?.into(),
            (16, Field::Varint(v)) => {
                element.as_mut().ok_or(DecodeError)?.id = Some(IdOrName::Id(v as i64))
            }
            (17, Field::Bytes(b)) => {
                element.as_mut().ok_or(DecodeError)?.id = Some(IdOrName::Name(utf8(b)?))
            }
            (14, Field::EndGroup) => key
                .as_mut()
                .ok_or(DecodeError)?
                .path
                .push(element.take().ok_or(DecodeError)?),
            (12, Field::EndGroup) => value = IndexValue::Key(key.take().ok_or(DecodeError)?),
            _ => {}
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(project: &str, ns: &str, kind: &str, id: IdOrName) -> Key {
        Key {
            partition: PartitionId {
                project_id: project.into(),
                database_id: Default::default(),
                namespace_id: ns.into(),
            },
            path: vec![PathElement::new(kind, Some(id))],
        }
    }

    fn b64(bytes: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..4 {
                out.push(if i <= chunk.len() {
                    T[(n >> (18 - 6 * i) & 63) as usize] as char
                } else {
                    '='
                });
            }
        }
        out
    }

    #[test]
    fn matches_java_cursor_bytes() {
        assert_eq!(b64(&encode(&Position::default())), "CgA=");
        let a = Position {
            key: Some(key("p", "", "P", IdOrName::Name("a".into()))),
            ..Default::default()
        };
        assert_eq!(b64(&encode(&a)), "ChMSDWoBcHIICxIBUCIBYQwYACAA");
        let two = Position {
            key: Some(key("p", "", "P", IdOrName::Id(2))),
            ..Default::default()
        };
        assert_eq!(b64(&encode(&two)), "ChISDGoBcHIHCxIBUBgCDBgAIAA=");
        let ns = Position {
            key: Some(key("p", "ns", "P", IdOrName::Name("a".into()))),
            ..Default::default()
        };
        assert_eq!(b64(&encode(&ns)), "ChgSEmoBcHIICxIBUCIBYQyiAQJucxgAIAA=");
        let desc = Position {
            index_values: vec![("age".into(), IndexValue::Int(35))],
            key: Some(key("p", "", "P", IdOrName::Name("c".into()))),
            before: false,
            before_ascending: true,
        };
        assert_eq!(
            b64(&encode(&desc)),
            "Ch4KCQoDYWdlEgIIIxINagFwcggLEgFQIgFjDBgAIAE="
        );
        let distinct = Position {
            index_values: vec![("n".into(), IndexValue::Int(6))],
            ..Default::default()
        };
        assert_eq!(b64(&encode(&distinct)), "Cg0KBwoBbhICCAYYACAA");
        for p in [a, two, ns, desc, distinct] {
            assert_eq!(decode(&encode(&p)).unwrap(), p);
        }
    }
}
