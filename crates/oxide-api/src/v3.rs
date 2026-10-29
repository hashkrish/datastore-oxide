//! v1 entities as App Engine `storage_onestore_v3.EntityProto`s: the "user-visible" form Java
//! exports (and shows v3 clients for v1-written data), and the v1 view of such a proto.

use std::collections::BTreeMap;

use bytes::Bytes;
use os::property_value::{PointValue, ReferenceValue, UserValue, reference_value};
use oxide_model::{Entity, IdOrName, Key, PartitionId, PathElement, Value, ValueKind};
use oxide_proto::storage_onestore_v3 as os;
use prost::Message;

use crate::appengine::entity::{BLOB, BYTESTRING, EMPTY_LIST, ENTITY_PROTO, GD_WHEN, GEORSS_POINT};

/// v1 meaning of a user value (an entity of `email`, `auth_domain`, `user_id`).
const USER: i32 = 20;
/// v1 meaning of a point that has no `GEORSS_POINT` meaning in v3.
const POINT: i32 = 23;
const MIN_TIMESTAMP_MICROS: i64 = -62_135_596_800_000_000;
const MAX_TIMESTAMP_MICROS: i64 = 253_402_300_799_999_999;

/// Serializes `entity` (its v1 properties; `Entity::v3` is ignored) as Java's user-visible
/// `EntityProto`: key apps are project ids, properties in map order, excluded values in
/// `raw_property`, and `entity_group` the key's root (empty for a keyless embedded entity).
pub fn encode_entity(entity: &Entity) -> Vec<u8> {
    entity_proto(entity).encode_to_vec()
}

fn entity_proto(entity: &Entity) -> os::EntityProto {
    let mut proto = os::EntityProto {
        key: entity.key.as_ref().map(reference).unwrap_or_default(),
        entity_group: os::Path {
            element: entity
                .key
                .iter()
                .flat_map(|k| k.path.first())
                .map(element)
                .collect(),
        },
        ..Default::default()
    };
    for (name, value) in &entity.properties {
        let name = Bytes::copy_from_slice(name.as_bytes());
        match &value.kind {
            ValueKind::Array(values) if values.is_empty() => proto.property.push(os::Property {
                meaning: Some(EMPTY_LIST),
                name,
                ..Default::default()
            }),
            ValueKind::Array(values) => {
                for v in values {
                    push_property(&mut proto, &name, v, true);
                }
            }
            _ => push_property(&mut proto, &name, value, false),
        }
    }
    proto
}

fn push_property(proto: &mut os::EntityProto, name: &Bytes, value: &Value, multiple: bool) {
    let (meaning, value_proto) = property_value(value);
    let property = os::Property {
        meaning: (meaning != 0).then_some(meaning),
        name: name.clone(),
        multiple,
        value: value_proto,
        ..Default::default()
    };
    if value.exclude_from_indexes {
        proto.raw_property.push(property);
    } else {
        proto.property.push(property);
    }
}

fn property_value(value: &Value) -> (i32, os::PropertyValue) {
    let mut pv = os::PropertyValue::default();
    let meaning = match &value.kind {
        ValueKind::Null | ValueKind::Array(_) => value.meaning,
        ValueKind::Boolean(b) => {
            pv.boolean_value = Some(*b);
            value.meaning
        }
        ValueKind::Integer(i) => {
            pv.int64_value = Some(*i);
            value.meaning
        }
        ValueKind::Double(d) => {
            pv.double_value = Some(if *d == 0.0 { 0.0 } else { *d });
            value.meaning
        }
        ValueKind::Timestamp(micros) => {
            pv.int64_value = Some(*micros);
            GD_WHEN
        }
        ValueKind::Key(key) => {
            pv.referencevalue = Some(reference_value(key));
            value.meaning
        }
        ValueKind::String(s) => {
            pv.string_value = Some(Bytes::copy_from_slice(s.as_bytes()));
            value.meaning
        }
        ValueKind::Blob(b) => {
            pv.string_value = Some(Bytes::copy_from_slice(b));
            match value.meaning {
                0 if value.exclude_from_indexes => BLOB,
                0 => BYTESTRING,
                m => m,
            }
        }
        ValueKind::GeoPoint {
            latitude,
            longitude,
        } => {
            pv.pointvalue = Some(PointValue {
                x: *latitude,
                y: *longitude,
            });
            if value.meaning == POINT {
                0
            } else {
                GEORSS_POINT
            }
        }
        ValueKind::Entity(e) if value.meaning == USER => {
            let text = |name: &str| match e.properties.get(name).map(|v| &v.kind) {
                Some(ValueKind::String(s)) => Some(Bytes::copy_from_slice(s.as_bytes())),
                _ => None,
            };
            pv.uservalue = Some(UserValue {
                email: text("email").unwrap_or_default(),
                auth_domain: text("auth_domain").unwrap_or_default(),
                nickname: None,
                gaiaid: 0,
                obfuscated_gaiaid: text("user_id"),
                federated_identity: text("federated_identity"),
                federated_provider: text("federated_provider"),
            });
            0
        }
        ValueKind::Entity(e) => {
            pv.string_value = Some(encode_entity(e).into());
            ENTITY_PROTO
        }
    };
    (meaning, pv)
}

fn bytes(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

fn optional(s: &str) -> Option<Bytes> {
    (!s.is_empty()).then(|| bytes(s))
}

fn element(e: &PathElement) -> os::path::Element {
    let (id, name) = match &e.id {
        Some(IdOrName::Id(id)) => (Some(*id), None),
        Some(IdOrName::Name(name)) => (None, Some(bytes(name))),
        None => (None, None),
    };
    os::path::Element {
        r#type: bytes(&e.kind),
        id,
        name,
    }
}

fn reference(key: &Key) -> os::Reference {
    os::Reference {
        app: bytes(&key.partition.project_id),
        path: os::Path {
            element: key.path.iter().map(element).collect(),
        },
        name_space: optional(&key.partition.namespace_id),
        database_id: optional(&key.partition.database_id),
    }
}

fn reference_value(key: &Key) -> ReferenceValue {
    ReferenceValue {
        app: bytes(&key.partition.project_id),
        pathelement: key
            .path
            .iter()
            .map(|e| {
                let e = element(e);
                reference_value::PathElement {
                    r#type: e.r#type,
                    id: e.id,
                    name: e.name,
                }
            })
            .collect(),
        name_space: optional(&key.partition.namespace_id),
        database_id: optional(&key.partition.database_id),
    }
}

/// An `EntityProto` that could not be parsed (at the top level or in an embedded entity).
#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError;

type R<T> = Result<T, DecodeError>;

/// The v1 view of a serialized `EntityProto`: key apps become project ids, `property` values are
/// indexed and `raw_property` ones excluded, repeated names and `multiple` values become arrays
/// (indexed values first), and v3 meanings map to v1 value types (`GD_WHEN` timestamps,
/// `BLOB`/`BYTESTRING` blobs, `ENTITY_PROTO` entities, user values as meaning-20 entities, points
/// without `GEORSS_POINT` with meaning 23).
///
/// With `project` (import), apps are rewritten as an import does: the key's app (and, recursively,
/// those of indexed embedded entities) unless it is empty, and every indexed key value's app.
/// Excluded values keep theirs. Owner, `kind`, `kind_uri`, `meaning_uri` and `entity_group` are not
/// part of the view.
pub fn decode_entity(bytes: &[u8], project: Option<&str>) -> R<Entity> {
    let proto = os::EntityProto::decode(bytes).map_err(|_| DecodeError)?;
    view(&proto, project)
}

fn view(proto: &os::EntityProto, project: Option<&str>) -> R<Entity> {
    let key = &proto.key;
    let key = (!key.path.element.is_empty()).then(|| {
        let app = match project {
            Some(project) if !key.app.is_empty() => project.to_string(),
            _ => text(&key.app),
        };
        Key {
            partition: partition(app, key.database_id.as_ref(), key.name_space.as_ref()),
            path: key
                .path
                .element
                .iter()
                .map(|e| path_element(&e.r#type, e.id, e.name.as_ref()))
                .collect(),
        }
    });
    // Per name: its values and whether it is a list.
    let mut values: BTreeMap<String, (Vec<Value>, bool)> = BTreeMap::new();
    for (list, excluded) in [(&proto.property, false), (&proto.raw_property, true)] {
        for property in list {
            let entry = values.entry(text(&property.name)).or_default();
            if property.meaning == Some(EMPTY_LIST) && property.value == Default::default() {
                entry.1 = true;
                continue;
            }
            let project = if excluded { None } else { project };
            entry.0.push(value(property, excluded, project)?);
            entry.1 |= property.multiple || entry.0.len() > 1;
        }
    }
    let properties = values
        .into_iter()
        .map(|(name, (mut values, list))| {
            let value = if list {
                Value::new(ValueKind::Array(values))
            } else {
                values.pop().expect("one value")
            };
            (name, value)
        })
        .collect();
    Ok(Entity {
        key,
        properties,
        v3: None,
    })
}

fn value(property: &os::Property, excluded: bool, project: Option<&str>) -> R<Value> {
    let pv = &property.value;
    let meaning = property.meaning.unwrap_or(0);
    let (kind, meaning) = if let Some(i) = pv.int64_value {
        if meaning == GD_WHEN && (MIN_TIMESTAMP_MICROS..=MAX_TIMESTAMP_MICROS).contains(&i) {
            (ValueKind::Timestamp(i), 0)
        } else {
            (ValueKind::Integer(i), meaning)
        }
    } else if let Some(b) = pv.boolean_value {
        (ValueKind::Boolean(b), meaning)
    } else if let Some(s) = &pv.string_value {
        match meaning {
            ENTITY_PROTO => (ValueKind::Entity(Box::new(decode_entity(s, project)?)), 0),
            BLOB | BYTESTRING => (ValueKind::Blob(s.to_vec()), 0),
            // v1's compressed-blob meaning has no v3 counterpart and is kept on the blob.
            22 => (ValueKind::Blob(s.to_vec()), meaning),
            _ => (ValueKind::String(text(s)), meaning),
        }
    } else if let Some(d) = pv.double_value {
        (ValueKind::Double(d), meaning)
    } else if let Some(p) = &pv.pointvalue {
        let kind = ValueKind::GeoPoint {
            latitude: p.x,
            longitude: p.y,
        };
        (kind, if meaning == GEORSS_POINT { 0 } else { POINT })
    } else if let Some(u) = &pv.uservalue {
        let mut user = Entity::default();
        let fields = [
            ("email", Some(&u.email)),
            ("auth_domain", Some(&u.auth_domain)),
            ("user_id", u.obfuscated_gaiaid.as_ref()),
            ("federated_identity", u.federated_identity.as_ref()),
            ("federated_provider", u.federated_provider.as_ref()),
        ];
        for (name, field) in fields {
            if let Some(field) = field {
                let mut v = Value::new(ValueKind::String(text(field)));
                v.exclude_from_indexes = true;
                user.properties.insert(name.into(), v);
            }
        }
        (ValueKind::Entity(Box::new(user)), USER)
    } else if let Some(r) = &pv.referencevalue {
        let app = project.map_or_else(|| text(&r.app), str::to_string);
        let key = Key {
            partition: partition(app, r.database_id.as_ref(), r.name_space.as_ref()),
            path: r
                .pathelement
                .iter()
                .map(|e| path_element(&e.r#type, e.id, e.name.as_ref()))
                .collect(),
        };
        (ValueKind::Key(key), meaning)
    } else {
        (ValueKind::Null, meaning)
    };
    Ok(Value {
        kind,
        meaning,
        exclude_from_indexes: excluded,
    })
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn partition(
    project_id: String,
    database: Option<&Bytes>,
    namespace: Option<&Bytes>,
) -> PartitionId {
    PartitionId {
        project_id,
        database_id: database.map(|d| text(d)).unwrap_or_default(),
        namespace_id: namespace.map(|n| text(n)).unwrap_or_default(),
    }
}

fn path_element(kind: &[u8], id: Option<i64>, name: Option<&Bytes>) -> PathElement {
    let id = match (id, name) {
        (Some(id), _) if id != 0 => Some(IdOrName::Id(id)),
        (_, Some(name)) => Some(IdOrName::Name(text(name))),
        _ => None,
    };
    PathElement::new(text(kind), id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn key(project: &str, ns: &str, path: &[(&str, IdOrName)]) -> Key {
        Key {
            partition: PartitionId {
                project_id: project.into(),
                database_id: String::new(),
                namespace_id: ns.into(),
            },
            path: path
                .iter()
                .map(|(k, id)| PathElement::new(*k, Some(id.clone())))
                .collect(),
        }
    }

    fn entity(key: Option<Key>, props: Vec<(&str, Value)>) -> Entity {
        Entity {
            key,
            properties: props.into_iter().map(|(n, v)| (n.to_string(), v)).collect(),
            v3: None,
        }
    }

    fn excluded(kind: ValueKind) -> Value {
        Value {
            kind,
            meaning: 0,
            exclude_from_indexes: true,
        }
    }

    #[test]
    fn simple_entities_match_java_bytes() {
        // From export_basic.golden.
        let e = entity(
            Some(key("p", "", &[("L", IdOrName::Id(5))])),
            vec![("v", ValueKind::Integer(3).into())],
        );
        assert_eq!(
            hex(&encode_entity(&e)),
            "6a0c6a017072070b12014c18050c72091a017620002a0208038201070b12014c18050c"
        );
        let e = entity(
            Some(key("p", "ns1", &[("K", IdOrName::Name("n".into()))])),
            vec![("v", ValueKind::Integer(4).into())],
        );
        assert_eq!(
            hex(&encode_entity(&e)),
            "6a136a017072080b12014b22016e0ca201036e733172091a017620002a0208048201080b12014b22016e0c"
        );
    }

    #[test]
    fn value_mappings_match_java_bytes() {
        let k = key("p", "", &[("T", IdOrName::Name("all".into()))]);
        let embedded = entity(None, vec![("x", ValueKind::Integer(1).into())]);
        let e = entity(
            Some(k),
            vec![
                (
                    "arr",
                    ValueKind::Array(vec![
                        ValueKind::Integer(1).into(),
                        ValueKind::String("a".into()).into(),
                        ValueKind::Null.into(),
                        ValueKind::Entity(Box::default()).into(),
                    ])
                    .into(),
                ),
                ("arr0", ValueKind::Array(vec![]).into()),
                ("blob", ValueKind::Blob(vec![0, 1, 2, 3]).into()),
                ("dbl8", ValueKind::Double(-0.0).into()),
                (
                    "geo",
                    ValueKind::GeoPoint {
                        latitude: 1.5,
                        longitude: -2.0,
                    }
                    .into(),
                ),
                (
                    "key2",
                    ValueKind::Key(key(
                        "p",
                        "ns",
                        &[("X", IdOrName::Name("n".into())), ("Y", IdOrName::Id(1))],
                    ))
                    .into(),
                ),
                ("ts6", ValueKind::Timestamp(MIN_TIMESTAMP_MICROS).into()),
                ("unidx", excluded(ValueKind::String("u".into()))),
                ("unidxe", excluded(ValueKind::Entity(Box::new(embedded)))),
            ],
        );
        let expected = concat!(
            "6a0f6a0170720a0b1201542203616c6c0c",
            "720b1a0361727220012a020801",
            "720c1a0361727220012a031a0161",
            "72091a0361727220012a00",
            "721608131a0361727220012a0b1a096a046a007200820100",
            "720c08181a046172723020002a00",
            "721208101a04626c6f6220002a061a0400010203",
            "72131a0464626c3820002a09210000000000000000",
            "721f08091a0367656f20002a142b31000000000000f83f3900000000000000c02c",
            "72251a046b65793220002a1b636a0170737a01588a01016e74737a015980010174a201026e7364",
            "721608071a0374733620002a0b088080d1869080d091ff01",
            "7a0e1a05756e69647820002a031a0175",
            "7a2408131a06756e6964786520002a161a146a046a00720072091a017820002a020801820100",
            "82010a0b1201542203616c6c0c",
        );
        assert_eq!(hex(&encode_entity(&e)), expected);
    }

    #[test]
    fn round_trips_v1_values() {
        let inner = entity(
            Some(key("p", "", &[("E", IdOrName::Name("e".into()))])),
            vec![("q", ValueKind::Boolean(false).into())],
        );
        let user = entity(
            None,
            vec![
                ("email", excluded(ValueKind::String("a@b".into()))),
                ("auth_domain", excluded(ValueKind::String("d".into()))),
                ("user_id", excluded(ValueKind::String("1".into()))),
            ],
        );
        let mut rating = Value::new(ValueKind::Integer(50));
        rating.meaning = 13;
        let mut pt = Value::new(ValueKind::GeoPoint {
            latitude: 1.0,
            longitude: 2.0,
        });
        pt.meaning = POINT;
        let mut u = Value::new(ValueKind::Entity(Box::new(user)));
        u.meaning = USER;
        let mut text = excluded(ValueKind::String("t".into()));
        text.meaning = 15;
        let mut k = key("p", "ns", &[("K", IdOrName::Id(-3))]);
        k.partition.database_id = "d2".into();
        let e = entity(
            Some(k.clone()),
            vec![
                ("b", ValueKind::Blob(vec![1]).into()),
                ("rb", excluded(ValueKind::Blob(vec![2]))),
                ("ent", ValueKind::Entity(Box::new(inner)).into()),
                ("rating", rating),
                ("pt", pt),
                ("u", u),
                ("text", text),
                ("k", ValueKind::Key(k).into()),
                ("big", ValueKind::Integer(i64::MIN).into()),
                ("nan", ValueKind::Double(f64::NAN).into()),
                ("ts", ValueKind::Timestamp(-1).into()),
                ("empty", ValueKind::Array(vec![]).into()),
                (
                    "one",
                    ValueKind::Array(vec![excluded(ValueKind::Null)]).into(),
                ),
                (
                    "mixed",
                    ValueKind::Array(vec![
                        ValueKind::Integer(1).into(),
                        excluded(ValueKind::Integer(2)),
                    ])
                    .into(),
                ),
            ],
        );
        assert_eq!(decode_entity(&encode_entity(&e), None), Ok(e));
    }

    #[test]
    fn decodes_v3_only_constructs() {
        let property = |name: &str, multiple: bool, meaning: Option<i32>, value| os::Property {
            meaning,
            name: bytes(name),
            multiple,
            value,
            ..Default::default()
        };
        let proto = os::EntityProto {
            key: reference(&key("app", "", &[("K", IdOrName::Name("a".into()))])),
            property: vec![
                property(
                    "dup",
                    false,
                    None,
                    os::PropertyValue {
                        int64_value: Some(1),
                        ..Default::default()
                    },
                ),
                property(
                    "dup",
                    false,
                    None,
                    os::PropertyValue {
                        int64_value: Some(2),
                        string_value: Some(bytes("ignored")),
                        ..Default::default()
                    },
                ),
                property(
                    "when",
                    false,
                    Some(GD_WHEN),
                    os::PropertyValue {
                        int64_value: Some(i64::MAX),
                        ..Default::default()
                    },
                ),
                property(
                    "blob",
                    false,
                    Some(BLOB),
                    os::PropertyValue {
                        string_value: Some(bytes("b")),
                        ..Default::default()
                    },
                ),
            ],
            raw_property: vec![property(
                "dup",
                true,
                None,
                os::PropertyValue {
                    int64_value: Some(3),
                    ..Default::default()
                },
            )],
            ..Default::default()
        };
        let e = decode_entity(&proto.encode_to_vec(), None).unwrap();
        assert_eq!(e.key.unwrap().partition.project_id, "app");
        let ints = |v: &Value| match &v.kind {
            ValueKind::Array(vs) => vs
                .iter()
                .map(|v| (v.kind.clone(), v.exclude_from_indexes))
                .collect::<Vec<_>>(),
            _ => panic!("not an array"),
        };
        assert_eq!(
            ints(&e.properties["dup"]),
            [
                (ValueKind::Integer(1), false),
                (ValueKind::Integer(2), false),
                (ValueKind::Integer(3), true)
            ]
        );
        assert_eq!(e.properties["when"].kind, ValueKind::Integer(i64::MAX));
        assert_eq!(e.properties["when"].meaning, GD_WHEN);
        assert_eq!(e.properties["blob"], ValueKind::Blob(b"b".to_vec()).into());
        assert_eq!(decode_entity(b"\x0a", None), Err(DecodeError));
    }

    #[test]
    fn import_rewrites_indexed_apps_only() {
        let src = |path: &[(&str, IdOrName)]| key("src", "", path);
        let inner = entity(
            Some(src(&[("E", IdOrName::Id(1))])),
            vec![("r", ValueKind::Key(src(&[("R", IdOrName::Id(2))])).into())],
        );
        let keyless = entity(None, vec![]);
        let e = entity(
            Some(src(&[("K", IdOrName::Id(1))])),
            vec![
                ("ri", ValueKind::Key(src(&[("R", IdOrName::Id(3))])).into()),
                (
                    "ru",
                    excluded(ValueKind::Key(src(&[("R", IdOrName::Id(4))]))),
                ),
                ("ei", ValueKind::Entity(Box::new(inner.clone())).into()),
                ("eu", excluded(ValueKind::Entity(Box::new(inner)))),
                ("en", ValueKind::Entity(Box::new(keyless)).into()),
            ],
        );
        let got = decode_entity(&encode_entity(&e), Some("dst")).unwrap();
        let project = |k: &Key| k.partition.project_id.clone();
        assert_eq!(project(got.key.as_ref().unwrap()), "dst");
        let get = |name: &str| got.properties[name].kind.clone();
        let ValueKind::Key(ri) = get("ri") else {
            panic!()
        };
        let ValueKind::Key(ru) = get("ru") else {
            panic!()
        };
        assert_eq!((project(&ri), project(&ru)), ("dst".into(), "src".into()));
        let ValueKind::Entity(ei) = get("ei") else {
            panic!()
        };
        let ValueKind::Entity(eu) = get("eu") else {
            panic!()
        };
        let ValueKind::Entity(en) = get("en") else {
            panic!()
        };
        assert_eq!(project(ei.key.as_ref().unwrap()), "dst");
        assert_eq!(project(eu.key.as_ref().unwrap()), "src");
        let ValueKind::Key(inner_ref) = &ei.properties["r"].kind else {
            panic!()
        };
        assert_eq!(project(inner_ref), "dst");
        assert_eq!(en.key, None);
    }
}
