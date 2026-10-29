//! The on-disk store (`--store_on_disk`): an [`oxide_engine::Snapshot`] saved as one file.
//!
//! Format: the 8-byte magic `OXDS\0\0\0\x01` followed by a protobuf message holding the id
//! counters and, per project, the entities (`google.datastore.v1.Entity`) with their versions in
//! creation order and the kinds in Java's iteration order. The Java emulator's `local_db.bin` (Java serialization) is not readable here;
//! data moves between the two through export/import.
//!
//! Saving writes a temporary file next to the target, syncs it and renames it over the target, so
//! a crash leaves either the old or the new store, never a torn one.

use std::io::Write;
use std::path::{Path, PathBuf};

use oxide_engine::{Form, ProjectSnapshot, Snapshot};
use oxide_proto::google::datastore::v1 as pb;
use prost::Message;

use crate::convert::{entity_to_proto, stored_entity_from_proto};

const MAGIC: &[u8; 8] = b"OXDS\0\0\0\x01";

#[derive(Clone, PartialEq, Message)]
struct StoreRecord {
    #[prost(int64, tag = "1")]
    id_counter: i64,
    #[prost(int64, tag = "2")]
    scattered_counter: i64,
    #[prost(message, repeated, tag = "3")]
    projects: Vec<ProjectRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct ProjectRecord {
    #[prost(string, tag = "1")]
    project_id: String,
    #[prost(message, repeated, tag = "2")]
    entities: Vec<EntityRecord>,
    /// Kinds in Java's iteration order.
    #[prost(string, repeated, tag = "3")]
    kinds: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
struct EntityRecord {
    #[prost(message, optional, tag = "1")]
    entity: Option<pb::Entity>,
    #[prost(int64, tag = "2")]
    version: i64,
    /// Written under the SCATTERED id policy (no hidden `__scatter__` property).
    #[prost(bool, tag = "3")]
    unscattered: bool,
    /// An export stripped its `__scatter__` and embedded-entity index data.
    #[prost(bool, tag = "4")]
    stripped: bool,
    /// The `EntityProto` of an entity written through the App Engine v3 API.
    #[prost(bytes = "vec", optional, tag = "5")]
    v3: Option<Vec<u8>>,
}

pub fn encode(snapshot: &Snapshot) -> Vec<u8> {
    let record = StoreRecord {
        id_counter: snapshot.id_counter,
        scattered_counter: snapshot.scattered_counter,
        projects: snapshot
            .projects
            .iter()
            .map(|p| ProjectRecord {
                project_id: p.project_id.clone(),
                entities: p
                    .entities
                    .iter()
                    .map(|(entity, version, form)| EntityRecord {
                        entity: Some(entity_to_proto(entity)),
                        version: *version,
                        unscattered: !form.scatter,
                        stripped: form.stripped,
                        v3: entity.v3.as_deref().map(<[u8]>::to_vec),
                    })
                    .collect(),
                kinds: p.kinds.clone(),
            })
            .collect(),
    };
    let mut out = MAGIC.to_vec();
    record.encode(&mut out).expect("Vec grows as needed");
    out
}

pub fn decode(bytes: &[u8]) -> Result<Snapshot, String> {
    let body = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or("not a datastore-oxide store (bad header)")?;
    let record = StoreRecord::decode(body).map_err(|e| e.to_string())?;
    let projects = record
        .projects
        .into_iter()
        .map(|p| {
            let entities = p
                .entities
                .into_iter()
                .map(|r| {
                    let mut entity = r
                        .entity
                        .as_ref()
                        .and_then(stored_entity_from_proto)
                        .filter(|e| e.key.is_some())
                        .ok_or("malformed entity")?;
                    entity.v3 = r.v3.map(Into::into);
                    let form = Form {
                        scatter: !r.unscattered,
                        stripped: r.stripped,
                    };
                    Ok((entity, r.version, form))
                })
                .collect::<Result<_, String>>()?;
            Ok(ProjectSnapshot {
                project_id: p.project_id,
                entities,
                kinds: p.kinds,
            })
        })
        .collect::<Result<_, String>>()?;
    Ok(Snapshot {
        id_counter: record.id_counter,
        scattered_counter: record.scattered_counter,
        projects,
    })
}

/// Atomically replaces `path` with `snapshot`.
pub fn save(path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
    let bytes = encode(snapshot);
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Make the rename itself durable.
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
        && let Ok(dir) = std::fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// The outcome of reading the store at startup.
pub enum Loaded {
    /// No store yet; it is created on the first save.
    Missing,
    Snapshot(Snapshot),
    /// The file could not be read and was moved aside to the given path, so the first save does
    /// not destroy it (Java starts empty in this case too, but later overwrites the file).
    Unreadable {
        error: String,
        moved_to: PathBuf,
    },
}

pub fn load(path: &Path) -> std::io::Result<Loaded> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Loaded::Missing),
        Err(e) => return Err(e),
    };
    match decode(&bytes) {
        Ok(snapshot) => Ok(Loaded::Snapshot(snapshot)),
        Err(error) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".unreadable-{stamp}"));
            let moved_to = path.with_file_name(name);
            std::fs::rename(path, &moved_to)?;
            Ok(Loaded::Unreadable { error, moved_to })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxide_model::{Entity, IdOrName, Key, PartitionId, PathElement, Value, ValueKind};

    #[test]
    fn round_trips() {
        let key = Key {
            partition: PartitionId {
                project_id: "p".into(),
                database_id: "d".into(),
                namespace_id: "n".into(),
            },
            path: vec![PathElement::new("K", Some(IdOrName::Name("a".into())))],
        };
        let mut entity = Entity::new(key.clone());
        for (name, kind) in [
            ("ts", ValueKind::Timestamp(-1)),
            ("d", ValueKind::Double(-0.0)),
            ("k", ValueKind::Key(key)),
            (
                "arr",
                ValueKind::Array(vec![
                    Value::new(ValueKind::Null),
                    Value::new(ValueKind::Blob(vec![0, 255])),
                ]),
            ),
        ] {
            entity.properties.insert(name.into(), Value::new(kind));
        }
        entity.properties.insert(
            "m".into(),
            Value {
                kind: ValueKind::String("x".into()),
                meaning: 15,
                exclude_from_indexes: true,
            },
        );
        let snapshot = Snapshot {
            id_counter: 7,
            scattered_counter: 3,
            projects: vec![ProjectSnapshot {
                project_id: "p".into(),
                entities: vec![(
                    entity,
                    42,
                    Form {
                        scatter: true,
                        stripped: false,
                    },
                )],
                kinds: vec!["K".into()],
            }],
        };
        assert_eq!(decode(&encode(&snapshot)).unwrap(), snapshot);
        assert!(decode(b"\xac\xed\x00\x05").is_err());
    }
}
