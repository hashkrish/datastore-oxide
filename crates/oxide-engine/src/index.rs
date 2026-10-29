//! Index values: what a built-in index row stores for a property value, and their ordering.
//!
//! Values are compared the way the Java emulator's indexes order them (verified in
//! `tests/conformance/java/query_orders.golden` and `query_filters.golden`):
//! null (and empty arrays) < integers and timestamps (as microseconds) < booleans < strings and
//! blobs (as bytes) < doubles (NaN last) < geo points < keys. Embedded entities are not indexed
//! themselves; their properties are, under dotted names (`name.sub`, plus `name.__key__`).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use oxide_model::{Entity, Key, Value, ValueKind};

#[derive(Clone, Debug)]
pub enum IndexValue {
    Null,
    Int(i64),
    Bool(bool),
    Bytes(Vec<u8>),
    Double(f64),
    Point { latitude: f64, longitude: f64 },
    Key(Key),
}

impl IndexValue {
    fn rank(&self) -> u8 {
        match self {
            IndexValue::Null => 0,
            IndexValue::Int(_) => 1,
            IndexValue::Bool(_) => 2,
            IndexValue::Bytes(_) => 3,
            IndexValue::Double(_) => 4,
            IndexValue::Point { .. } => 5,
            IndexValue::Key(_) => 6,
        }
    }

    /// The index value of a single (non-array, non-entity) value.
    pub fn from_value(value: &Value) -> Option<IndexValue> {
        Some(match &value.kind {
            ValueKind::Null => IndexValue::Null,
            ValueKind::Boolean(b) => IndexValue::Bool(*b),
            ValueKind::Integer(i) | ValueKind::Timestamp(i) => IndexValue::Int(*i),
            ValueKind::Double(d) => IndexValue::Double(*d),
            ValueKind::String(s) => IndexValue::Bytes(s.as_bytes().to_vec()),
            ValueKind::Blob(b) => IndexValue::Bytes(b.clone()),
            ValueKind::GeoPoint {
                latitude,
                longitude,
            } => IndexValue::Point {
                latitude: *latitude,
                longitude: *longitude,
            },
            ValueKind::Key(k) => IndexValue::Key(k.clone()),
            ValueKind::Array(a) if a.is_empty() => IndexValue::Null,
            ValueKind::Array(_) | ValueKind::Entity(_) => return None,
        })
    }
}

/// Doubles order with NaN after every other value.
fn cmp_f64(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => a.partial_cmp(&b).expect("not NaN"),
    }
}

impl Ord for IndexValue {
    fn cmp(&self, other: &Self) -> Ordering {
        use IndexValue::*;
        match (self, other) {
            (Int(a), Int(b)) => a.cmp(b),
            (Bool(a), Bool(b)) => a.cmp(b),
            (Bytes(a), Bytes(b)) => a.cmp(b),
            (Double(a), Double(b)) => cmp_f64(*a, *b),
            (
                Point {
                    latitude: a,
                    longitude: b,
                },
                Point {
                    latitude: c,
                    longitude: d,
                },
            ) => cmp_f64(*a, *c).then_with(|| cmp_f64(*b, *d)),
            (Key(a), Key(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for IndexValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for IndexValue {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for IndexValue {}

/// Distinct indexed values of every property of `entity`, sorted, by (dotted) property name.
pub fn index_values(entity: &Entity) -> BTreeMap<String, Vec<IndexValue>> {
    index_values_for(entity, true)
}

/// Like [`index_values`]; with `flatten` false (an entity whose embedded-entity index data an
/// export stripped), embedded entities contribute nothing.
pub fn index_values_for(entity: &Entity, flatten: bool) -> BTreeMap<String, Vec<IndexValue>> {
    let mut out: BTreeMap<String, Vec<IndexValue>> = BTreeMap::new();
    for (name, value) in &entity.properties {
        add(&mut out, name, value, flatten);
    }
    for values in out.values_mut() {
        values.sort();
        values.dedup();
    }
    out
}

/// Indexed values of the top-level property `name` (not a dotted embedded-entity path), unsorted.
pub fn property_index_values(entity: &Entity, name: &str) -> Vec<IndexValue> {
    let mut out = BTreeMap::new();
    if let Some(value) = entity.properties.get(name)
        && !matches!(value.kind, ValueKind::Entity(_))
    {
        add(&mut out, name, value, true);
    }
    out.into_values().next().unwrap_or_default()
}

/// Indexed values of `entity` in stored order (array element order, duplicates kept), as the
/// Java emulator writes them into cursors.
pub fn index_values_in_order(entity: &Entity) -> BTreeMap<String, Vec<IndexValue>> {
    let mut out: BTreeMap<String, Vec<IndexValue>> = BTreeMap::new();
    for (name, value) in &entity.properties {
        add(&mut out, name, value, true);
    }
    out
}

fn add(out: &mut BTreeMap<String, Vec<IndexValue>>, name: &str, value: &Value, flatten: bool) {
    if value.exclude_from_indexes {
        return;
    }
    match &value.kind {
        ValueKind::Array(values) if !values.is_empty() => {
            for v in values {
                add(out, name, v, flatten);
            }
        }
        ValueKind::Entity(_) if !flatten => {}
        ValueKind::Entity(entity) => {
            if let Some(key) = &entity.key {
                out.entry(format!("{name}.__key__"))
                    .or_default()
                    .push(IndexValue::Key(key.clone()));
            }
            for (sub, v) in &entity.properties {
                add(out, &format!("{name}.{sub}"), v, true);
            }
        }
        _ => {
            if let Some(v) = IndexValue::from_value(value) {
                out.entry(name.to_string()).or_default().push(v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_type_order() {
        let mut values = [
            IndexValue::Double(f64::NAN),
            IndexValue::Bytes(b"abc".to_vec()),
            IndexValue::Bool(false),
            IndexValue::Int(5),
            IndexValue::Null,
            IndexValue::Double(-0.5),
            IndexValue::Point {
                latitude: 1.0,
                longitude: 2.0,
            },
        ];
        values.sort();
        let ranks: Vec<u8> = values.iter().map(IndexValue::rank).collect();
        assert_eq!(ranks, [0, 1, 2, 3, 4, 4, 5]);
        assert!(matches!(values[5], IndexValue::Double(d) if d.is_nan()));
    }
}
