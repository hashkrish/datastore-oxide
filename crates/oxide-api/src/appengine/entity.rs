//! App Engine v3 entities: keys, the stored `EntityProto` and the v1 view derived from it.

use bytes::Bytes;
use oxide_model::{Entity, IdOrName, Key, PartitionId, PathElement, Value, ValueKind};
use oxide_proto::storage_onestore_v3 as os;
use prost::Message;

use super::appid::Unscrubbable;

pub const GD_WHEN: i32 = 7;
pub const GEORSS_POINT: i32 = 9;
pub const BLOB: i32 = 14;
pub const TEXT: i32 = 15;
pub const BYTESTRING: i32 = 16;
pub const INDEX_VALUE: i32 = 18;
pub const ENTITY_PROTO: i32 = 19;
pub const EMPTY_LIST: i32 = 24;
/// v1 meanings of App Engine values without a v1 type.
const GEO_POINT_WITHOUT_MEANING: i32 = 23;
const USER: i32 = 20;

/// A proto1 byte string as text.
pub fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn element_of(e: &os::path::Element) -> PathElement {
    let id = match (&e.name, e.id) {
        (Some(name), _) => Some(IdOrName::Name(text(name))),
        (None, Some(id)) if id != 0 => Some(IdOrName::Id(id)),
        _ => None,
    };
    PathElement::new(text(&e.r#type), id)
}

/// The key of a reference, in `project`.
pub fn key_of(r: &os::Reference, project: &str) -> Key {
    Key {
        partition: PartitionId {
            project_id: project.to_string(),
            database_id: r.database_id.as_deref().map(text).unwrap_or_default(),
            namespace_id: r.name_space.as_deref().map(text).unwrap_or_default(),
        },
        path: r.path.element.iter().map(element_of).collect(),
    }
}

pub fn path_of(path: &[PathElement]) -> os::Path {
    os::Path {
        element: path
            .iter()
            .map(|e| os::path::Element {
                r#type: e.kind.clone().into(),
                id: match &e.id {
                    Some(IdOrName::Id(id)) => Some(*id),
                    _ => None,
                },
                name: match &e.id {
                    Some(IdOrName::Name(name)) => Some(name.clone().into()),
                    _ => None,
                },
            })
            .collect(),
    }
}

fn non_empty(s: &str) -> Option<Bytes> {
    (!s.is_empty()).then(|| Bytes::from(s.to_string()))
}

/// The reference of `key` under app id `app`.
pub fn reference(key: &Key, app: &str) -> os::Reference {
    os::Reference {
        app: app.to_string().into(),
        path: path_of(&key.path),
        name_space: non_empty(&key.partition.namespace_id),
        database_id: non_empty(&key.partition.database_id),
    }
}

/// An element in protobuf text format, as Java prints it in error messages.
fn element_text(e: &os::path::Element) -> String {
    let mut out = format!("type: {}\n", oxide_engine::debug_quote(&text(&e.r#type)));
    if let Some(id) = e.id.filter(|id| *id != 0) {
        out.push_str(&format!("id: {id}\n"));
    }
    if let Some(name) = &e.name {
        out.push_str(&format!(
            "name: {}\n",
            oxide_engine::debug_quote(&text(name))
        ));
    }
    out
}

/// Whether an element has neither a name nor a (non-zero) id.
pub(super) fn incomplete(e: &os::path::Element) -> bool {
    e.name.is_none() && e.id.is_none_or(|id| id == 0)
}

/// The checks a read (or delete) key gets: a complete path in a set app.
pub fn check_read_key(r: &os::Reference) -> Result<(), String> {
    if r.app.is_empty() {
        return Err("A OnestoreEntity.Reference must set app.".into());
    }
    if r.path.element.is_empty() {
        return Err("A OnestoreEntity.Path must have at least one Element.".into());
    }
    for e in &r.path.element {
        if e.name.is_some() && e.id.is_some() {
            return Err("A OnestoreEntity.Path.Element must not set both id and name.".into());
        }
        if e.name.as_ref().is_some_and(Bytes::is_empty) {
            return Err(
                "When OnestoreEntity.Path.Element has name set it must not be empty.".into(),
            );
        }
        if incomplete(e) {
            return Err(format!("Incomplete key.path.element: {}", element_text(e)));
        }
    }
    if r.name_space.as_ref().is_some_and(Bytes::is_empty) {
        return Err("The namespace is the empty string.".into());
    }
    Ok(())
}

fn is_embedded_entity(p: &os::Property) -> bool {
    p.meaning == Some(ENTITY_PROTO) && p.value.string_value.is_some()
}

/// Rewrites every app id in an entity (its key, reference values, embedded entities) with `f`.
/// Embedded entities that do not parse are left as they are.
pub fn map_apps(
    e: &mut os::EntityProto,
    f: &mut dyn FnMut(&[u8]) -> Result<Bytes, Unscrubbable>,
) -> Result<(), Unscrubbable> {
    if !e.key.app.is_empty() {
        e.key.app = f(&e.key.app)?;
    }
    for p in e.property.iter_mut().chain(e.raw_property.iter_mut()) {
        if let Some(r) = &mut p.value.referencevalue
            && !r.app.is_empty()
        {
            r.app = f(&r.app)?;
        }
        if is_embedded_entity(p) {
            let bytes = p.value.string_value.as_ref().expect("checked");
            match os::EntityProto::decode(bytes.as_ref()) {
                Ok(mut inner) => {
                    map_apps(&mut inner, f)?;
                    p.value.string_value = Some(inner.encode_to_vec().into());
                }
                Err(_) => tracing::warn!("Received an entity value which could not be parsed."),
            }
        }
    }
    Ok(())
}

/// The form reads return: computed properties dropped and stashed ones moved back to their
/// positions (appended when out of range).
pub fn read_form(mut e: os::EntityProto) -> os::EntityProto {
    let props: Vec<os::Property> = std::mem::take(&mut e.property)
        .into_iter()
        .filter(|p| p.computed != Some(true))
        .collect();
    let (mut stashed, mut plain): (Vec<_>, Vec<_>) = props
        .into_iter()
        .partition(|p| p.stashed.is_some_and(|i| i >= 0));
    stashed.sort_by_key(|p| p.stashed);
    for mut p in stashed {
        let at = usize::try_from(p.stashed.take().unwrap_or_default()).unwrap_or(usize::MAX);
        let len = plain.len();
        plain.insert(at.min(len), p);
    }
    for p in &mut plain {
        p.stashed = None;
        p.computed = None;
    }
    e.property = plain;
    e.raw_property.retain(|p| p.computed != Some(true));
    for p in &mut e.raw_property {
        p.stashed = None;
        p.computed = None;
    }
    e
}

/// The stored `EntityProto` of an entity (app ids are project ids): the v3-written proto, else
/// the user-visible encoding of its v1 form.
pub fn stored_proto(entity: &Entity) -> os::EntityProto {
    let bytes = match &entity.v3 {
        Some(bytes) => bytes.to_vec(),
        None => crate::v3::encode_entity(entity),
    };
    os::EntityProto::decode(bytes.as_slice()).expect("stored entity protos parse")
}

/// The v1 view of a stored entity, as Java's conversion derives it.
pub fn view_of(e: &os::EntityProto) -> Entity {
    let project = text(&e.key.app);
    let mut entity = Entity::new(key_of(&e.key, &project));
    entity.properties = properties_of(e, &project);
    entity
}

fn properties_of(e: &os::EntityProto, project: &str) -> oxide_model::Properties {
    let mut grouped: Vec<(String, Vec<Value>, bool)> = Vec::new();
    let all = e
        .property
        .iter()
        .map(|p| (p, false))
        .chain(e.raw_property.iter().map(|p| (p, true)));
    for (p, excluded) in all {
        let name = text(&p.name);
        let value = value_of(p, excluded, project);
        match grouped.iter_mut().find(|(n, _, _)| *n == name) {
            Some((_, values, multiple)) => {
                values.push(value);
                *multiple = true;
            }
            None => grouped.push((name, vec![value], p.multiple)),
        }
    }
    grouped
        .into_iter()
        .map(|(name, mut values, multiple)| {
            let value = if multiple {
                Value::new(ValueKind::Array(values))
            } else {
                values.pop().expect("one value")
            };
            (name, value)
        })
        .collect()
}

/// The v1 value of one property value. A value with several fields set counts as the first of
/// them (integer, boolean, string, double, point, user, reference).
fn value_of(p: &os::Property, excluded: bool, project: &str) -> Value {
    let meaning = p.meaning.unwrap_or(0);
    let v = &p.value;
    let kept = |m: i32| {
        if (1..=13).contains(&m) || m == 17 {
            m
        } else {
            0
        }
    };
    let (kind, meaning) = if let Some(i) = v.int64_value {
        if meaning == GD_WHEN {
            (ValueKind::Timestamp(i), 0)
        } else {
            (ValueKind::Integer(i), kept(meaning))
        }
    } else if let Some(b) = v.boolean_value {
        (ValueKind::Boolean(b), 0)
    } else if let Some(s) = &v.string_value {
        match meaning {
            BLOB | BYTESTRING => (ValueKind::Blob(s.to_vec()), 0),
            ENTITY_PROTO => match os::EntityProto::decode(s.as_ref()) {
                Ok(inner) => (ValueKind::Entity(Box::new(embedded_of(&inner, project))), 0),
                Err(_) => (ValueKind::Blob(s.to_vec()), 0),
            },
            TEXT => (ValueKind::String(text(s)), TEXT),
            m => (ValueKind::String(text(s)), kept(m)),
        }
    } else if let Some(d) = v.double_value {
        (ValueKind::Double(d), 0)
    } else if let Some(point) = &v.pointvalue {
        let kind = ValueKind::GeoPoint {
            latitude: point.x,
            longitude: point.y,
        };
        if meaning == GEORSS_POINT {
            (kind, 0)
        } else {
            (kind, GEO_POINT_WITHOUT_MEANING)
        }
    } else if let Some(user) = &v.uservalue {
        let mut e = Entity::default();
        let mut field = |name: &str, value: &[u8]| {
            e.properties.insert(
                name.into(),
                Value {
                    kind: ValueKind::String(text(value)),
                    meaning: 0,
                    exclude_from_indexes: true,
                },
            );
        };
        field("email", &user.email);
        field("auth_domain", &user.auth_domain);
        if let Some(id) = &user.obfuscated_gaiaid {
            field("user_id", id);
        }
        (ValueKind::Entity(Box::new(e)), USER)
    } else if let Some(r) = &v.referencevalue {
        let key = Key {
            partition: PartitionId {
                project_id: text(&r.app),
                database_id: r.database_id.as_deref().map(text).unwrap_or_default(),
                namespace_id: r.name_space.as_deref().map(text).unwrap_or_default(),
            },
            path: r
                .pathelement
                .iter()
                .map(|e| {
                    element_of(&os::path::Element {
                        r#type: e.r#type.clone(),
                        id: e.id,
                        name: e.name.clone(),
                    })
                })
                .collect(),
        };
        (ValueKind::Key(Box::new(key)), 0)
    } else if meaning == EMPTY_LIST {
        (ValueKind::Array(Vec::new()), 0)
    } else {
        (ValueKind::Null, 0)
    };
    Value {
        kind,
        meaning,
        exclude_from_indexes: excluded,
    }
}

/// An embedded entity: keyed only when its key has a path.
fn embedded_of(e: &os::EntityProto, project: &str) -> Entity {
    let app = text(&e.key.app);
    let project = if app.is_empty() { project } else { &app };
    Entity {
        key: (!e.key.path.element.is_empty()).then(|| key_of(&e.key, project)),
        properties: properties_of(e, project),
        v3: None,
    }
}

/// An entity read from an export file, for import into `project`: the key and indexed reference
/// values (also inside indexed embedded entities) take the project; unindexed values keep theirs.
/// `None` when the entity or an indexed embedded entity does not parse, or its entity group is
/// not a single element.
pub fn imported(bytes: &[u8], project: &str) -> Option<Entity> {
    fn rewrite(e: &mut os::EntityProto, project: &str) -> Option<()> {
        if !e.key.app.is_empty() {
            e.key.app = project.to_string().into();
        }
        for p in &mut e.property {
            if let Some(r) = &mut p.value.referencevalue {
                r.app = project.to_string().into();
            }
            if is_embedded_entity(p) {
                let bytes = p.value.string_value.as_ref().expect("checked");
                let mut inner = os::EntityProto::decode(bytes.as_ref()).ok()?;
                rewrite(&mut inner, project)?;
                p.value.string_value = Some(inner.encode_to_vec().into());
            }
        }
        Some(())
    }
    let mut e = os::EntityProto::decode(bytes).ok()?;
    rewrite(&mut e, project)?;
    e.key.app = project.to_string().into();
    match e.entity_group.element.len() {
        0 => {
            e.entity_group =
                path_of(&key_of(&e.key, project).path[..1.min(e.key.path.element.len())])
        }
        1 => {}
        _ => return None,
    }
    let e = read_form(e);
    let mut entity = view_of(&e);
    entity.v3 = Some(e.encode_to_vec().into());
    Some(entity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(name: &str, value: os::PropertyValue) -> os::Property {
        os::Property {
            name: name.to_string().into(),
            value,
            ..Default::default()
        }
    }

    #[test]
    fn read_form_restores_stashed_and_drops_computed() {
        let mut e = os::EntityProto::default();
        let int = |i| os::PropertyValue {
            int64_value: Some(i),
            ..Default::default()
        };
        e.property = vec![
            os::Property {
                computed: Some(true),
                ..prop("c", int(1))
            },
            os::Property {
                stashed: Some(3),
                ..prop("st", int(2))
            },
            prop("p", int(3)),
            os::Property {
                stashed: Some(0),
                ..prop("st0", int(4))
            },
        ];
        let names: Vec<String> = read_form(e)
            .property
            .iter()
            .map(|p| text(&p.name))
            .collect();
        assert_eq!(names, ["st0", "p", "st"]);
    }

    #[test]
    fn repeated_names_become_arrays() {
        let mut e = os::EntityProto::default();
        e.key.app = "p".into();
        let int = |i| os::PropertyValue {
            int64_value: Some(i),
            ..Default::default()
        };
        e.property = vec![prop("dup", int(1)), prop("dup", int(2))];
        e.raw_property = vec![prop("raw", int(3))];
        let view = view_of(&e);
        assert_eq!(
            view.properties["dup"].kind,
            ValueKind::Array(vec![
                Value::new(ValueKind::Integer(1)),
                Value::new(ValueKind::Integer(2))
            ])
        );
        assert!(view.properties["raw"].exclude_from_indexes);
    }
}
