//! Datastore data model: keys, values and entities, independent of any wire format.
//!
//! Values compare and hash doubles by bit pattern (so `NaN == NaN` and `0.0 != -0.0`), matching
//! how the Java emulator compares stored entity protos.

mod properties;

use std::fmt;
use std::hash::{Hash, Hasher};

pub use properties::Properties;

/// Identifies a partition: project, database (`""` is the default database) and namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartitionId {
    pub project_id: String,
    pub database_id: String,
    pub namespace_id: String,
}

/// The id or name of a key path element. Ids sort before names.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IdOrName {
    Id(i64),
    Name(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PathElement {
    pub kind: String,
    /// `None` for an incomplete element (id to be allocated).
    pub id: Option<IdOrName>,
}

impl PathElement {
    pub fn new(kind: impl Into<String>, id: Option<IdOrName>) -> Self {
        Self {
            kind: kind.into(),
            id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub partition: PartitionId,
    pub path: Vec<PathElement>,
}

impl Key {
    /// True when every path element has an id or name.
    pub fn is_complete(&self) -> bool {
        !self.path.is_empty() && self.path.iter().all(|e| e.id.is_some())
    }

    /// The kind of the last path element.
    pub fn kind(&self) -> &str {
        self.path.last().map_or("", |e| e.kind.as_str())
    }

    /// The root key of this key's entity group.
    pub fn entity_group(&self) -> Key {
        Key {
            partition: self.partition.clone(),
            path: self.path[..1.min(self.path.len())].to_vec(),
        }
    }

    /// The path in the Java emulator's error-message format: `[K: 1, C: a, D: ]`.
    pub fn display_path(&self) -> String {
        let elements: Vec<String> = self
            .path
            .iter()
            .map(|e| match &e.id {
                Some(IdOrName::Id(id)) => format!("{}: {id}", e.kind),
                Some(IdOrName::Name(name)) => format!("{}: {name}", e.kind),
                None => format!("{}: ", e.kind),
            })
            .collect();
        format!("[{}]", elements.join(", "))
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_path())
    }
}

#[derive(Clone, Debug)]
pub enum ValueKind {
    Null,
    Boolean(bool),
    Integer(i64),
    Double(f64),
    /// Microseconds since the Unix epoch.
    Timestamp(i64),
    Key(Box<Key>),
    String(String),
    Blob(Vec<u8>),
    GeoPoint {
        latitude: f64,
        longitude: f64,
    },
    Entity(Box<Entity>),
    Array(Vec<Value>),
}

impl PartialEq for ValueKind {
    fn eq(&self, other: &Self) -> bool {
        use ValueKind::*;
        match (self, other) {
            (Null, Null) => true,
            (Boolean(a), Boolean(b)) => a == b,
            (Integer(a), Integer(b)) => a == b,
            (Double(a), Double(b)) => a.to_bits() == b.to_bits(),
            (Timestamp(a), Timestamp(b)) => a == b,
            (Key(a), Key(b)) => a == b,
            (String(a), String(b)) => a == b,
            (Blob(a), Blob(b)) => a == b,
            (
                GeoPoint {
                    latitude: a,
                    longitude: b,
                },
                GeoPoint {
                    latitude: c,
                    longitude: d,
                },
            ) => a.to_bits() == c.to_bits() && b.to_bits() == d.to_bits(),
            (Entity(a), Entity(b)) => a == b,
            (Array(a), Array(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for ValueKind {}

impl Hash for ValueKind {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            ValueKind::Null => {}
            ValueKind::Boolean(b) => b.hash(state),
            ValueKind::Integer(i) | ValueKind::Timestamp(i) => i.hash(state),
            ValueKind::Double(d) => d.to_bits().hash(state),
            ValueKind::Key(k) => k.hash(state),
            ValueKind::String(s) => s.hash(state),
            ValueKind::Blob(b) => b.hash(state),
            ValueKind::GeoPoint {
                latitude,
                longitude,
            } => {
                latitude.to_bits().hash(state);
                longitude.to_bits().hash(state);
            }
            ValueKind::Entity(e) => e.hash(state),
            ValueKind::Array(a) => a.hash(state),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Value {
    pub kind: ValueKind,
    /// Legacy App Engine meaning (0 = none).
    pub meaning: i32,
    pub exclude_from_indexes: bool,
}

impl Value {
    pub fn new(kind: ValueKind) -> Self {
        Self {
            kind,
            meaning: 0,
            exclude_from_indexes: false,
        }
    }
}

impl From<ValueKind> for Value {
    fn from(kind: ValueKind) -> Self {
        Value::new(kind)
    }
}

/// An entity: a key (absent for some embedded entities) and properties by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Entity {
    pub key: Option<Key>,
    pub properties: Properties,
    /// For entities written through the App Engine v3 API: the stored `EntityProto` (with project
    /// ids for app ids), which v3 reads return as is. `properties` is its v1 view. `None` for
    /// entities written through v1.
    pub v3: Option<std::sync::Arc<[u8]>>,
}

impl Entity {
    pub fn new(key: Key) -> Self {
        Self {
            key: Some(key),
            properties: Properties::new(),
            v3: None,
        }
    }
}

/// Java's `String.hashCode()`: `s[0]*31^(n-1) + ... + s[n-1]` over UTF-16 code units.
pub fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, unit| h.wrapping_mul(31).wrapping_add(unit as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &[(&str, Option<IdOrName>)]) -> Key {
        Key {
            partition: PartitionId::default(),
            path: path
                .iter()
                .map(|(k, id)| PathElement::new(*k, id.clone()))
                .collect(),
        }
    }

    #[test]
    fn display_path_matches_java() {
        let k = key(&[
            ("K", None),
            ("C", Some(IdOrName::Name("a".into()))),
            ("D", Some(IdOrName::Id(5))),
        ]);
        assert_eq!(k.display_path(), "[K: , C: a, D: 5]");
        assert!(!k.is_complete());
        assert_eq!(k.kind(), "D");
    }

    #[test]
    fn key_ordering_ids_before_names_parents_first() {
        let parent = key(&[("K", Some(IdOrName::Id(5)))]);
        let child = key(&[("K", Some(IdOrName::Id(5))), ("C", Some(IdOrName::Id(1)))]);
        let named = key(&[("K", Some(IdOrName::Name("a".into())))]);
        let bigger = key(&[("K", Some(IdOrName::Id(6)))]);
        let mut keys = vec![named.clone(), child.clone(), bigger.clone(), parent.clone()];
        keys.sort();
        assert_eq!(keys, vec![parent, child, bigger, named]);
    }

    #[test]
    fn doubles_compare_by_bits() {
        assert_eq!(ValueKind::Double(f64::NAN), ValueKind::Double(f64::NAN));
        assert_ne!(ValueKind::Double(0.0), ValueKind::Double(-0.0));
    }

    #[test]
    fn java_hash() {
        assert_eq!(java_string_hash(""), 0);
        assert_eq!(java_string_hash("zz"), 3904);
        assert_eq!(java_string_hash("hello"), 99162322);
        assert_eq!(java_string_hash("é𝄞"), {
            // 'é' = 0xE9, '𝄞' = surrogates 0xD834 0xDD1E
            let units = [0xE9i32, 0xD834, 0xDD1E];
            units
                .iter()
                .fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(*u))
        });
    }
}
