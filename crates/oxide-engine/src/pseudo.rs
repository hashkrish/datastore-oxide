use crate::index::{IndexValue, index_values};
use crate::query::{Direction, KEY_PROPERTY, Query};
use crate::{Error, Result};
use oxide_model::{Entity, IdOrName, Key, PartitionId, PathElement, Value, ValueKind};
use std::collections::{BTreeMap, BTreeSet};
pub const KIND: &str = "__kind__";
pub const PROPERTY: &str = "__property__";
pub const NAMESPACE: &str = "__namespace__";
pub const ENTITY_GROUP: &str = "__entity_group__";
/// Firestore-in-Datastore mode: the collections under a parent (`HAS_PARENT` required).
pub const COLLECTION: &str = "__collection__";
/// Firestore-in-Datastore mode: the documents of a `__collection__` key.
pub const DOCUMENT: &str = "__document__";
fn named(kind: &str, name: &str) -> PathElement {
    PathElement::new(kind, Some(IdOrName::Name(name.to_string())))
}
fn representation(v: &IndexValue) -> &'static str {
    match v {
        IndexValue::Null => "NULL",
        IndexValue::Int(_) => "INT64",
        IndexValue::Bool(_) => "BOOLEAN",
        IndexValue::Bytes(_) => "STRING",
        IndexValue::Double(_) => "DOUBLE",
        IndexValue::Point { .. } => "POINT",
        IndexValue::Key(_) => "REFERENCE",
    }
}
/// Synthesizes the entities a pseudo-kind query runs over, or `None` for ordinary kinds. In
/// Firestore-in-Datastore mode `__entity_group__` is an ordinary kind.
pub fn entities<'a>(
    q: &Query,
    stored: impl Iterator<Item = (&'a Key, &'a Entity, i64)>,
    firestore: bool,
) -> Result<Option<Vec<Entity>>> {
    let Some(kind) = q.kind.as_deref() else {
        return Ok(None);
    };
    if kind == ENTITY_GROUP && !firestore {
        return Err(Error::invalid_argument(
            "queries not supported on __entity_group__",
        ));
    }
    let firestore_kind = firestore && [COLLECTION, DOCUMENT].contains(&kind);
    if ![KIND, PROPERTY, NAMESPACE].contains(&kind) && !firestore_kind {
        return Ok(None);
    }
    if firestore_kind && !q.shallow {
        return Err(Error::invalid_argument(format!(
            "queries on {kind} must specify a parent"
        )));
    }
    let implicit = q
        .projection
        .iter()
        .chain(&q.group_by)
        .any(|p| p != KEY_PROPERTY);
    if implicit || q.filters.iter().any(|f| f.property != KEY_PROPERTY) {
        return Err(Error::invalid_argument(
            "Only comparison filters on __key__ supported",
        ));
    }
    if q.orders
        .iter()
        .enumerate()
        .any(|(i, o)| i > 0 || o.property != KEY_PROPERTY || o.direction == Direction::Descending)
    {
        return Err(Error::invalid_argument(
            "Only ascending order on __key__ supported",
        ));
    }
    if q.ancestor.is_some() && kind != PROPERTY && !firestore_kind {
        return Err(Error::invalid_argument(format!(
            "ancestor queries on {kind} not allowed"
        )));
    }
    let partition = |namespace: &str| PartitionId {
        project_id: q.project_id.clone(),
        database_id: String::new(),
        namespace_id: namespace.to_string(),
    };
    let in_namespace = |k: &&Key| k.partition.namespace_id == q.namespace_id;
    if firestore_kind {
        return Ok(Some(collections_or_documents(q, kind, stored)?));
    }
    let stored = stored.map(|(k, e, _)| (k, e));
    let entities = match kind {
        KIND => {
            let kinds: BTreeSet<&str> = stored
                .map(|(k, _)| k)
                .filter(in_namespace)
                .map(Key::kind)
                .collect();
            kinds
                .into_iter()
                .map(|k| {
                    Entity::new(Key {
                        partition: partition(&q.namespace_id),
                        path: vec![named(KIND, k)],
                    })
                })
                .collect()
        }
        NAMESPACE => {
            let namespaces: BTreeSet<&str> = stored
                .map(|(k, _)| k)
                .map(|k| k.partition.namespace_id.as_str())
                .collect();
            namespaces
                .into_iter()
                .map(|ns| {
                    let id = if ns.is_empty() {
                        IdOrName::Id(1)
                    } else {
                        IdOrName::Name(ns.to_string())
                    };
                    Entity::new(Key {
                        partition: partition(&q.namespace_id),
                        path: vec![PathElement::new(NAMESPACE, Some(id))],
                    })
                })
                .collect()
        }
        _ => {
            let mut props: BTreeMap<(String, String), BTreeSet<&'static str>> = BTreeMap::new();
            for (key, entity) in stored {
                if key.partition.namespace_id != q.namespace_id {
                    continue;
                }
                for (name, values) in index_values(entity) {
                    let reps = props.entry((key.kind().to_string(), name)).or_default();
                    reps.extend(values.iter().map(representation));
                }
            }
            props
                .into_iter()
                .map(|((kind, name), reps)| {
                    let mut e = Entity::new(Key {
                        partition: partition(&q.namespace_id),
                        path: vec![named(KIND, &kind), named(PROPERTY, &name)],
                    });
                    let values = reps
                        .into_iter()
                        .map(|r| Value::new(ValueKind::String(r.to_string())))
                        .collect();
                    e.properties.insert(
                        "property_representation".into(),
                        Value::new(ValueKind::Array(values)),
                    );
                    e
                })
                .collect()
        }
    };
    Ok(Some(entities))
}
/// `__collection__` (the kinds of the children of the `HAS_PARENT` key, or of root entities) and
/// `__document__` (the children of a `<parent>/__collection__:"Kind"` key), over every namespace
/// and database of the project.
fn collections_or_documents<'a>(
    q: &Query,
    kind: &str,
    stored: impl Iterator<Item = (&'a Key, &'a Entity, i64)>,
) -> Result<Vec<Entity>> {
    let in_partition = |path: Vec<PathElement>| Key {
        partition: PartitionId {
            project_id: q.project_id.clone(),
            database_id: String::new(),
            namespace_id: q.namespace_id.clone(),
        },
        path,
    };
    let parent: &[PathElement] = q.ancestor.as_ref().map_or(&[], |a| &a.path);
    if kind == COLLECTION {
        let kinds: BTreeSet<Key> = stored
            .filter(|(k, _, _)| k.path.len() > parent.len() && k.path.starts_with(parent))
            .map(|(k, _, _)| {
                let mut path = parent.to_vec();
                path.push(named(COLLECTION, &k.path[parent.len()].kind));
                in_partition(path)
            })
            .collect();
        return Ok(kinds.into_iter().map(Entity::new).collect());
    }
    let Some((collection, outer)) = parent.split_last().filter(|(c, _)| c.kind == COLLECTION)
    else {
        return Err(Error::invalid_argument(
            "parent key must have kind __collection__",
        ));
    };
    let Some(IdOrName::Name(child_kind)) = &collection.id else {
        return Err(Error::invalid_argument(
            "__collection__ key must have a name",
        ));
    };
    let mut documents: BTreeMap<Key, Entity> = BTreeMap::new();
    let mut existing: BTreeMap<Key, (&Entity, i64)> = BTreeMap::new();
    for (key, entity, version) in stored {
        if key.path.len() <= outer.len()
            || !key.path.starts_with(outer)
            || key.path[outer.len()].kind != *child_kind
        {
            continue;
        }
        let document = Key {
            partition: key.partition.clone(),
            path: key.path[..=outer.len()].to_vec(),
        };
        if key.path.len() == outer.len() + 1 {
            existing.insert(document.clone(), (entity, version));
        }
        let mut path = parent.to_vec();
        path.push(PathElement::new(DOCUMENT, key.path[outer.len()].id.clone()));
        documents
            .entry(in_partition(path))
            .or_insert_with(|| Entity::new(document.clone()));
    }
    Ok(documents
        .into_iter()
        .map(|(key, stub)| {
            let document = stub.key.expect("documents have keys");
            let mut e = Entity::new(key);
            if let Some((entity, version)) = existing.get(&document) {
                let mut body = (*entity).clone();
                body.key = None;
                body.v3 = None;
                e.properties.insert(
                    "document".into(),
                    Value::new(ValueKind::Entity(Box::new(body))),
                );
                for name in ["created", "updated"] {
                    e.properties
                        .insert(name.into(), Value::new(ValueKind::Integer(*version)));
                }
            }
            e.properties
                .insert("document_key".into(), Value::new(ValueKind::Key(document)));
            e
        })
        .collect())
}
