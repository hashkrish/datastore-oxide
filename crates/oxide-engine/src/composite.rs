//! Which composite index a query needs, ported from App Engine's `NormalizedQuery`,
//! `IndexComponentsOnlyQuery` and `CompositeIndexManager` (Apache-2.0, see NOTICE).
//!
//! A query is first rewritten the way App Engine normalizes its v3 form (deduplicated equality
//! filters, implicit existence filters for projected and grouped properties, trimmed orders), then
//! the natively supported parts (`__key__` filters, a trailing `__key__` ascending order) are
//! removed. What remains is both the query-history key and the input to index selection.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::query::{Direction, FilterOp, KEY_PROPERTY, Query};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexProperty {
    pub name: String,
    /// `None` only for properties read from an XML file without a `direction` attribute; such a
    /// property matches either direction but does not equal a generated one.
    pub direction: Option<Direction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompositeIndex {
    pub kind: String,
    pub ancestor: bool,
    pub properties: Vec<IndexProperty>,
}

/// Java `String.compareTo`: UTF-16 code unit order.
pub(crate) fn java_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// A set of names iterated in Java `TreeSet<String>` order.
fn java_sorted<'a>(names: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut v: Vec<String> = names
        .into_iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    v.sort_by(|a, b| java_cmp(a, b));
    v
}

#[derive(Clone, Debug, PartialEq)]
enum Op {
    Cmp(FilterOp),
    Exists,
}

#[derive(Clone, Debug, PartialEq)]
struct NFilter {
    property: String,
    op: Op,
    value: Option<crate::index::IndexValue>,
}

/// A query reduced to what matters for index selection (App Engine's `IndexComponentsOnlyQuery`).
#[derive(Debug)]
pub struct IndexQuery {
    /// Equality of the reduced query, as a string (App Engine compares the whole v3 query proto).
    pub key: String,
    kind: Option<String>,
    ancestor: bool,
    filter_count: usize,
    order_count: usize,
    /// Equality-filtered properties, in filter order.
    prefix: Vec<String>,
    /// Ordered properties; the implied order of an inequality property has no direction.
    orders: Vec<(String, Option<Direction>)>,
    group_by: Vec<String>,
    exists: Vec<String>,
    has_key_property: bool,
}

impl IndexQuery {
    pub fn new(q: &Query) -> Self {
        // NormalizedQuery.
        let mut filters: Vec<NFilter> = Vec::new();
        let mut equality: Vec<String> = Vec::new();
        let mut inequality: Vec<String> = Vec::new();
        for f in &q.filters {
            let nf = NFilter {
                property: f.property.clone(),
                op: Op::Cmp(f.op),
                value: Some(f.value.clone()),
            };
            if f.op == FilterOp::Equal {
                if !filters.contains(&nf) {
                    filters.push(nf);
                    if !equality.contains(&f.property) {
                        equality.push(f.property.clone());
                    }
                }
            } else {
                filters.push(nf);
                if !inequality.contains(&f.property) {
                    inequality.push(f.property.clone());
                }
            }
        }
        equality.retain(|p| !inequality.contains(p));
        let mut orders = Vec::new();
        for o in &q.orders {
            if !equality.contains(&o.property) {
                equality.push(o.property.clone());
                orders.push(o.clone());
            }
        }
        let mut all: Vec<String> = equality;
        all.extend(inequality);
        // A projection of only `__key__` is a keys-only query.
        let projection: &[String] = if q.projection.len() == 1 && q.projection[0] == KEY_PROPERTY {
            &[]
        } else {
            &q.projection
        };
        for p in projection.iter().chain(&q.group_by) {
            if !all.contains(p) {
                all.push(p.clone());
                filters.push(NFilter {
                    property: p.clone(),
                    op: Op::Exists,
                    value: None,
                });
            }
        }
        if filters
            .iter()
            .any(|f| f.op == Op::Cmp(FilterOp::Equal) && f.property == KEY_PROPERTY)
        {
            orders.clear();
        }
        if let Some(i) = orders.iter().position(|o| o.property == KEY_PROPERTY) {
            orders.truncate(i + 1);
        }

        // removeNativelySupportedComponents.
        if !filters.iter().any(|f| f.op == Op::Exists) {
            let mut key_desc = false;
            if let Some(last) = orders.last().filter(|o| o.property == KEY_PROPERTY) {
                if last.direction == Direction::Ascending {
                    orders.pop();
                } else {
                    key_desc = true;
                }
            }
            if !key_desc {
                let non_key_inequality = filters.iter().any(|f| {
                    matches!(f.op, Op::Cmp(op) if op != FilterOp::Equal)
                        && f.property != KEY_PROPERTY
                });
                if !non_key_inequality {
                    filters.retain(|f| f.property != KEY_PROPERTY);
                }
            }
        }

        // categorizeQuery.
        let mut prefix = Vec::new();
        let mut exists = Vec::new();
        let mut ineq: Option<String> = None;
        let mut has_key_property = false;
        for f in &filters {
            match f.op {
                Op::Cmp(FilterOp::Equal) => prefix.push(f.property.clone()),
                Op::Exists => exists.push(f.property.clone()),
                Op::Cmp(_) => {
                    ineq.get_or_insert_with(|| f.property.clone());
                }
            }
            if f.property == KEY_PROPERTY {
                has_key_property = true;
            }
        }
        let mut order_props: Vec<(String, Option<Direction>)> = Vec::new();
        if orders.is_empty()
            && let Some(p) = ineq
        {
            order_props.push((p, None));
        }
        let mut group_by: Vec<String> = q.group_by.clone();
        exists.retain(|p| !group_by.contains(p));
        for o in &orders {
            if o.property == KEY_PROPERTY {
                has_key_property = true;
            }
            group_by.retain(|g| *g != o.property);
            order_props.push((o.property.clone(), Some(o.direction)));
        }

        let key = format!(
            "{:?}",
            (
                (&q.project_id, &q.database_id, &q.namespace_id, &q.kind),
                &q.ancestor,
                &filters,
                &orders,
                (
                    projection,
                    &q.group_by,
                    q.keys_only || projection.len() < q.projection.len()
                ),
                (&q.start_cursor, &q.end_cursor, q.offset, q.limit),
            )
        );
        IndexQuery {
            key,
            kind: q.kind.clone(),
            ancestor: q.ancestor.is_some(),
            filter_count: filters.len(),
            order_count: orders.len(),
            prefix,
            orders: order_props,
            group_by: java_sorted(&group_by),
            exists: java_sorted(&exists),
            has_key_property,
        }
    }

    fn ascending(names: &[String]) -> impl Iterator<Item = IndexProperty> + '_ {
        names.iter().map(|n| IndexProperty {
            name: n.clone(),
            direction: Some(Direction::Ascending),
        })
    }

    /// The postfix components, in order: (ordered properties, unordered group-by, unordered exists).
    fn postfix(&self) -> [Component<'_>; 3] {
        [
            Component::Ordered(&self.orders),
            Component::Unordered(&self.group_by),
            Component::Unordered(&self.exists),
        ]
    }

    /// The index this query needs (`CompositeIndexManager.compositeIndexForQuery`), if any.
    pub fn composite_index(&self) -> Option<CompositeIndex> {
        if self.filter_count == 0 && self.order_count == 0 {
            return None;
        }
        let mut properties: Vec<IndexProperty> =
            Self::ascending(&java_sorted(&self.prefix)).collect();
        properties.extend(self.orders.iter().map(|(name, d)| IndexProperty {
            name: name.clone(),
            direction: Some(d.unwrap_or(Direction::Ascending)),
        }));
        properties.extend(Self::ascending(&self.group_by));
        properties.extend(Self::ascending(&self.exists));
        let has_kind = self.kind.is_some();
        if has_kind
            && !self.prefix.is_empty()
            && self.prefix.len() == self.filter_count
            && !self.has_key_property
            && self.order_count == 0
        {
            return None;
        }
        if has_kind
            && !self.ancestor
            && properties.len() <= 1
            && (!self.has_key_property || properties[0].direction == Some(Direction::Ascending))
        {
            return None;
        }
        Some(CompositeIndex {
            kind: self.kind.clone().unwrap_or_default(),
            ancestor: self.ancestor,
            properties,
        })
    }

    /// The smallest index that, together with `indexes`, would serve this query
    /// (`CompositeIndexManager.minimumCompositeIndexForQuery`); `None` when no index is needed.
    pub fn minimum_index(&self, indexes: &[CompositeIndex]) -> Option<CompositeIndex> {
        let suggested = self.composite_index()?;
        let kind = self.kind.clone().unwrap_or_default();
        // Postfix → (remaining equality properties, remaining ancestor), in first-seen order.
        let mut remaining: Vec<(&[IndexProperty], Vec<String>, bool)> = Vec::new();
        'indexes: for index in indexes {
            if index.kind != kind || (!self.ancestor && index.ancestor) {
                continue;
            }
            let mut split = index.properties.len();
            for component in self.postfix().iter().rev() {
                let size = component.size();
                if !component.matches(&index.properties[split.saturating_sub(size)..split]) {
                    continue 'indexes;
                }
                split = split.saturating_sub(size);
            }
            let mut index_eq = Vec::new();
            for p in &index.properties[..split] {
                if !self.prefix.contains(&p.name) {
                    continue 'indexes;
                }
                index_eq.push(p.name.clone());
            }
            let postfix = &index.properties[split..];
            let slot = remaining.iter().position(|(p, _, _)| *p == postfix);
            let (mut eq, mut anc) = match slot {
                Some(i) => (remaining[i].1.clone(), remaining[i].2),
                None => {
                    let mut eq = self.prefix.clone();
                    eq.dedup();
                    (eq, self.ancestor)
                }
            };
            let before = eq.len();
            eq.retain(|p| !index_eq.contains(p));
            let mut modified = eq.len() != before;
            if anc && index.ancestor {
                modified = true;
                anc = false;
            }
            if eq.is_empty() && !anc {
                return None;
            }
            if !modified {
                continue;
            }
            match slot {
                Some(i) => remaining[i] = (postfix, eq, anc),
                None => remaining.push((postfix, eq, anc)),
            }
        }
        let Some((postfix, eq, anc)) = remaining
            .iter()
            .min_by_key(|(_, eq, anc)| eq.len() + if *anc { 2 } else { 0 })
        else {
            return Some(suggested);
        };
        let mut properties: Vec<IndexProperty> = Self::ascending(&java_sorted(eq)).collect();
        properties.extend(postfix.iter().cloned());
        Some(CompositeIndex {
            kind,
            ancestor: *anc,
            properties,
        })
    }
}

enum Component<'a> {
    Ordered(&'a [(String, Option<Direction>)]),
    Unordered(&'a [String]),
}

impl Component<'_> {
    fn size(&self) -> usize {
        match self {
            Component::Ordered(p) => p.len(),
            Component::Unordered(p) => p.len(),
        }
    }

    /// Whether the tail of `index` (the slice ending where this component should end) matches.
    fn matches(&self, index: &[IndexProperty]) -> bool {
        match self {
            Component::Ordered(wanted) => {
                if index.len() < wanted.len() {
                    return false;
                }
                wanted
                    .iter()
                    .rev()
                    .zip(index.iter().rev())
                    .all(|((name, dir), p)| {
                        // An undirected constraint (an inequality's implied order) matches either
                        // direction; otherwise the property must be equal, including a set direction.
                        p.name == *name && (dir.is_none() || p.direction == *dir)
                    })
            }
            Component::Unordered(wanted) => {
                let mut left: Vec<&String> = wanted.iter().collect();
                for p in index.iter().rev() {
                    if left.is_empty() {
                        break;
                    }
                    match left.iter().position(|n| **n == p.name) {
                        Some(i) => {
                            left.remove(i);
                        }
                        None => return false,
                    }
                }
                left.is_empty()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexValue;
    use crate::query::{Filter, Order};

    fn order(p: &str, d: Direction) -> Order {
        Order {
            property: p.into(),
            direction: d,
        }
    }

    fn q(kind: &str) -> Query {
        Query {
            project_id: "p".into(),
            kind: Some(kind.into()),
            ..Query::default()
        }
    }

    fn names(i: &CompositeIndex) -> Vec<String> {
        i.properties
            .iter()
            .map(|p| match p.direction {
                Some(Direction::Descending) => format!("-{}", p.name),
                _ => p.name.clone(),
            })
            .collect()
    }

    #[test]
    fn computes_indexes() {
        let mut a = q("K");
        a.orders = vec![
            order("a", Direction::Ascending),
            order("b", Direction::Descending),
        ];
        assert_eq!(
            names(&IndexQuery::new(&a).composite_index().unwrap()),
            ["a", "-b"]
        );
        let mut b = q("K");
        b.filters = vec![Filter {
            property: "x".into(),
            op: FilterOp::Equal,
            value: IndexValue::Int(1),
        }];
        assert!(IndexQuery::new(&b).composite_index().is_none());
        b.orders = vec![order("a", Direction::Descending)];
        assert_eq!(
            names(&IndexQuery::new(&b).composite_index().unwrap()),
            ["x", "-a"]
        );
        let mut c = q("K");
        c.orders = vec![
            order("a", Direction::Ascending),
            order("__key__", Direction::Ascending),
        ];
        assert!(IndexQuery::new(&c).composite_index().is_none());
        let mut d = q("K");
        d.projection = vec!["b".into(), "a".into()];
        assert_eq!(
            names(&IndexQuery::new(&d).composite_index().unwrap()),
            ["a", "b"]
        );
    }
}
