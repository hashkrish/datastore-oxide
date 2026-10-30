//! Interned strings for the names that repeat across entities: kinds, property names, project,
//! database and namespace ids. Every copy of a name shares one allocation.

use std::borrow::Borrow;
use std::collections::HashSet;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::ops::Deref;
use std::sync::{Arc, LazyLock, RwLock};

/// A shared, immutable string that compares, orders and hashes like the `str` it holds.
#[derive(Clone)]
pub struct Name(Arc<str>);

struct Interner {
    names: HashSet<Arc<str>>,
    /// Names no longer in use are dropped when the set grows past this size.
    sweep_at: usize,
}

/// Shards, so threads converting requests at the same time rarely wait for each other; a name
/// already interned only needs a read lock.
const SHARDS: usize = 16;

struct Shards {
    hasher: RandomState,
    shards: [RwLock<Interner>; SHARDS],
}

static INTERNER: LazyLock<Shards> = LazyLock::new(|| Shards {
    hasher: RandomState::new(),
    shards: std::array::from_fn(|_| {
        RwLock::new(Interner {
            names: HashSet::new(),
            sweep_at: 1024,
        })
    }),
});

static EMPTY: LazyLock<Name> = LazyLock::new(|| Name(Arc::from("")));

impl Name {
    pub fn new(s: &str) -> Self {
        if s.is_empty() {
            return EMPTY.clone();
        }
        let shard = &INTERNER.shards[INTERNER.hasher.hash_one(s) as usize % SHARDS];
        if let Some(name) = shard.read().unwrap_or_else(|p| p.into_inner()).names.get(s) {
            return Name(name.clone());
        }
        let mut interner = shard.write().unwrap_or_else(|p| p.into_inner());
        if let Some(name) = interner.names.get(s) {
            return Name(name.clone());
        }
        if interner.names.len() >= interner.sweep_at {
            // Only the set itself holds names with a count of one.
            interner.names.retain(|n| Arc::strong_count(n) > 1);
            interner.sweep_at = (interner.names.len() * 2).max(1024);
        }
        let name: Arc<str> = Arc::from(s);
        interner.names.insert(name.clone());
        Name(name)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Name {
    fn default() -> Self {
        EMPTY.clone()
    }
}

impl Deref for Name {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Name {
    fn from(s: &str) -> Self {
        Name::new(s)
    }
}

impl From<String> for Name {
    fn from(s: String) -> Self {
        Name::new(&s)
    }
}

impl From<&String> for Name {
    fn from(s: &String) -> Self {
        Name::new(s)
    }
}

impl From<Name> for String {
    fn from(n: Name) -> Self {
        n.0.to_string()
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || *self.0 == *other.0
    }
}

impl Eq for Name {}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for Name {
    fn eq(&self, other: &String) -> bool {
        *self.0 == **other
    }
}

impl PartialEq<Name> for str {
    fn eq(&self, other: &Name) -> bool {
        self == &*other.0
    }
}

impl PartialEq<Name> for &str {
    fn eq(&self, other: &Name) -> bool {
        *self == &*other.0
    }
}

impl PartialEq<Name> for String {
    fn eq(&self, other: &Name) -> bool {
        **self == *other.0
    }
}

impl PartialOrd for Name {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Name {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl Hash for Name {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // As `str` hashes, so `Borrow<str>` lookups work.
        (*self.0).hash(state)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.0, f)
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_names_share_one_allocation() {
        let a = Name::new("kind-a");
        let b = Name::from(String::from("kind-a"));
        assert!(Arc::ptr_eq(&a.0, &b.0));
        assert_eq!(a, "kind-a");
        assert!(Name::new("a") < Name::new("b"));
        assert!(Name::new("B") < Name::new("a"));
    }
}
