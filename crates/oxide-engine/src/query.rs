//! Query execution, ported from App Engine's `LocalDatastoreService.runQuery` and the comparators
//! behind it (`NormalizedQuery`, `ValidatedQuery`, `BaseEntityComparator`, `FilterMatcher`;
//! Apache-2.0, see NOTICE).
//!
//! Pipeline: normalize → validate → candidates (kind, ancestor, partition) → filter (every filtered
//! or ordered property must have an indexed value) → split projections into value combinations →
//! stable sort with the adjusted orders → group-by (distinct) on consecutive rows → cursors →
//! offset / limit in batches of at most 300.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use oxide_model::{Entity, Key};

use crate::cost::{Form, SCATTER_PROPERTY, scatter_value};
use crate::cursor::Position;
use crate::index::{IndexValue, index_values_for, index_values_in_order, property_index_values};
use crate::{Error, Result};

/// Maximum results (and skipped results) per batch.
pub const MAX_BATCH: usize = 300;

pub const KEY_PROPERTY: &str = "__key__";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterOp {
    Equal,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

impl FilterOp {
    fn is_inequality(self) -> bool {
        self != FilterOp::Equal
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub property: String,
    pub op: FilterOp,
    pub value: IndexValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Ascending,
    Descending,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Order {
    pub property: String,
    pub direction: Direction,
}

/// A validated, engine-level query (all filters ANDed).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub project_id: String,
    pub database_id: String,
    pub namespace_id: String,
    pub kind: Option<String>,
    pub ancestor: Option<Key>,
    /// `HAS_PARENT`: only direct children of `ancestor` (or, without one, root entities).
    pub shallow: bool,
    pub filters: Vec<Filter>,
    /// Disjunctive normal form beyond `filters` (superset only): when not empty, a result must
    /// also match every filter of at least one branch.
    pub disjunction: Vec<Vec<Filter>>,
    /// Production's rules (superset): inequality filters on several properties, none of which has
    /// to be the first order; those not ordered explicitly are ordered after the explicit orders,
    /// by name, in the direction of the last explicit order.
    pub multiple_inequalities: bool,
    pub orders: Vec<Order>,
    pub projection: Vec<String>,
    pub keys_only: bool,
    pub group_by: Vec<String>,
    pub start_cursor: Option<Position>,
    pub end_cursor: Option<Position>,
    pub offset: usize,
    pub limit: Option<usize>,
    /// Return every row from the start cursor up to `offset + limit`, without skipping the offset
    /// or batching (the App Engine v3 API batches the snapshot itself).
    pub unbatched: bool,
}

/// One query result row.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    pub key: Key,
    /// The full entity (with its version) for non-projection, non-keys-only queries.
    pub entity: Option<(Entity, i64)>,
    /// Projected values, in projection order.
    pub projection: Vec<(String, IndexValue)>,
    pub cursor: Position,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueryBatch {
    pub results: Vec<QueryResult>,
    pub skipped_results: usize,
    pub skipped_cursor: Option<Position>,
    pub end_cursor: Position,
    /// True when the batch stopped at the 300 cap with more to come.
    pub more_results: bool,
    /// The project's read timestamp, reported for ancestor queries.
    pub snapshot_version: Option<i64>,
    /// Keys of every matching entity, in result order, before distinct, cursors, offset and
    /// limit (Firestore-mode transactions track their groups).
    pub matched: Vec<Key>,
}

fn bad_request(message: impl Into<String>) -> Error {
    Error::invalid_argument(message)
}

/// Inequality bounds on one property, merged to the tightest range.
#[derive(Clone, Debug, Default)]
struct Matcher {
    equals: Vec<IndexValue>,
    lower: Option<(IndexValue, bool)>, // (bound, inclusive)
    upper: Option<(IndexValue, bool)>,
}

impl Matcher {
    fn add(&mut self, op: FilterOp, value: IndexValue) {
        match op {
            FilterOp::Equal => self.equals.push(value),
            FilterOp::GreaterThan | FilterOp::GreaterThanOrEqual => {
                let inclusive = op == FilterOp::GreaterThanOrEqual;
                let tighter = match &self.lower {
                    None => true,
                    Some((v, inc)) => value > *v || (value == *v && *inc && !inclusive),
                };
                if tighter {
                    self.lower = Some((value, inclusive));
                }
            }
            FilterOp::LessThan | FilterOp::LessThanOrEqual => {
                let inclusive = op == FilterOp::LessThanOrEqual;
                let tighter = match &self.upper {
                    None => true,
                    Some((v, inc)) => value < *v || (value == *v && *inc && !inclusive),
                };
                if tighter {
                    self.upper = Some((value, inclusive));
                }
            }
        }
    }

    fn in_range(&self, v: &IndexValue) -> bool {
        let above = self
            .lower
            .as_ref()
            .is_none_or(|(b, inc)| if *inc { v >= b } else { v > b });
        let below = self
            .upper
            .as_ref()
            .is_none_or(|(b, inc)| if *inc { v <= b } else { v < b });
        above && below
    }

    /// A single value passes when it is in range and equals every equality value.
    fn accepts(&self, v: &IndexValue) -> bool {
        self.in_range(v) && self.equals.iter().all(|e| e == v)
    }

    /// Entity-level match: all equality values present and some value in range.
    fn matches(&self, values: &[IndexValue]) -> bool {
        self.equals.iter().all(|e| values.contains(e)) && values.iter().any(|v| self.in_range(v))
    }
}

/// The normalized query: filters grouped per property, and the adjusted sort orders.
struct Plan {
    matchers: BTreeMap<String, Matcher>,
    /// Per-branch matchers of a disjunctive query; a row must match one branch.
    branches: Vec<BTreeMap<String, Matcher>>,
    /// Adjusted orders, ending with `__key__` ascending.
    orders: Vec<Order>,
    first_descending: bool,
}

fn normalize_and_validate(q: &Query) -> Result<Plan> {
    // ValidatedQuery checks, in its order.
    if !q.projection.is_empty() && q.keys_only {
        return Err(bad_request("projection and keys_only cannot both be set"));
    }
    for (i, p) in q.projection.iter().enumerate() {
        if q.projection[..i].contains(p) {
            return Err(bad_request("cannot project a property multiple times"));
        }
    }
    for (i, p) in q.group_by.iter().enumerate() {
        if q.group_by[..i].contains(p) {
            return Err(bad_request("cannot group by a property multiple times"));
        }
        if p.len() >= 4 && p.starts_with("__") && p.ends_with("__") {
            return Err(bad_request(format!(
                "group by is not supported for the property: {p}"
            )));
        }
    }

    // NormalizedQuery: deduplicate equality filters.
    let mut filters: Vec<&Filter> = Vec::new();
    for f in &q.filters {
        if !(f.op == FilterOp::Equal
            && filters
                .iter()
                .any(|g| g.op == FilterOp::Equal && g.property == f.property && g.value == f.value))
        {
            filters.push(f);
        }
    }
    let has_equality = |p: &str| {
        filters
            .iter()
            .any(|f| f.property == p && f.op == FilterOp::Equal)
    };
    let has_inequality = |p: &str| {
        filters
            .iter()
            .any(|f| f.property == p && f.op.is_inequality())
    };
    let key_equality = filters
        .iter()
        .any(|f| f.property == KEY_PROPERTY && f.op == FilterOp::Equal);

    // Orders: drop equality-filtered properties (without inequality) and repeats; a `__key__ = k`
    // filter clears all orders; nothing after the first `__key__` order matters.
    let mut orders: Vec<Order> = Vec::new();
    if !key_equality {
        for o in &q.orders {
            if (has_equality(&o.property) && !has_inequality(&o.property))
                || orders.iter().any(|e| e.property == o.property)
            {
                continue;
            }
            let is_key = o.property == KEY_PROPERTY;
            orders.push(o.clone());
            if is_key {
                break;
            }
        }
    }

    // Group-by orderings must come first.
    if !q.group_by.is_empty() {
        let mut remaining: Vec<&String> = q.group_by.iter().collect();
        for o in &orders {
            if remaining.is_empty() {
                break;
            }
            if let Some(i) = remaining.iter().position(|g| **g == o.property) {
                remaining.remove(i);
            } else {
                return Err(bad_request(
                    "must specify all group by orderings before any non group by orderings",
                ));
            }
        }
    }

    if q.kind.is_none() {
        // Projected and grouped properties are implicit existence filters.
        let implicit = q
            .projection
            .iter()
            .chain(&q.group_by)
            .any(|p| p != KEY_PROPERTY);
        if implicit || filters.iter().any(|f| f.property != KEY_PROPERTY) {
            return Err(bad_request("kind is required for non-__key__ filters"));
        }
        if orders
            .iter()
            .any(|o| o.property != KEY_PROPERTY || o.direction == Direction::Descending)
        {
            return Err(bad_request(
                "kind is required for all orders except __key__ ascending",
            ));
        }
    }

    let mut inequality_property: Option<&str> = None;
    for f in filters.iter().filter(|_| !q.multiple_inequalities) {
        if f.op.is_inequality() {
            match inequality_property {
                None => inequality_property = Some(&f.property),
                Some(p) if p != f.property => {
                    return Err(bad_request(format!(
                        "Only one inequality filter per query is supported.  Encountered both {p} and {}",
                        f.property
                    )));
                }
                _ => {}
            }
        }
    }
    for p in &q.projection {
        if has_equality(p) {
            return Err(bad_request(
                "cannot use projection on a property with an equality filter",
            ));
        }
    }
    for p in &q.group_by {
        if has_equality(p) {
            return Err(bad_request(
                "cannot use group by on a property with an equality filter",
            ));
        }
    }
    let mut inequalities: Vec<&str> = filters
        .iter()
        .filter(|f| f.op.is_inequality())
        .map(|f| f.property.as_str())
        .collect();
    inequalities.sort();
    inequalities.dedup();
    if q.multiple_inequalities {
        for p in &inequalities {
            if !q.group_by.is_empty() && !q.group_by.iter().any(|g| g == p) {
                return Err(bad_request(format!(
                    "Inequality filter on {p} must also be a group by property when group by properties are set."
                )));
            }
        }
    }
    if let Some(p) = inequality_property {
        if !q.group_by.is_empty() && !q.group_by.iter().any(|g| g == p) {
            return Err(bad_request(format!(
                "Inequality filter on {p} must also be a group by property when group by properties are set."
            )));
        }
        if let Some(first) = orders.first().filter(|o| o.property != p) {
            return Err(bad_request(format!(
                "The first sort property must be the same as the property to which the inequality filter is applied.  In your query the first sort property is {} but the inequality filter is on {p}",
                first.property
            )));
        }
    }

    let mut matchers = build_matchers(filters.iter().copied());

    // Adjusted orders: explicit orders, else the first inequality property; then EXISTS orders for
    // projected / grouped properties without filters (by name); then `__key__` ascending.
    let mut adjusted = orders.clone();
    if q.multiple_inequalities {
        add_inequality_orders(&mut adjusted, &inequalities);
    } else if adjusted.is_empty()
        && let Some(p) = inequality_property
    {
        adjusted.push(Order {
            property: p.to_string(),
            direction: Direction::Ascending,
        });
    }
    let mut exists: Vec<&String> = q
        .projection
        .iter()
        .chain(&q.group_by)
        .filter(|p| *p != KEY_PROPERTY && !matchers.contains_key(*p))
        .collect();
    exists.sort();
    exists.dedup();
    for p in exists {
        if !adjusted.iter().any(|o| &o.property == p) {
            adjusted.push(Order {
                property: p.clone(),
                direction: Direction::Ascending,
            });
        }
        matchers.entry(p.clone()).or_default();
    }
    if adjusted
        .last()
        .is_none_or(|o| o.property != KEY_PROPERTY || o.direction != Direction::Ascending)
    {
        adjusted.push(Order {
            property: KEY_PROPERTY.into(),
            direction: Direction::Ascending,
        });
    }
    // Ordered properties must exist.
    for o in &adjusted {
        if o.property != KEY_PROPERTY {
            matchers.entry(o.property.clone()).or_default();
        }
    }
    let first_descending = adjusted
        .first()
        .is_some_and(|o| o.direction == Direction::Descending);
    Ok(Plan {
        matchers,
        branches: Vec::new(),
        orders: adjusted,
        first_descending,
    })
}

/// Orders the inequality properties that are not ordered yet, by name, in the direction of the
/// last order (ascending without one).
fn add_inequality_orders(orders: &mut Vec<Order>, inequalities: &[&str]) {
    let direction = orders
        .iter()
        .rfind(|o| o.property != KEY_PROPERTY)
        .map_or(Direction::Ascending, |o| o.direction);
    let at = orders
        .iter()
        .position(|o| o.property == KEY_PROPERTY)
        .unwrap_or(orders.len());
    let missing: Vec<Order> = inequalities
        .iter()
        .filter(|p| !orders.iter().any(|o| o.property == **p))
        .map(|p| Order {
            property: p.to_string(),
            direction,
        })
        .collect();
    orders.splice(at..at, missing);
}

fn build_matchers<'a>(filters: impl Iterator<Item = &'a Filter>) -> BTreeMap<String, Matcher> {
    let mut matchers: BTreeMap<String, Matcher> = BTreeMap::new();
    for f in filters {
        let value = match &f.value {
            IndexValue::Key(k) if f.property == KEY_PROPERTY => IndexValue::Key(query_key(k)),
            v => v.clone(),
        };
        matchers
            .entry(f.property.clone())
            .or_default()
            .add(f.op, value);
    }
    matchers
}

/// Plans a disjunctive query: each branch is validated as a query of its own (with the base
/// filters), all branches share at most one inequality property, which is the implicit first
/// order, and rows are ordered by the base query's orders.
fn normalize_disjunction(q: &Query) -> Result<Plan> {
    let mut inequality: Option<&str> = None;
    let mut inequalities: Vec<&str> = Vec::new();
    let mut branches = Vec::with_capacity(q.disjunction.len());
    for branch in &q.disjunction {
        let mut single = q.clone();
        single.disjunction.clear();
        single.filters.extend(branch.iter().cloned());
        normalize_and_validate(&single)?;
        for f in branch.iter().filter(|f| f.op.is_inequality()) {
            inequalities.push(&f.property);
            if q.multiple_inequalities {
                continue;
            }
            match inequality {
                None => inequality = Some(&f.property),
                Some(p) if p != f.property => {
                    return Err(bad_request(format!(
                        "Only one inequality filter per query is supported.  Encountered both {p} and {}",
                        f.property
                    )));
                }
                _ => {}
            }
        }
        branches.push(build_matchers(branch.iter()));
    }
    let mut base = q.clone();
    base.disjunction.clear();
    let mut plan = normalize_and_validate(&base)?;
    if q.multiple_inequalities {
        inequalities.sort();
        inequalities.dedup();
        let explicit = plan.orders.len();
        add_inequality_orders(&mut plan.orders, &inequalities);
        for o in &plan.orders[..] {
            if o.property != KEY_PROPERTY {
                plan.matchers.entry(o.property.clone()).or_default();
            }
        }
        if explicit != plan.orders.len() {
            plan.first_descending = plan
                .orders
                .first()
                .is_some_and(|o| o.direction == Direction::Descending);
        }
    } else if let Some(p) = inequality
        && !plan.orders.iter().any(|o| o.property == p)
    {
        plan.orders.insert(
            0,
            Order {
                property: p.to_string(),
                direction: Direction::Ascending,
            },
        );
        plan.matchers.entry(p.to_string()).or_default();
        plan.first_descending = false;
    }
    plan.branches = branches;
    Ok(plan)
}

/// `key` as queries compare it: Java's queries ignore the database (they see every database of
/// the project), so keys that differ only in database are equal and keep insertion order.
fn query_key(key: &Key) -> Key {
    let mut k = key.clone();
    k.partition.database_id.clear();
    k
}

/// A candidate row: an entity (or one value combination of it, for projections).
#[derive(Clone, Debug)]
struct Row {
    key: Key,
    entity: Option<(Entity, i64)>,
    values: BTreeMap<String, Vec<IndexValue>>,
    /// True for rows made by a projection split, whose values are single index values.
    split: bool,
}

impl Plan {
    /// The value used to order `row` by `order`: min (asc) / max (desc) of the values in range.
    fn sort_value(&self, row: &Row, order: &Order) -> Option<IndexValue> {
        if order.property == KEY_PROPERTY {
            return Some(IndexValue::Key(query_key(&row.key)));
        }
        let values = row.values.get(&order.property)?;
        let matcher = self.matchers.get(&order.property);
        let in_some_branch = |v: &IndexValue| {
            let mut constrained = self
                .branches
                .iter()
                .filter_map(|b| b.get(&order.property))
                .peekable();
            constrained.peek().is_none() || constrained.any(|m| m.in_range(v))
        };
        let relevant = values
            .iter()
            .filter(|v| matcher.is_none_or(|m| m.in_range(v)) && in_some_branch(v));
        match order.direction {
            Direction::Ascending => relevant.min().cloned(),
            Direction::Descending => relevant.max().cloned(),
        }
    }

    fn compare(&self, a: &Row, b: &Row) -> Ordering {
        for order in &self.orders {
            if order.property == KEY_PROPERTY && (a.key.path.is_empty() || b.key.path.is_empty()) {
                return Ordering::Equal; // cursor positions without a key
            }
            let (Some(x), Some(y)) = (self.sort_value(a, order), self.sort_value(b, order)) else {
                return Ordering::Equal;
            };
            let ord = match order.direction {
                Direction::Ascending => x.cmp(&y),
                Direction::Descending => y.cmp(&x),
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }

    /// False if a filter on a plain top-level property (no `.`, not `__key__` or another special
    /// property) cannot match `entity`; true otherwise (then [`Plan::matches`] decides).
    fn may_match(&self, entity: &Entity) -> bool {
        let all = |matchers: &BTreeMap<String, Matcher>| {
            matchers.iter().all(|(name, m)| {
                if name.contains('.') || name.starts_with("__") {
                    return true;
                }
                match entity.properties.get(name) {
                    Some(v) if matches!(v.kind, oxide_model::ValueKind::Entity(_)) => true,
                    _ => m.matches(&property_index_values(entity, name)),
                }
            })
        };
        all(&self.matchers) && (self.branches.is_empty() || self.branches.iter().any(all))
    }

    fn matches(&self, row: &Row) -> bool {
        let all = |matchers: &BTreeMap<String, Matcher>| {
            matchers.iter().all(|(name, m)| {
                if name == KEY_PROPERTY {
                    m.matches(&[IndexValue::Key(query_key(&row.key))])
                } else {
                    row.values.get(name).is_some_and(|v| m.matches(v))
                }
            })
        };
        all(&self.matchers) && (self.branches.is_empty() || self.branches.iter().any(all))
    }

    /// The cursor position just after `row`.
    fn position(&self, row: &Row, group_by: &[String]) -> Position {
        let mut index_values = Vec::new();
        let names: Vec<&str> = if group_by.is_empty() {
            self.orders
                .iter()
                .map(|o| o.property.as_str())
                .filter(|p| *p != KEY_PROPERTY)
                .collect()
        } else {
            group_by.iter().map(String::as_str).collect()
        };
        // Full entities contribute every stored value, in stored order.
        let ordered = match (&row.entity, row.split) {
            (Some((entity, _)), false) => Some(index_values_in_order(entity)),
            _ => None,
        };
        let values = ordered.as_ref().unwrap_or(&row.values);
        for name in names {
            for v in values.get(name).into_iter().flatten() {
                index_values.push((name.to_string(), v.clone()));
            }
        }
        Position {
            index_values,
            key: group_by.is_empty().then(|| row.key.clone()),
            before: false,
            before_ascending: self.first_descending,
        }
    }

    /// Where a cursor falls in the sorted rows.
    fn locate(&self, rows: &[Row], cursor: &Position, kind: Option<&str>) -> Result<usize> {
        if let (Some(kind), Some(key)) = (kind, &cursor.key)
            && key.kind() != kind
        {
            return Err(bad_request(format!(
                "The query kind is {kind} but cursor.postfix_position.key kind is {}.",
                key.kind()
            )));
        }
        // The cursor's index values must be those of the query's orders.
        let mut names: Vec<&str> = Vec::new();
        for (name, _) in &cursor.index_values {
            if names.last() != Some(&name.as_str()) {
                names.push(name);
            }
        }
        let ordered: Vec<&str> = self
            .orders
            .iter()
            .map(|o| o.property.as_str())
            .filter(|p| *p != KEY_PROPERTY)
            .collect();
        if names != ordered {
            return Err(bad_request("cursor does not match query"));
        }
        let mut values: BTreeMap<String, Vec<IndexValue>> = BTreeMap::new();
        for (name, v) in &cursor.index_values {
            // Only the first value per name takes part in comparisons.
            values
                .entry(name.clone())
                .or_insert_with(|| vec![v.clone()]);
        }
        let probe = Row {
            key: cursor.key.clone().unwrap_or(Key {
                partition: Default::default(),
                path: Vec::new(),
            }),
            entity: None,
            values,
            split: true,
        };
        let inclusive = cursor.before_ascending ^ self.first_descending;
        Ok(match rows.binary_search_by(|r| self.compare(r, &probe)) {
            Ok(i) if inclusive => i,
            Ok(i) => i + 1,
            Err(i) => i,
        })
    }
}

/// Splits a row into one row per combination of single values of the postfix properties.
fn split_row(plan: &Plan, row: Row) -> Vec<Row> {
    let postfix: Vec<&str> = plan
        .orders
        .iter()
        .map(|o| o.property.as_str())
        .filter(|p| *p != KEY_PROPERTY)
        .collect();
    if postfix
        .iter()
        .all(|p| row.values.get(*p).is_none_or(|v| v.len() <= 1))
    {
        let mut row = row;
        row.values.retain(|k, _| postfix.contains(&k.as_str()));
        row.split = true;
        return vec![row];
    }
    let mut combos: Vec<BTreeMap<String, Vec<IndexValue>>> = vec![BTreeMap::new()];
    for p in postfix {
        let matcher = plan.matchers.get(p);
        let values: Vec<&IndexValue> = row
            .values
            .get(p)
            .into_iter()
            .flatten()
            .filter(|v| matcher.is_none_or(|m| m.accepts(v)))
            .collect();
        combos = combos
            .into_iter()
            .flat_map(|c| {
                values.iter().map(move |v| {
                    let mut c = c.clone();
                    c.insert(p.to_string(), vec![(*v).clone()]);
                    c
                })
            })
            .collect();
    }
    combos
        .into_iter()
        .map(|values| Row {
            key: row.key.clone(),
            entity: None,
            values,
            split: true,
        })
        .collect()
}

/// Runs `q` over `entities` (all of the project's stored entities with their versions and
/// stored forms: the hidden `__scatter__` value is indexed, and an export may have stripped the
/// embedded-entity index data).
pub fn run<'a>(
    q: &Query,
    entities: impl Iterator<Item = (&'a Key, &'a Entity, i64, Form)>,
    read_timestamp: i64,
) -> Result<QueryBatch> {
    let plan = if q.disjunction.is_empty() {
        normalize_and_validate(q)?
    } else {
        normalize_disjunction(q)?
    };
    let projecting = !q.projection.is_empty();

    let mut rows: Vec<Row> = entities
        .filter(|(key, _, _, _)| {
            key.partition.namespace_id == q.namespace_id
                && q.kind.as_ref().is_none_or(|k| key.kind() == k)
                && q.ancestor.as_ref().is_none_or(|a| {
                    key.path.len() >= a.path.len() && key.path[..a.path.len()] == a.path[..]
                })
                && (!q.shallow
                    || key.path.len() == q.ancestor.as_ref().map_or(0, |a| a.path.len()) + 1)
        })
        // Cheap check of plain property filters first: most entities fail it.
        .filter(|(_, entity, _, _)| plan.may_match(entity))
        .filter_map(|(key, entity, version, form)| {
            let mut values = index_values_for(entity, !form.stripped);
            if let Some(scatter) = (form.scatter && !form.stripped)
                .then(|| scatter_value(key))
                .flatten()
            {
                values.insert(
                    SCATTER_PROPERTY.to_string(),
                    vec![IndexValue::Bytes(scatter.to_vec())],
                );
            }
            let mut row = Row {
                key: key.clone(),
                entity: None,
                values,
                split: false,
            };
            // Only matching entities are copied.
            plan.matches(&row).then(|| {
                row.entity = Some((entity.clone(), version));
                row
            })
        })
        .collect();
    if projecting {
        rows = rows.into_iter().flat_map(|r| split_row(&plan, r)).collect();
    }
    rows.sort_by(|a, b| plan.compare(a, b));
    let mut matched: Vec<Key> = Vec::new();
    for row in &rows {
        if matched.last() != Some(&row.key) {
            matched.push(row.key.clone());
        }
    }

    if !q.group_by.is_empty() {
        let mut last: Option<Vec<(String, IndexValue)>> = None;
        rows.retain(|row| {
            let pairs: Vec<(String, IndexValue)> = q
                .group_by
                .iter()
                .flat_map(|g| {
                    row.values
                        .get(g)
                        .into_iter()
                        .flatten()
                        .map(move |v| (g.clone(), v.clone()))
                })
                .collect();
            let emit = last.as_ref().is_none_or(|l| {
                pairs
                    .iter()
                    .any(|p| !l.iter().any(|q| q.0 == p.0 && same_value(&q.1, &p.1)))
            });
            if emit {
                last = Some(pairs);
            }
            emit
        });
    }

    // Cursors. A cursor with no position counts as an unset start cursor, even as an end cursor.
    let kind = q.kind.as_deref();
    let end = match &q.end_cursor {
        None => rows.len(),
        Some(c) if c.is_empty() => 0,
        Some(c) => plan.locate(&rows, c, kind)?.min(rows.len()),
    };
    let start = match &q.start_cursor {
        None => 0,
        Some(c) if c.is_empty() => 0,
        Some(c) => plan.locate(&rows, c, kind)?,
    }
    .min(end);
    rows.truncate(end);
    let mut rows = rows.split_off(start);
    if let Some(limit) = q.limit {
        rows.truncate(limit.saturating_add(q.offset));
    }

    let (skip, take) = if q.unbatched {
        (0, rows.len())
    } else {
        let skip = q.offset.min(rows.len()).min(MAX_BATCH);
        let take = if skip < q.offset {
            0
        } else {
            (rows.len() - skip).min(MAX_BATCH)
        };
        (skip, take)
    };
    let skipped_cursor = (skip > 0).then(|| plan.position(&rows[skip - 1], &q.group_by));
    let consumed = skip + take;
    let more_results = consumed < rows.len();

    let start_position = || match &q.start_cursor {
        Some(c) => {
            let mut c = c.clone();
            c.before = false;
            c
        }
        None => Position::default(),
    };
    let end_cursor = match consumed {
        0 => start_position(),
        n => plan.position(&rows[n - 1], &q.group_by),
    };
    let results = rows[skip..consumed]
        .iter()
        .map(|row| QueryResult {
            key: row.key.clone(),
            entity: if projecting || q.keys_only {
                None
            } else {
                row.entity.clone()
            },
            projection: q
                .projection
                .iter()
                .filter(|p| *p != KEY_PROPERTY)
                .filter_map(|p| {
                    row.values
                        .get(p)
                        .and_then(|v| v.first())
                        .map(|v| (p.clone(), v.clone()))
                })
                .collect(),
            cursor: plan.position(row, &q.group_by),
        })
        .collect();
    Ok(QueryBatch {
        results,
        skipped_results: skip,
        skipped_cursor,
        end_cursor,
        more_results,
        snapshot_version: q.ancestor.as_ref().map(|_| read_timestamp),
        matched,
    })
}

/// The indexes a query would use (superset query explain): per disjunction, the equality-filtered
/// properties by name, then the query's adjusted orders (ending with `__key__`).
pub fn indexes_used(q: &Query) -> Result<Vec<Vec<(String, Direction)>>> {
    let plan = if q.disjunction.is_empty() {
        normalize_and_validate(q)?
    } else {
        normalize_disjunction(q)?
    };
    let branches: Vec<Vec<&Filter>> = if q.disjunction.is_empty() {
        vec![q.filters.iter().collect()]
    } else {
        q.disjunction
            .iter()
            .map(|b| q.filters.iter().chain(b).collect())
            .collect()
    };
    Ok(branches
        .into_iter()
        .map(|filters| {
            let mut equalities: Vec<&str> = filters
                .iter()
                .filter(|f| f.op == FilterOp::Equal && f.property != KEY_PROPERTY)
                .map(|f| f.property.as_str())
                .collect();
            equalities.sort();
            equalities.dedup();
            let mut index: Vec<(String, Direction)> = equalities
                .into_iter()
                .map(|p| (p.to_string(), Direction::Ascending))
                .collect();
            for o in &plan.orders {
                if !index.iter().any(|(p, _)| *p == o.property) {
                    index.push((o.property.clone(), o.direction));
                }
            }
            index
        })
        .collect())
}

/// Group-by compares stored values exactly (proto equality), not by index order.
fn same_value(a: &IndexValue, b: &IndexValue) -> bool {
    match (a, b) {
        (IndexValue::Double(x), IndexValue::Double(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}
