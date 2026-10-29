//! Lenient parsing of App Engine v3 requests, the way the Java emulator's proto runtime reads
//! them: unknown fields and known fields sent with the wrong wire type are skipped, malformed input
//! is an error, and every `required` field must be present, recursively.

use std::sync::OnceLock;

use prost::Message;
use prost_reflect::{Cardinality, DescriptorPool, FieldDescriptor, Kind, MessageDescriptor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    Malformed,
    MissingRequired,
}

pub const MALFORMED: &str = "Error parsing protocol message";
pub const MISSING_REQUIRED: &str = "Message was missing required fields.  (Lite runtime could not determine which fields were missing).";

/// The descriptors of every generated message, adapted like the generated v3 types.
pub fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let mut fds = prost_types::FileDescriptorSet::decode(oxide_proto::FILE_DESCRIPTOR_SET)
            .expect("descriptor set");
        oxide_proto::as_proto1(&mut fds);
        DescriptorPool::from_file_descriptor_set(fds).expect("descriptor pool")
    })
}

/// The descriptor of a message by its full name (e.g. `apphosting_datastore_v3.GetRequest`).
pub fn descriptor(name: &str) -> MessageDescriptor {
    pool()
        .get_message_by_name(name)
        .unwrap_or_else(|| panic!("unknown message {name}"))
}

/// Parses `bytes` as `desc` leniently and decodes what is left as `M`.
pub fn decode<M: Message + Default>(
    desc: &MessageDescriptor,
    bytes: &[u8],
) -> Result<M, WireError> {
    let mut reader = Reader { buf: bytes };
    let mut missing = false;
    let clean = reader.message(desc, None, &mut missing)?;
    if missing {
        return Err(WireError::MissingRequired);
    }
    M::decode(clean.as_slice()).map_err(|_| WireError::Malformed)
}

const VARINT: u8 = 0;
const FIXED64: u8 = 1;
const DELIMITED: u8 = 2;
const START_GROUP: u8 = 3;
const END_GROUP: u8 = 4;
const FIXED32: u8 = 5;

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, WireError> {
        prost::encoding::decode_varint(&mut self.buf).map_err(|_| WireError::Malformed)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if n > self.buf.len() {
            return Err(WireError::Malformed);
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    /// Reads one tag: `(field number, wire type)`.
    fn tag(&mut self) -> Result<(u32, u8), WireError> {
        let key = self.varint()?;
        let number = key >> 3;
        if number == 0 || number > u64::from(u32::MAX >> 3) {
            return Err(WireError::Malformed);
        }
        Ok((number as u32, (key & 7) as u8))
    }

    /// Skips a field's value of wire type `wire`, returning its bytes.
    fn skip(&mut self, number: u32, wire: u8) -> Result<&'a [u8], WireError> {
        let start = self.buf;
        match wire {
            VARINT => {
                self.varint()?;
            }
            FIXED64 => {
                self.take(8)?;
            }
            DELIMITED => {
                let len = self.varint()?;
                self.take(usize::try_from(len).map_err(|_| WireError::Malformed)?)?;
            }
            START_GROUP => loop {
                let (n, w) = self.tag()?;
                if w == END_GROUP {
                    if n != number {
                        return Err(WireError::Malformed);
                    }
                    break;
                }
                self.skip(n, w)?;
            },
            FIXED32 => {
                self.take(4)?;
            }
            _ => return Err(WireError::Malformed),
        }
        Ok(&start[..start.len() - self.buf.len()])
    }

    /// Reads the fields of a message (up to the end group of `group`, if any), keeping only known
    /// fields with the expected wire type, and returns them re-encoded.
    fn message(
        &mut self,
        desc: &MessageDescriptor,
        group: Option<u32>,
        missing: &mut bool,
    ) -> Result<Vec<u8>, WireError> {
        let mut out = Vec::new();
        let mut seen = Vec::new();
        loop {
            if self.buf.is_empty() {
                if group.is_some() {
                    return Err(WireError::Malformed);
                }
                break;
            }
            let (number, wire) = self.tag()?;
            if wire == END_GROUP {
                if group == Some(number) {
                    break;
                }
                return Err(WireError::Malformed);
            }
            let field = desc.get_field(number);
            match field.filter(|f| accepts(f, wire)) {
                Some(field) if wire == START_GROUP || is_message(&field) => {
                    let Kind::Message(inner) = field.kind() else {
                        unreachable!("message field")
                    };
                    let body = if wire == START_GROUP {
                        self.message(&inner, Some(number), missing)?
                    } else {
                        let len = self.varint()?;
                        let bytes =
                            self.take(usize::try_from(len).map_err(|_| WireError::Malformed)?)?;
                        Reader { buf: bytes }.message(&inner, None, missing)?
                    };
                    seen.push(number);
                    put_tag(&mut out, number, wire);
                    if wire == DELIMITED {
                        prost::encoding::encode_varint(body.len() as u64, &mut out);
                    }
                    out.extend_from_slice(&body);
                    if wire == START_GROUP {
                        put_tag(&mut out, number, END_GROUP);
                    }
                }
                Some(_) => {
                    seen.push(number);
                    put_tag(&mut out, number, wire);
                    out.extend_from_slice(self.skip(number, wire)?);
                }
                None => {
                    self.skip(number, wire)?;
                }
            }
        }
        if desc
            .fields()
            .any(|f| f.cardinality() == Cardinality::Required && !seen.contains(&f.number()))
        {
            *missing = true;
        }
        Ok(out)
    }
}

fn put_tag(out: &mut Vec<u8>, number: u32, wire: u8) {
    prost::encoding::encode_varint((u64::from(number) << 3) | u64::from(wire), out);
}

fn is_message(field: &FieldDescriptor) -> bool {
    matches!(field.kind(), Kind::Message(_))
}

/// Whether a known field may arrive with wire type `wire` (repeated scalars may be packed).
fn accepts(field: &FieldDescriptor, wire: u8) -> bool {
    let expected = match field.kind() {
        Kind::Double | Kind::Fixed64 | Kind::Sfixed64 => FIXED64,
        Kind::Float | Kind::Fixed32 | Kind::Sfixed32 => FIXED32,
        Kind::String | Kind::Bytes => DELIMITED,
        Kind::Message(_) if field.is_group() => START_GROUP,
        Kind::Message(_) => DELIMITED,
        _ => VARINT,
    };
    wire == expected
        || (wire == DELIMITED
            && field.is_list()
            && expected != DELIMITED
            && expected != START_GROUP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxide_proto::apphosting_datastore_v3 as v3;

    fn get(bytes: &[u8]) -> Result<v3::GetRequest, WireError> {
        decode(&descriptor("apphosting_datastore_v3.GetRequest"), bytes)
    }

    #[test]
    fn skips_unknown_and_mistyped_fields() {
        assert_eq!(get(&[0x0d, 0, 0, 0, 0]), Ok(v3::GetRequest::default()));
        assert_eq!(get(&[0x08, 0x01]), Ok(v3::GetRequest::default()));
        assert_eq!(get(&[0xf8, 0x01, 0x05]), Ok(v3::GetRequest::default()));
    }

    #[test]
    fn rejects_malformed_input() {
        for bytes in [
            &[0x0a, 0x05][..],
            &[0x08],
            &[0x0b],
            &[0x0c],
            &[0x0e],
            &[0x0f],
            &[0x00, 0x01],
        ] {
            assert_eq!(get(bytes), Err(WireError::Malformed), "{bytes:02x?}");
        }
    }

    #[test]
    fn requires_required_fields() {
        // A key (field 1) without app or path.
        assert_eq!(get(&[0x0a, 0x00]), Err(WireError::MissingRequired));
        // app "p" and a path with one element of type "K".
        let key = [0x6a, 0x01, b'p', 0x72, 0x05, 0x0b, 0x12, 0x01, b'K', 0x0c];
        let mut bytes = vec![0x0a, key.len() as u8];
        bytes.extend(key);
        let r = get(&bytes).unwrap();
        assert_eq!(r.key[0].path.element[0].r#type.as_ref(), b"K");
    }
}
