//! Conversion between `google.datastore.v1` protos and the model, with the Java emulator's request
//! validation and error messages (recorded in `tests/conformance/java/validation_*.golden`,
//! `meanings_matrix.golden` and `project_ids.golden`).

use oxide_model::{Entity, IdOrName, Key, PartitionId, PathElement, Value, ValueKind};
use oxide_proto::google::datastore::v1 as pb;
use pb::value::ValueType;
use tonic::Status;

const MAX_STRING_BYTES: usize = 1500;
const MAX_PATH_ELEMENTS: usize = 100;
const MAX_NAMESPACE_CHARS: usize = 100;

/// Valid timestamps: 0001-01-01T00:00:00Z to 9999-12-31T23:59:59.999999Z, in microseconds.
const MIN_TIMESTAMP_MICROS: i64 = -62_135_596_800_000_000;
const MAX_TIMESTAMP_MICROS: i64 = 253_402_300_799_999_999;

pub(crate) fn invalid(message: impl Into<String>) -> Status {
    Status::invalid_argument(message)
}

/// The project and database a request is addressed to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub project_id: String,
    pub database_id: String,
    /// Production values the Java emulator rejects (vectors) are accepted (superset).
    pub superset: bool,
}

impl Scope {
    /// Validates the request-level ids. An empty project id fails like Java (UNKNOWN, no message).
    pub fn new(project_id: &str, database_id: &str) -> Result<Self, Status> {
        if project_id.is_empty() {
            return Err(Status::unknown(""));
        }
        if !is_valid_project_id(project_id) {
            return Err(invalid(format!(
                "\"{project_id}\" is an invalid project id."
            )));
        }
        if !database_id.is_empty()
            && !database_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(invalid(format!(
                "\"{database_id}\" is an invalid database id."
            )));
        }
        Ok(Self {
            project_id: project_id.into(),
            database_id: database_id.into(),
            superset: false,
        })
    }

    /// `<unknown!>~project#database`, as in Java's "mismatched databases" message.
    fn describe(project_id: &str, database_id: &str) -> String {
        if database_id.is_empty() {
            format!("<unknown!>~{project_id}")
        } else {
            format!("<unknown!>~{project_id}#{database_id}")
        }
    }
}

/// Lowercase letters, digits and hyphens, starting with a letter or digit, at most 100 chars.
fn is_valid_project_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() <= 100
        && bytes
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// What a key is used for; this decides which checks apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyUse {
    /// Lookups: complete; reserved names allowed.
    Read,
    /// Writes and `reserveIds`: reserved names rejected; `allow_incomplete` for insert/upsert.
    Write { allow_incomplete: bool },
    /// `allocateIds`: the last element must be incomplete.
    Allocate,
    /// A `key_value` property: complete, any partition, reserved names allowed.
    Value,
    /// The key of an embedded entity value: may be incomplete.
    Embedded,
}

impl KeyUse {
    fn checks_reserved(self) -> bool {
        matches!(self, KeyUse::Write { .. } | KeyUse::Allocate)
    }

    fn checks_partition(self) -> bool {
        matches!(self, KeyUse::Read | KeyUse::Write { .. } | KeyUse::Allocate)
    }
}

/// `__.*__` names are reserved.
fn is_reserved(name: &str) -> bool {
    name.len() >= 4 && name.starts_with("__") && name.ends_with("__")
}

pub fn key_from_proto(key: &pb::Key, scope: &Scope, key_use: KeyUse) -> Result<Key, Status> {
    if key.path.is_empty() {
        return Err(invalid("Key path is empty."));
    }
    if key.path.len() > MAX_PATH_ELEMENTS {
        return Err(invalid("Key path is too long. Cannot exceed 100 elements."));
    }
    let mut path = Vec::with_capacity(key.path.len());
    for element in &key.path {
        if element.kind.is_empty() {
            return Err(invalid("The kind is the empty string."));
        }
        if element.kind.len() > MAX_STRING_BYTES {
            return Err(invalid("The kind is longer than 1500 bytes."));
        }
        if key_use.checks_reserved() && is_reserved(&element.kind) {
            return Err(invalid(format!(
                "The kind \"{}\" is reserved.",
                element.kind
            )));
        }
        let id = match &element.id_type {
            None => None,
            Some(pb::key::path_element::IdType::Id(0)) => {
                return Err(invalid("Key path id is invalid. Must not be zero."));
            }
            Some(pb::key::path_element::IdType::Id(id)) => Some(IdOrName::Id(*id)),
            Some(pb::key::path_element::IdType::Name(name)) => {
                if name.is_empty() {
                    return Err(invalid("The key path element name is the empty string."));
                }
                if name.len() > MAX_STRING_BYTES {
                    return Err(invalid(
                        "The key path element name is longer than 1500 bytes.",
                    ));
                }
                if key_use.checks_reserved() && is_reserved(name) {
                    return Err(invalid(format!(
                        "The key path element name \"{name}\" is reserved."
                    )));
                }
                Some(IdOrName::Name(name.clone()))
            }
        };
        path.push(PathElement {
            kind: element.kind.clone(),
            id,
        });
    }

    let partition = partition_from_proto(key.partition_id.as_ref(), scope, key_use)?;
    let key = Key { partition, path };

    let (ancestors, last) = key.path.split_at(key.path.len() - 1);
    let incomplete_ancestor = ancestors.iter().any(|e| e.id.is_none());
    let last_incomplete = last[0].id.is_none();
    let incomplete_allowed = match key_use {
        KeyUse::Write { allow_incomplete } => allow_incomplete,
        KeyUse::Allocate | KeyUse::Embedded => true,
        KeyUse::Read | KeyUse::Value => false,
    };
    if incomplete_ancestor || (last_incomplete && !incomplete_allowed) {
        return Err(invalid(format!(
            "Key path element must not be incomplete: {}",
            key.display_path()
        )));
    }
    if key_use == KeyUse::Allocate && !last_incomplete {
        return Err(invalid(format!(
            "Key path element must not be complete: {}",
            key.display_path()
        )));
    }
    Ok(key)
}

fn partition_from_proto(
    partition: Option<&pb::PartitionId>,
    scope: &Scope,
    key_use: KeyUse,
) -> Result<PartitionId, Status> {
    let default = pb::PartitionId::default();
    let p = partition.unwrap_or(&default);
    let namespace = &p.namespace_id;
    if namespace.chars().count() > MAX_NAMESPACE_CHARS
        || !namespace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid(format!(
            "Illegal string \"{namespace}\" in namespace id."
        )));
    }
    if key_use.checks_reserved() && is_reserved(namespace) {
        return Err(invalid(format!(
            "The namespace id \"{namespace}\" is reserved."
        )));
    }
    // A partition that names neither project nor database belongs to the request's database.
    let (project_id, database_id) = if p.project_id.is_empty() && p.database_id.is_empty() {
        (scope.project_id.clone(), scope.database_id.clone())
    } else {
        (p.project_id.clone(), p.database_id.clone())
    };
    if key_use.checks_partition()
        && (project_id != scope.project_id || database_id != scope.database_id)
    {
        return Err(invalid(format!(
            "mismatched databases within request: {} vs. {}",
            Scope::describe(&scope.project_id, &scope.database_id),
            Scope::describe(&project_id, &database_id)
        )));
    }
    let project_id = if project_id.is_empty() {
        scope.project_id.clone()
    } else {
        project_id
    };
    Ok(PartitionId {
        project_id,
        database_id,
        namespace_id: namespace.clone(),
    })
}

pub fn entity_from_proto(
    entity: &pb::Entity,
    scope: &Scope,
    key_use: KeyUse,
) -> Result<Entity, Status> {
    let key = entity
        .key
        .as_ref()
        .map(|k| key_from_proto(k, scope, key_use))
        .transpose()?;
    // Validate in name order so the reported error is deterministic.
    let mut names: Vec<&String> = entity.properties.keys().collect();
    names.sort();
    let mut out = Entity {
        key,
        properties: oxide_model::Properties::with_capacity(names.len()),
        ..Default::default()
    };
    for name in names {
        if name.is_empty() {
            return Err(invalid("The property.name is the empty string."));
        }
        if name.len() > MAX_STRING_BYTES {
            return Err(invalid("The property.name is longer than 1500 bytes."));
        }
        if is_reserved(name) {
            return Err(invalid(format!(
                "The property.name \"{name}\" is reserved."
            )));
        }
        let value = value_from_proto(name, &entity.properties[name], scope)?;
        out.properties.insert(name.clone(), value);
    }
    Ok(out)
}

/// The value field a meaning requires, or the error for a meaning that is never valid.
fn meaning_field(meaning: i32) -> Result<Option<&'static str>, String> {
    Ok(Some(match meaning {
        0 => return Ok(None),
        1..=6 | 8 | 10..=12 | 15 | 17 => "string",
        7 | 13 => "integer",
        16 | 19 | 22 => "blob",
        20 => "entity",
        23 => "geo_point",
        18 => return Err("Projection property cannot be used.".into()),
        m => return Err(format!("Unknown value meaning {m}.")),
    }))
}

fn value_field_name(v: &ValueType) -> &'static str {
    match v {
        ValueType::NullValue(_) => "null",
        ValueType::BooleanValue(_) => "boolean",
        ValueType::IntegerValue(_) => "integer",
        ValueType::DoubleValue(_) => "double",
        ValueType::TimestampValue(_) => "timestamp",
        ValueType::KeyValue(_) => "key",
        ValueType::StringValue(_) => "string",
        ValueType::BlobValue(_) => "blob",
        ValueType::GeoPointValue(_) => "geo_point",
        ValueType::EntityValue(_) => "entity",
        ValueType::ArrayValue(_) => "array",
    }
}

/// Validates and converts a single value (as used in query filters).
pub(crate) fn single_value_from_proto(
    name: &str,
    value: &pb::Value,
    scope: &Scope,
) -> Result<Value, Status> {
    value_from_proto(name, value, scope)
}

/// The meaning of a vector value (an array of doubles).
pub const VECTOR_MEANING: i32 = 31;
/// Production's limit on vector dimensions.
pub const MAX_VECTOR_DIMENSIONS: usize = 2048;

/// A vector value (superset): an array of at most 2048 doubles, never indexed.
fn vector_from_proto(name: &str, array: &pb::ArrayValue) -> Result<Value, Status> {
    if array.values.is_empty() || array.values.len() > MAX_VECTOR_DIMENSIONS {
        return Err(invalid(format!(
            "The vector value \"{name}\" must have between 1 and {MAX_VECTOR_DIMENSIONS} dimensions."
        )));
    }
    let values = array
        .values
        .iter()
        .map(|v| match v.value_type {
            Some(ValueType::DoubleValue(d)) => Ok(Value {
                kind: ValueKind::Double(d),
                meaning: 0,
                exclude_from_indexes: true,
            }),
            _ => Err(invalid(format!(
                "The vector value \"{name}\" must contain only double values."
            ))),
        })
        .collect::<Result<_, _>>()?;
    Ok(Value {
        kind: ValueKind::Array(values),
        meaning: VECTOR_MEANING,
        exclude_from_indexes: true,
    })
}

/// The components of a vector value.
pub fn vector_components(value: &Value) -> Option<Vec<f64>> {
    match &value.kind {
        ValueKind::Array(values) if value.meaning == VECTOR_MEANING => values
            .iter()
            .map(|v| match v.kind {
                ValueKind::Double(d) => Some(d),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Array elements are validated under the array's property name.
fn value_from_proto(name: &str, value: &pb::Value, scope: &Scope) -> Result<Value, Status> {
    let Some(value_type) = &value.value_type else {
        return Err(invalid(format!(
            "The value \"{name}\" does not contain a value."
        )));
    };
    let indexed = !value.exclude_from_indexes;
    let mut meaning = value.meaning;

    if let ValueType::ArrayValue(array) = value_type {
        if scope.superset && meaning == VECTOR_MEANING {
            return vector_from_proto(name, array);
        }
        if value.exclude_from_indexes {
            return Err(invalid(
                "Exclude from indexes cannot be set on a list value",
            ));
        }
        if meaning != 0 {
            return Err(invalid(
                "A Value containing a list_value cannot specify a meaning.",
            ));
        }
        let values = array
            .values
            .iter()
            .map(|v| value_from_proto(name, v, scope))
            .collect::<Result<_, _>>()?;
        return Ok(Value {
            kind: ValueKind::Array(values),
            meaning: 0,
            exclude_from_indexes: false,
        });
    }
    if let Some(field) = meaning_field(meaning).map_err(invalid)? {
        if field != value_field_name(value_type) {
            return Err(invalid(format!(
                "Value \"{name}\" has a meaning {meaning} which does not match {field}_value field."
            )));
        }
        match (meaning, value_type) {
            (15 | 22, _) if indexed => {
                return Err(invalid(format!("Indexed value has meaning {meaning}.")));
            }
            (19, _) => return Err(invalid("Unknown meaning 19.")),
            (7, ValueType::IntegerValue(micros))
                if (MIN_TIMESTAMP_MICROS..=MAX_TIMESTAMP_MICROS).contains(micros) =>
            {
                return Err(invalid(
                    "A timestamp in range [0001-01-01T00:00:00Z, 9999-12-31T23:59:59.999999Z] must be stored as a timestamp value.",
                ));
            }
            (20, ValueType::EntityValue(e)) if !e.properties.contains_key("email") => {
                return Err(invalid(
                    "The user entity is missing required property \"email\".",
                ));
            }
            // An indexed blob is already a byte string; Java drops the redundant meaning.
            (16, ValueType::BlobValue(_)) if indexed => meaning = 0,
            _ => {}
        }
    }

    let kind = match value_type {
        ValueType::NullValue(_) => ValueKind::Null,
        ValueType::BooleanValue(b) => ValueKind::Boolean(*b),
        ValueType::IntegerValue(i) => ValueKind::Integer(*i),
        // Java stores -0.0 as 0.0.
        ValueType::DoubleValue(d) => ValueKind::Double(if *d == 0.0 { 0.0 } else { *d }),
        ValueType::TimestampValue(ts) => ValueKind::Timestamp(timestamp_to_micros(ts)?),
        ValueType::KeyValue(k) => ValueKind::Key(key_from_proto(k, scope, KeyUse::Value)?),
        ValueType::StringValue(s) => {
            if indexed && s.len() > MAX_STRING_BYTES {
                return Err(too_long(name));
            }
            ValueKind::String(s.clone())
        }
        ValueType::BlobValue(b) => {
            if indexed && b.len() > MAX_STRING_BYTES {
                return Err(too_long(name));
            }
            ValueKind::Blob(b.to_vec())
        }
        ValueType::GeoPointValue(g) => {
            check_coordinate("latitude", g.latitude, 90.0)?;
            check_coordinate("longitude", g.longitude, 180.0)?;
            ValueKind::GeoPoint {
                latitude: g.latitude,
                longitude: g.longitude,
            }
        }
        ValueType::EntityValue(e) => {
            ValueKind::Entity(Box::new(entity_from_proto(e, scope, KeyUse::Embedded)?))
        }
        ValueType::ArrayValue(_) => unreachable!("handled above"),
    };
    Ok(Value {
        kind,
        meaning,
        exclude_from_indexes: value.exclude_from_indexes,
    })
}

fn too_long(name: &str) -> Status {
    invalid(format!(
        "The value of property \"{name}\" is longer than 1500 bytes."
    ))
}

fn check_coordinate(axis: &str, v: f64, limit: f64) -> Result<(), Status> {
    if v.is_nan() {
        return Err(invalid(format!("Geo point {axis} is not a number.")));
    }
    if !(-limit..=limit).contains(&v) {
        return Err(invalid(format!(
            "Geo point {axis} outside permitted range -{limit:.1} to {limit:.1}."
        )));
    }
    Ok(())
}

pub(crate) fn timestamp_to_micros(ts: &prost_types::Timestamp) -> Result<i64, Status> {
    let micros = ts
        .seconds
        .checked_mul(1_000_000)
        .and_then(|s| s.checked_add(i64::from(ts.nanos.div_euclid(1000))))
        .filter(|m| (MIN_TIMESTAMP_MICROS..=MAX_TIMESTAMP_MICROS).contains(m));
    micros.ok_or_else(|| invalid("Payload isn't valid for request."))
}

pub fn micros_to_timestamp(micros: i64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: micros.div_euclid(1_000_000),
        nanos: (micros.rem_euclid(1_000_000) * 1000) as i32,
    }
}

pub fn key_to_proto(key: &Key) -> pb::Key {
    pb::Key {
        partition_id: Some(pb::PartitionId {
            project_id: key.partition.project_id.clone(),
            database_id: key.partition.database_id.clone(),
            namespace_id: key.partition.namespace_id.clone(),
        }),
        path: key
            .path
            .iter()
            .map(|e| pb::key::PathElement {
                kind: e.kind.clone(),
                id_type: e.id.as_ref().map(|id| match id {
                    IdOrName::Id(id) => pb::key::path_element::IdType::Id(*id),
                    IdOrName::Name(name) => pb::key::path_element::IdType::Name(name.clone()),
                }),
            })
            .collect(),
    }
}

pub fn entity_to_proto(entity: &Entity) -> pb::Entity {
    pb::Entity {
        key: entity.key.as_ref().map(key_to_proto),
        properties: entity
            .properties
            .iter()
            .map(|(k, v)| (k.to_string(), value_to_proto(v)))
            .collect(),
    }
}

pub(crate) fn value_to_proto(value: &Value) -> pb::Value {
    let value_type = match &value.kind {
        ValueKind::Null => ValueType::NullValue(0),
        ValueKind::Boolean(b) => ValueType::BooleanValue(*b),
        ValueKind::Integer(i) => ValueType::IntegerValue(*i),
        ValueKind::Double(d) => ValueType::DoubleValue(*d),
        ValueKind::Timestamp(t) => ValueType::TimestampValue(micros_to_timestamp(*t)),
        ValueKind::Key(k) => ValueType::KeyValue(key_to_proto(k)),
        ValueKind::String(s) => ValueType::StringValue(s.clone()),
        ValueKind::Blob(b) => ValueType::BlobValue(b.clone().into()),
        ValueKind::GeoPoint {
            latitude,
            longitude,
        } => ValueType::GeoPointValue(oxide_proto::google::r#type::LatLng {
            latitude: *latitude,
            longitude: *longitude,
        }),
        ValueKind::Entity(e) => ValueType::EntityValue(entity_to_proto(e)),
        ValueKind::Array(values) => ValueType::ArrayValue(pb::ArrayValue {
            values: values
                .iter()
                .map(|v| {
                    let mut v = value_to_proto(v);
                    // A vector's components carry no flags of their own.
                    if value.meaning == VECTOR_MEANING {
                        v.exclude_from_indexes = false;
                    }
                    v
                })
                .collect(),
        }),
    };
    pb::Value {
        value_type: Some(value_type),
        meaning: value.meaning,
        exclude_from_indexes: value.exclude_from_indexes,
    }
}

/// The inverse of [`entity_to_proto`] for data this emulator wrote itself (saved stores), without
/// request validation. `None` if the entity is malformed.
pub fn stored_entity_from_proto(entity: &pb::Entity) -> Option<Entity> {
    Some(Entity {
        key: match &entity.key {
            Some(k) => Some(stored_key_from_proto(k)?),
            None => None,
        },
        properties: entity
            .properties
            .iter()
            .map(|(k, v)| Some((k.clone(), stored_value_from_proto(v)?)))
            .collect::<Option<_>>()?,
        v3: None,
    })
}

pub fn stored_key_from_proto(key: &pb::Key) -> Option<Key> {
    let partition = key.partition_id.clone().unwrap_or_default();
    Some(Key {
        partition: PartitionId {
            project_id: partition.project_id,
            database_id: partition.database_id,
            namespace_id: partition.namespace_id,
        },
        path: key
            .path
            .iter()
            .map(|e| PathElement {
                kind: e.kind.clone(),
                id: e.id_type.as_ref().map(|id| match id {
                    pb::key::path_element::IdType::Id(id) => IdOrName::Id(*id),
                    pb::key::path_element::IdType::Name(n) => IdOrName::Name(n.clone()),
                }),
            })
            .collect(),
    })
}

fn stored_value_from_proto(value: &pb::Value) -> Option<Value> {
    let kind = match value.value_type.as_ref()? {
        ValueType::NullValue(_) => ValueKind::Null,
        ValueType::BooleanValue(b) => ValueKind::Boolean(*b),
        ValueType::IntegerValue(i) => ValueKind::Integer(*i),
        ValueType::DoubleValue(d) => ValueKind::Double(*d),
        ValueType::TimestampValue(t) => ValueKind::Timestamp(
            t.seconds
                .checked_mul(1_000_000)?
                .checked_add(i64::from(t.nanos / 1000))?,
        ),
        ValueType::KeyValue(k) => ValueKind::Key(stored_key_from_proto(k)?),
        ValueType::StringValue(s) => ValueKind::String(s.clone()),
        ValueType::BlobValue(b) => ValueKind::Blob(b.to_vec()),
        ValueType::GeoPointValue(g) => ValueKind::GeoPoint {
            latitude: g.latitude,
            longitude: g.longitude,
        },
        ValueType::EntityValue(e) => ValueKind::Entity(Box::new(stored_entity_from_proto(e)?)),
        ValueType::ArrayValue(a) => ValueKind::Array(
            a.values
                .iter()
                .map(|v| {
                    let mut v = stored_value_from_proto(v)?;
                    // Vector components are never indexed.
                    v.exclude_from_indexes |= value.meaning == VECTOR_MEANING;
                    Some(v)
                })
                .collect::<Option<_>>()?,
        ),
    };
    Some(Value {
        kind,
        meaning: value.meaning,
        exclude_from_indexes: value.exclude_from_indexes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope::new("p", "").unwrap()
    }

    fn pkey(path: &[(&str, Option<pb::key::path_element::IdType>)]) -> pb::Key {
        pb::Key {
            partition_id: None,
            path: path
                .iter()
                .map(|(k, id)| pb::key::PathElement {
                    kind: (*k).into(),
                    id_type: id.clone(),
                })
                .collect(),
        }
    }

    use pb::key::path_element::IdType;

    #[test]
    fn project_ids() {
        for ok in ["p", "1p", "p-q", "x"] {
            assert!(Scope::new(ok, "").is_ok(), "{ok}");
        }
        for bad in ["P", "p_q", "a.b", "-p", "s~p"] {
            assert_eq!(
                Scope::new(bad, "").unwrap_err().message(),
                format!("\"{bad}\" is an invalid project id.")
            );
        }
        assert_eq!(
            Scope::new("p", "(default)").unwrap_err().message(),
            "\"(default)\" is an invalid database id."
        );
    }

    #[test]
    fn key_messages() {
        let incomplete = pkey(&[("K", None), ("C", Some(IdType::Name("a".into())))]);
        let err = key_from_proto(
            &incomplete,
            &scope(),
            KeyUse::Write {
                allow_incomplete: true,
            },
        )
        .unwrap_err();
        assert_eq!(
            err.message(),
            "Key path element must not be incomplete: [K: , C: a]"
        );
        let complete = pkey(&[("A", Some(IdType::Id(5)))]);
        let err = key_from_proto(&complete, &scope(), KeyUse::Allocate).unwrap_err();
        assert_eq!(
            err.message(),
            "Key path element must not be complete: [A: 5]"
        );
        let reserved = pkey(&[("__K__", Some(IdType::Name("a".into())))]);
        assert!(key_from_proto(&reserved, &scope(), KeyUse::Read).is_ok());
        assert_eq!(
            key_from_proto(
                &reserved,
                &scope(),
                KeyUse::Write {
                    allow_incomplete: false
                }
            )
            .unwrap_err()
            .message(),
            "The kind \"__K__\" is reserved."
        );
    }

    #[test]
    fn partition_mismatch_message() {
        let mut k = pkey(&[("K", Some(IdType::Name("a".into())))]);
        k.partition_id = Some(pb::PartitionId {
            database_id: "x".into(),
            ..Default::default()
        });
        let scope = Scope::new("p", "x").unwrap();
        assert_eq!(
            key_from_proto(&k, &scope, KeyUse::Read)
                .unwrap_err()
                .message(),
            "mismatched databases within request: <unknown!>~p#x vs. <unknown!>~#x"
        );
    }

    #[test]
    fn timestamps_truncate_to_micros() {
        let ts = prost_types::Timestamp {
            seconds: 1,
            nanos: 123_456_789,
        };
        assert_eq!(timestamp_to_micros(&ts).unwrap(), 1_123_456);
        assert_eq!(
            micros_to_timestamp(-1),
            prost_types::Timestamp {
                seconds: -1,
                nanos: 999_999_000
            }
        );
    }
}
