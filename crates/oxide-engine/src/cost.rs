//! Write cost (`indexUpdates`) and the hidden `__scatter__` property.
//!
//! Ported from the App Engine Java SDK (Apache-2.0, see NOTICE):
//! `LocalDatastoreCostAnalysis` and `LocalDatastoreService.SpecialProperty.SCATTER`.
//! Index entries follow the Cloud Datastore emulator's behavior observed in
//! `tests/conformance/java/index_updates.golden`: embedded entities are flattened into dotted
//! sub-properties and an empty array is one indexed value.

use std::collections::{HashMap, HashSet};

use md5::{Digest, Md5};

use crate::composite::CompositeIndex;
use oxide_model::{Entity, IdOrName, Key, Value, ValueKind, java_string_hash};

/// The meaning of an entity value that stands for an App Engine user.
const USER_MEANING: i32 = 20;

/// Name of the special property App Engine adds to a hash-selected subset of entities.
pub const SCATTER_PROPERTY: &str = "__scatter__";

/// Index-relevant value of one property entry (what an index row would contain).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum IndexValue {
    Value(ValueKind, i32),
    EmptyList,
    Scatter([u8; 2]),
}

/// The hidden `__scatter__` value for `key`, if this entity gets one: the first two bytes of
/// MD5(decimal hash of the key path), kept when the low bit of the first byte is set.
pub fn scatter_value(key: &Key) -> Option<[u8; 2]> {
    let mut hash: i32 = 0;
    for element in &key.path {
        match &element.id {
            Some(IdOrName::Id(id)) => hash = ((hash as i64) ^ id) as i32,
            Some(IdOrName::Name(name)) => hash ^= java_string_hash(name),
            None => return None,
        }
    }
    let digest = Md5::digest(hash.to_string().as_bytes());
    (digest[0] & 0x01 != 0).then_some([digest[0], digest[1]])
}

/// Distinct indexed values per property name, including `__scatter__`. Embedded entities are
/// flattened into `name.sub` values unless an export stripped that index data (`flatten` false:
/// the embedded entity is then one opaque value).
fn index_entries(
    entity: &Entity,
    scatter: bool,
    flatten: bool,
) -> HashMap<String, HashSet<IndexValue>> {
    let mut out: HashMap<String, HashSet<IndexValue>> = HashMap::new();
    for (name, value) in &entity.properties {
        add_value(&mut out, name, value, flatten);
    }
    if let Some(scatter) = entity
        .key
        .as_ref()
        .filter(|_| scatter)
        .and_then(scatter_value)
    {
        out.entry(SCATTER_PROPERTY.into())
            .or_default()
            .insert(IndexValue::Scatter(scatter));
    }
    out
}

fn add_value(
    out: &mut HashMap<String, HashSet<IndexValue>>,
    name: &str,
    value: &Value,
    flatten: bool,
) {
    if value.exclude_from_indexes {
        return;
    }
    match &value.kind {
        ValueKind::Array(values) if values.is_empty() => {
            out.entry(name.into())
                .or_default()
                .insert(IndexValue::EmptyList);
        }
        ValueKind::Array(values) => {
            for v in values {
                add_value(out, name, v, flatten);
            }
        }
        // A user (an entity value with meaning 20, a v3 `UserValue`) is one indexed value.
        ValueKind::Entity(_) if value.meaning == USER_MEANING => {
            out.entry(name.into())
                .or_default()
                .insert(IndexValue::Value(value.kind.clone(), value.meaning));
        }
        ValueKind::Entity(entity) if flatten => {
            // The key of an embedded entity is indexed as `<name>.__key__`.
            if let Some(key) = &entity.key {
                out.entry(format!("{name}.__key__"))
                    .or_default()
                    .insert(IndexValue::Value(ValueKind::Key(Box::new(key.clone())), 0));
            }
            for (sub, v) in &entity.properties {
                add_value(out, &format!("{name}.{sub}"), v, true);
            }
        }
        kind => {
            out.entry(name.into())
                .or_default()
                .insert(IndexValue::Value(kind.clone(), value.meaning));
        }
    }
}

/// Index writes needed to go from `old` to `new` (both stored forms of the same key):
/// 0 if nothing changed, otherwise ascending + descending rows per changed distinct value,
/// plus 1 when the entity is new.
/// `scatter` says whether the hidden `__scatter__` property is in use for `new` (it is not under
/// the SCATTERED auto-id policy); `old` comes with its stored [`Form`].
pub fn write_cost(
    old: Option<(&Entity, Form)>,
    new: &Entity,
    scatter: bool,
    indexes: &[CompositeIndex],
) -> i64 {
    let old_entries = old
        .map(|(o, form)| index_entries(o, form.scatter && !form.stripped, !form.stripped))
        .unwrap_or_default();
    let new_entries = index_entries(new, scatter, true);
    let old_rows = old
        .map(|(o, _)| composite_rows(o, &old_entries, indexes))
        .unwrap_or_default();
    let new_rows = composite_rows(new, &new_entries, indexes);
    let names: HashSet<&String> = old_entries.keys().chain(new_entries.keys()).collect();
    let mut writes = 0i64;
    for name in names {
        let empty = HashSet::new();
        let old_values = old_entries.get(name).unwrap_or(&empty);
        let new_values = new_entries.get(name).unwrap_or(&empty);
        let common = new_values.intersection(old_values).count() as i64;
        let changed = (old_values.len() as i64 - common) + (new_values.len() as i64 - common);
        writes += 2 * changed; // EntitiesByProperty and EntitiesByPropertyDesc
    }
    // One row per composite-index row added or removed.
    writes += old_rows.symmetric_difference(&new_rows).count() as i64;
    if old.is_none() {
        writes += 1;
    }
    writes
}

/// Cost of deleting `old`.
pub fn delete_cost(old: &Entity, form: Form, indexes: &[CompositeIndex]) -> i64 {
    let entries = index_entries(old, form.scatter && !form.stripped, !form.stripped);
    1 + 2 * entries.values().map(|v| v.len() as i64).sum::<i64>()
        + composite_rows(old, &entries, indexes).len() as i64
}

/// The rows composite `indexes` hold for `entity` (with its distinct indexed values `entries`):
/// for each index on its kind, one row per combination of the indexed properties' values, and for
/// ancestor indexes one per ancestor (the entity included). Entities missing a property (or
/// indexes on `__key__`) have none.
fn composite_rows(
    entity: &Entity,
    entries: &HashMap<String, HashSet<IndexValue>>,
    indexes: &[CompositeIndex],
) -> HashSet<(usize, usize, Vec<IndexValue>)> {
    let mut rows = HashSet::new();
    let Some(key) = &entity.key else {
        return rows;
    };
    for (i, index) in indexes.iter().enumerate() {
        if index.kind != key.kind() {
            continue;
        }
        let mut combos: Vec<Vec<IndexValue>> = vec![Vec::new()];
        for p in &index.properties {
            // `__key__` is not a property: indexes on it hold no rows here (as in Java).
            let values: Vec<IndexValue> = entries
                .get(&p.name)
                .map(|v| v.iter().cloned().collect())
                .unwrap_or_default();
            combos = combos
                .iter()
                .flat_map(|c| {
                    values.iter().map(move |v| {
                        let mut c = c.clone();
                        c.push(v.clone());
                        c
                    })
                })
                .collect();
        }
        let ancestors = if index.ancestor { key.path.len() } else { 1 };
        for a in 0..ancestors {
            for c in &combos {
                rows.insert((i, a, c.clone()));
            }
        }
    }
    rows
}

/// How an entity is stored, beyond its properties.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Form {
    /// Written under the sequential id policy, so it gets a `__scatter__` value if its key does.
    pub scatter: bool,
    /// An export stripped the `__scatter__` value and the flattened embedded-entity index data
    /// (a Java emulator side effect, undone by the next write).
    pub stripped: bool,
}

impl Form {
    /// Whether two stored forms of `entity` index identically.
    pub fn same_index_data(self, other: Form, entity: &Entity) -> bool {
        let scatter = |f: Form| {
            f.scatter && !f.stripped && entity.key.as_ref().and_then(scatter_value).is_some()
        };
        let embedded = entity.properties.values().any(has_indexed_entity);
        scatter(self) == scatter(other) && (!embedded || self.stripped == other.stripped)
    }
}

fn has_indexed_entity(value: &Value) -> bool {
    !value.exclude_from_indexes
        && match &value.kind {
            ValueKind::Entity(_) => true,
            ValueKind::Array(values) => values.iter().any(has_indexed_entity),
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxide_model::{PartitionId, PathElement};

    fn named(name: &str) -> Key {
        Key {
            partition: PartitionId::default(),
            path: vec![PathElement::new("K", Some(IdOrName::Name(name.into())))],
        }
    }

    fn id(id: i64) -> Key {
        Key {
            partition: PartitionId::default(),
            path: vec![PathElement::new("K", Some(IdOrName::Id(id)))],
        }
    }

    #[test]
    fn scatter_matches_java_emulator() {
        // Observed indexUpdates for key-only upserts: 3 when scattered, 1 otherwise.
        let names: Vec<bool> = "abcdefghij"
            .chars()
            .map(|c| scatter_value(&named(&c.to_string())).is_some())
            .collect();
        assert_eq!(
            names,
            [
                false, true, false, false, false, false, true, true, true, false
            ]
        );
        let ids: Vec<bool> = (1..=10).map(|i| scatter_value(&id(i)).is_some()).collect();
        assert_eq!(
            ids,
            [
                false, false, false, false, false, false, true, true, true, true
            ]
        );
    }

    #[test]
    fn unchanged_entity_costs_nothing() {
        let mut e = Entity::new(named("a"));
        e.properties
            .insert("x".into(), Value::new(ValueKind::Integer(1)));
        let form = Form {
            scatter: true,
            stripped: false,
        };
        assert_eq!(write_cost(Some((&e, form)), &e, true, &[]), 0);
        assert_eq!(write_cost(None, &e, true, &[]), 3);
    }
}
