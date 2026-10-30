//! An entity's properties by name, kept as a vector sorted by name.
//!
//! Entities usually have a few dozen properties at most, so a sorted vector looks names up as
//! fast as a map while storing each property once, without per-node slack. Iteration is in name
//! order (byte order, as for `String`), like the `BTreeMap` it replaces.

use std::fmt;
use std::ops::Index;

use crate::Value;

#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Properties {
    /// Sorted by name, names unique.
    entries: Vec<(String, Value)>,
}

impl Properties {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
        }
    }

    fn find(&self, name: &str) -> Result<usize, usize> {
        self.entries.binary_search_by(|(n, _)| n.as_str().cmp(name))
    }

    /// Sets `name` to `value`, returning the value it replaces.
    pub fn insert(&mut self, name: String, value: Value) -> Option<Value> {
        // Names usually arrive in order: appending needs no search.
        if self.entries.last().is_none_or(|(last, _)| *last < name) {
            self.entries.push((name, value));
            return None;
        }
        match self.find(&name) {
            Ok(i) => Some(std::mem::replace(&mut self.entries[i].1, value)),
            Err(i) => {
                self.entries.insert(i, (name, value));
                None
            }
        }
    }

    /// The value of `name`, inserting `default()` first if there is none.
    pub fn get_or_insert_with(
        &mut self,
        name: String,
        default: impl FnOnce() -> Value,
    ) -> &mut Value {
        let i = match self.find(&name) {
            Ok(i) => i,
            Err(i) => {
                self.entries.insert(i, (name, default()));
                i
            }
        };
        &mut self.entries[i].1
    }

    pub fn get(&self, name: impl AsRef<str>) -> Option<&Value> {
        self.find(name.as_ref()).ok().map(|i| &self.entries[i].1)
    }

    pub fn get_mut(&mut self, name: impl AsRef<str>) -> Option<&mut Value> {
        self.find(name.as_ref())
            .ok()
            .map(|i| &mut self.entries[i].1)
    }

    pub fn contains_key(&self, name: impl AsRef<str>) -> bool {
        self.find(name.as_ref()).is_ok()
    }

    pub fn remove(&mut self, name: impl AsRef<str>) -> Option<Value> {
        self.find(name.as_ref())
            .ok()
            .map(|i| self.entries.remove(i).1)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&str, &mut Value) -> bool) {
        self.entries.retain_mut(|(n, v)| keep(n, v));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&str, &Value)> + ExactSizeIterator {
        self.entries.iter().map(|(n, v)| (n.as_str(), v))
    }

    pub fn iter_mut(
        &mut self,
    ) -> impl DoubleEndedIterator<Item = (&str, &mut Value)> + ExactSizeIterator {
        self.entries.iter_mut().map(|(n, v)| (n.as_str(), v))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &str> + ExactSizeIterator {
        self.entries.iter().map(|(n, _)| n.as_str())
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &Value> + ExactSizeIterator {
        self.entries.iter().map(|(_, v)| v)
    }

    pub fn values_mut(
        &mut self,
    ) -> impl DoubleEndedIterator<Item = &mut Value> + ExactSizeIterator {
        self.entries.iter_mut().map(|(_, v)| v)
    }

    /// Drops spare capacity (for entities that are kept).
    pub fn shrink_to_fit(&mut self) {
        self.entries.shrink_to_fit();
    }
}

impl<Q: AsRef<str> + ?Sized> Index<&Q> for Properties {
    type Output = Value;

    fn index(&self, name: &Q) -> &Value {
        self.get(name.as_ref()).expect("no property with that name")
    }
}

impl fmt::Debug for Properties {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Later values win for repeated names, as with `BTreeMap`.
impl<N: Into<String>> FromIterator<(N, Value)> for Properties {
    fn from_iter<I: IntoIterator<Item = (N, Value)>>(iter: I) -> Self {
        let mut entries: Vec<(String, Value)> =
            iter.into_iter().map(|(n, v)| (n.into(), v)).collect();
        if !entries.is_sorted_by(|a, b| a.0 < b.0) {
            // Stable: equal names stay in arrival order, so the last one is kept.
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut deduped: Vec<(String, Value)> = Vec::with_capacity(entries.len());
            for entry in entries {
                match deduped.last_mut() {
                    Some(last) if last.0 == entry.0 => *last = entry,
                    _ => deduped.push(entry),
                }
            }
            entries = deduped;
        }
        Self { entries }
    }
}

impl<N: Into<String>> Extend<(N, Value)> for Properties {
    fn extend<I: IntoIterator<Item = (N, Value)>>(&mut self, iter: I) {
        for (n, v) in iter {
            self.insert(n.into(), v);
        }
    }
}

impl IntoIterator for Properties {
    type Item = (String, Value);
    type IntoIter = std::vec::IntoIter<(String, Value)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a Properties {
    type Item = (&'a str, &'a Value);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (String, Value)>,
        fn(&'a (String, Value)) -> (&'a str, &'a Value),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(n, v)| (n.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ValueKind;

    fn int(n: i64) -> Value {
        Value::new(ValueKind::Integer(n))
    }

    #[test]
    fn behaves_like_a_sorted_map() {
        let mut p = Properties::new();
        assert_eq!(p.insert("b".into(), int(1)), None);
        assert_eq!(p.insert("a".into(), int(2)), None);
        assert_eq!(p.insert("c".into(), int(3)), None);
        assert_eq!(p.insert("b".into(), int(4)), Some(int(1)));
        assert_eq!(p.keys().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(p["b"], int(4));
        assert_eq!(p.get("z"), None);
        assert_eq!(p.remove("a"), Some(int(2)));
        p.retain(|n, _| n != "c");
        assert_eq!(p.iter().collect::<Vec<_>>(), [("b", &int(4))]);
    }

    #[test]
    fn collecting_sorts_and_keeps_the_last_duplicate() {
        let p: Properties = [("b", int(1)), ("a", int(2)), ("b", int(3))]
            .into_iter()
            .collect();
        assert_eq!(
            p.iter().collect::<Vec<_>>(),
            [("a", &int(2)), ("b", &int(3))]
        );
        // Names in byte order, as `String`'s `Ord` (and the old `BTreeMap`) has them.
        let p: Properties = [("a", int(0)), ("B", int(0)), ("é", int(0)), ("_", int(0))]
            .into_iter()
            .collect();
        assert_eq!(p.keys().collect::<Vec<_>>(), ["B", "_", "a", "é"]);
    }
}
