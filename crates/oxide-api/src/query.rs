//! `RunQuery`: v1 query validation/conversion (with the Java emulator's v1-layer messages, see
//! `tests/conformance/java/query_*.golden`) and result conversion.

use oxide_engine::cursor::{self, Position};
use oxide_engine::index::IndexValue;
use oxide_engine::query::{Direction, Filter, FilterOp, KEY_PROPERTY, Order, Query, QueryBatch};
use oxide_model::Key;
use oxide_proto::google::datastore::v1 as pb;
use pb::value::ValueType;
use tonic::Status;

use crate::convert::{self, KeyUse, Scope, invalid};

/// Meaning the Java emulator puts on projected (index) values.
const INDEX_VALUE_MEANING: i32 = 18;

/// The namespace a query runs in, validated like a key's partition.
pub fn query_namespace(
    partition: Option<&pb::PartitionId>,
    scope: &Scope,
) -> Result<String, Status> {
    // Reuse key validation by validating a dummy key in that partition.
    let probe = pb::Key {
        partition_id: partition.cloned(),
        path: vec![pb::key::PathElement {
            kind: "K".into(),
            id_type: Some(pb::key::path_element::IdType::Id(1)),
        }],
    };
    Ok(convert::key_from_proto(&probe, scope, KeyUse::Read)?
        .partition
        .namespace_id
        .to_string())
}

fn property_name(r: Option<&pb::PropertyReference>) -> Result<String, Status> {
    let name = r.map(|r| r.name.clone()).unwrap_or_default();
    if name.is_empty() {
        return Err(invalid("The property.name is the empty string."));
    }
    Ok(name)
}

pub fn decode_cursor(bytes: &[u8]) -> Result<Option<Position>, Status> {
    if bytes.is_empty() {
        return Ok(None);
    }
    cursor::decode(bytes)
        .map(Some)
        .map_err(|_| invalid("Error parsing protocol message"))
}

/// Converts a structured v1 query. With `superset`, filters may also use production's `OR`,
/// multi-value `IN`, `NOT_IN` and `NOT_EQUAL` (see [`add_disjunctive_filter`]).
pub fn query_from_proto(
    q: &pb::Query,
    scope: &Scope,
    namespace: String,
    superset: bool,
) -> Result<Query, Status> {
    let mut out = Query {
        project_id: scope.project_id.clone(),
        database_id: scope.database_id.clone(),
        namespace_id: namespace,
        ..Default::default()
    };
    match q.kind.as_slice() {
        [] => {}
        [k] => {
            if k.name.is_empty() {
                return Err(invalid("The kind is the empty string."));
            }
            if k.name.len() > 1500 {
                return Err(invalid("The kind is longer than 1500 bytes."));
            }
            out.kind = Some(k.name.clone());
        }
        _ => return Err(invalid("multiple kinds not supported")),
    }
    out.multiple_inequalities = superset;
    if let Some(filter) = &q.filter {
        if superset {
            add_disjunctive_filter(filter, scope, &mut out)?;
        } else {
            check_filter_counts(filter)?;
            add_filter(filter, scope, &mut out)?;
        }
    }
    for o in &q.order {
        let property = property_name(o.property.as_ref())?;
        let direction = match o.direction() {
            pb::property_order::Direction::Descending => Direction::Descending,
            _ => Direction::Ascending,
        };
        out.orders.push(Order {
            property,
            direction,
        });
    }
    let projected: Vec<String> = q
        .projection
        .iter()
        .map(|p| property_name(p.property.as_ref()))
        .collect::<Result<_, _>>()?;
    if !projected.is_empty() && projected.iter().all(|p| p == KEY_PROPERTY) {
        out.keys_only = true;
    } else {
        out.projection = projected
            .into_iter()
            .filter(|p| p != KEY_PROPERTY)
            .collect();
    }
    out.group_by = q.distinct_on.iter().map(|p| p.name.clone()).collect();
    if let Some(offset) = (q.offset != 0).then_some(q.offset) {
        out.offset =
            usize::try_from(offset).map_err(|_| invalid("Offset must be non-negative."))?;
    }
    if let Some(limit) = &q.limit {
        out.limit =
            Some(usize::try_from(*limit).map_err(|_| invalid("Limit must be non-negative."))?);
    }
    out.start_cursor = decode_cursor(&q.start_cursor)?;
    out.end_cursor = decode_cursor(&q.end_cursor)?;
    Ok(out)
}

/// The v1 layer counts IN / NOT_IN / NOT_EQUAL across the whole filter before converting it.
fn check_filter_counts(filter: &pb::Filter) -> Result<(), Status> {
    fn walk(f: &pb::Filter, ops: &mut Vec<pb::property_filter::Operator>) {
        match &f.filter_type {
            Some(pb::filter::FilterType::CompositeFilter(c)) => {
                c.filters.iter().for_each(|f| walk(f, ops))
            }
            Some(pb::filter::FilterType::PropertyFilter(p)) => ops.push(p.op()),
            None => {}
        }
    }
    use pb::property_filter::Operator::*;
    let mut ops = Vec::new();
    walk(filter, &mut ops);
    if ops.iter().filter(|o| matches!(o, NotEqual | NotIn)).count() > 1 {
        return Err(invalid(
            "Only a single 'NOT_EQUAL' or 'NOT_IN' is allowed per query.",
        ));
    }
    if ops.iter().filter(|o| matches!(o, In | NotIn)).count() > 1 {
        return Err(invalid(
            "Only a single 'IN' or 'NOT_IN' filter allowed per query.",
        ));
    }
    Ok(())
}

/// `PropertyFilter.Operator.HAS_PARENT` in the Java emulator's protos.
const HAS_PARENT: i32 = 12;

fn add_filter(filter: &pb::Filter, scope: &Scope, out: &mut Query) -> Result<(), Status> {
    use pb::property_filter::Operator;
    match &filter.filter_type {
        None => Err(invalid("a filter must have exactly one of its fields set")),
        Some(pb::filter::FilterType::CompositeFilter(c)) => {
            if c.filters.is_empty() {
                return Err(invalid(
                    "A composite filter must have at least one sub-filter.",
                ));
            }
            match c.op() {
                pb::composite_filter::Operator::And => {}
                pb::composite_filter::Operator::Or => {
                    return Err(invalid("unsupported composite property operator"));
                }
                _ => return Err(invalid("a composite filter must specify an operator")),
            }
            c.filters.iter().try_for_each(|f| add_filter(f, scope, out))
        }
        Some(pb::filter::FilterType::PropertyFilter(p)) => {
            let property = property_name(p.property.as_ref())?;
            // HAS_PARENT (12): in Java's protos only, so it arrives as a raw value.
            if p.op == HAS_PARENT {
                let parent = match p.value.as_ref().and_then(|v| v.value_type.as_ref()) {
                    Some(ValueType::KeyValue(k)) => {
                        Some(convert::key_from_proto(k, scope, KeyUse::Value)?)
                    }
                    Some(ValueType::NullValue(_)) => None,
                    _ => return Err(invalid("HAS_PARENT requires a key value or null")),
                };
                if property != KEY_PROPERTY {
                    return Err(invalid("property must be __key__"));
                }
                if out.ancestor.is_some() || out.shallow {
                    return Err(invalid("duplicate ancestor or parent constraint"));
                }
                out.ancestor = parent;
                out.shallow = true;
                return Ok(());
            }
            let op = match p.op() {
                Operator::Unspecified => {
                    return Err(invalid("a property filter must specify an operator"));
                }
                Operator::LessThan => FilterOp::LessThan,
                Operator::LessThanOrEqual => FilterOp::LessThanOrEqual,
                Operator::GreaterThan => FilterOp::GreaterThan,
                Operator::GreaterThanOrEqual => FilterOp::GreaterThanOrEqual,
                Operator::Equal | Operator::In => FilterOp::Equal,
                Operator::NotEqual => return Err(Status::unknown("")),
                Operator::NotIn => return Err(invalid("Unrecognized PropertyFilter.op: NOT_IN")),
                Operator::HasAncestor => {
                    let Some(ValueType::KeyValue(k)) =
                        p.value.as_ref().and_then(|v| v.value_type.as_ref())
                    else {
                        return Err(invalid("HAS_ANCESTOR requires a key value"));
                    };
                    if property != KEY_PROPERTY {
                        return Err(invalid("property must be __key__"));
                    }
                    let key = convert::key_from_proto(k, scope, KeyUse::Value)?;
                    if out.shallow {
                        return Err(invalid("duplicate ancestor or parent constraint"));
                    }
                    if out.ancestor.is_some() {
                        return Err(invalid(
                            "All disjunctions must have a common and unique ancestor constraint.",
                        ));
                    }
                    if key.partition.namespace_id != out.namespace_id {
                        return Err(invalid(format!(
                            "The query namespace is {} but ancestor namespace is {}",
                            out.namespace_id, key.partition.namespace_id
                        )));
                    }
                    out.ancestor = Some(key);
                    return Ok(());
                }
            };
            if p.op() == Operator::In {
                let values = match p.value.as_ref().and_then(|v| v.value_type.as_ref()) {
                    Some(ValueType::ArrayValue(a)) if !a.values.is_empty() => &a.values,
                    _ => {
                        return Err(invalid(format!(
                            "'IN' property filter for '{property}' requires a non-empty ArrayValue"
                        )));
                    }
                };
                if values.len() > 1 {
                    return Err(invalid(format!(
                        "Filter has {} properties, expected 1",
                        values.len()
                    )));
                }
                let value = filter_value(&property, &values[0], scope)?;
                out.filters.push(Filter {
                    property,
                    op,
                    value,
                });
                return Ok(());
            }
            let value = filter_value(
                &property,
                p.value.as_ref().unwrap_or(&pb::Value::default()),
                scope,
            )?;
            out.filters.push(Filter {
                property,
                op,
                value,
            });
            Ok(())
        }
    }
}

/// Production's limit on disjunctions (branches after expanding `OR` and `IN`).
const MAX_DISJUNCTIONS: usize = 30;
/// Production's limit on `NOT_IN` values.
const MAX_NOT_IN_VALUES: usize = 10;

/// One AND-branch of a filter in disjunctive normal form, with its ancestor constraint
/// (`Some(None)` is `HAS_PARENT null`: root entities).
#[derive(Clone, Default)]
struct Branch {
    filters: Vec<Filter>,
    ancestor: Option<Option<Key>>,
    shallow: bool,
}

impl Branch {
    fn and(&self, other: &Branch) -> Result<Branch, Status> {
        let mut out = self.clone();
        out.filters.extend(other.filters.iter().cloned());
        if other.ancestor.is_some() {
            if out.ancestor.is_some() {
                return Err(invalid(if out.shallow || other.shallow {
                    "duplicate ancestor or parent constraint"
                } else {
                    "All disjunctions must have a common and unique ancestor constraint."
                }));
            }
            out.ancestor = other.ancestor.clone();
            out.shallow = other.shallow;
        }
        Ok(out)
    }
}

/// Production's filters (superset): the filter is expanded into disjunctive normal form. `IN`
/// becomes one branch per value; `NOT_EQUAL` and `NOT_IN` become the ranges between the excluded
/// values, which like production also excludes null. Every branch must have the same ancestor.
fn add_disjunctive_filter(
    filter: &pb::Filter,
    scope: &Scope,
    out: &mut Query,
) -> Result<(), Status> {
    check_superset_filter_counts(filter)?;
    let mut branches = dnf(filter, scope, out)?;
    let first = branches[0].clone();
    if branches
        .iter()
        .any(|b| b.ancestor != first.ancestor || b.shallow != first.shallow)
    {
        return Err(invalid(
            "All disjunctions must have a common and unique ancestor constraint.",
        ));
    }
    if let Some(ancestor) = first.ancestor {
        out.ancestor = ancestor;
        out.shallow = first.shallow;
    }
    if branches.len() == 1 {
        out.filters = branches.remove(0).filters;
    } else {
        out.disjunction = branches.into_iter().map(|b| b.filters).collect();
    }
    Ok(())
}

/// The per-query rules on production's operators.
fn check_superset_filter_counts(filter: &pb::Filter) -> Result<(), Status> {
    fn walk(f: &pb::Filter, ops: &mut Vec<pb::property_filter::Operator>, or: &mut bool) {
        match &f.filter_type {
            Some(pb::filter::FilterType::CompositeFilter(c)) => {
                *or |= c.op() == pb::composite_filter::Operator::Or;
                c.filters.iter().for_each(|f| walk(f, ops, or))
            }
            Some(pb::filter::FilterType::PropertyFilter(p)) => ops.push(p.op()),
            None => {}
        }
    }
    use pb::property_filter::Operator::*;
    let (mut ops, mut or) = (Vec::new(), false);
    walk(filter, &mut ops, &mut or);
    if ops.iter().filter(|o| matches!(o, NotEqual | NotIn)).count() > 1 {
        return Err(invalid(
            "Only a single 'NOT_EQUAL' or 'NOT_IN' is allowed per query.",
        ));
    }
    if ops.contains(&NotIn) && (or || ops.contains(&In)) {
        return Err(invalid(
            "'NOT_IN' cannot be used in the same query with 'OR', 'IN' or 'NOT_EQUAL' filters.",
        ));
    }
    Ok(())
}

fn too_many_disjunctions() -> Status {
    invalid(format!(
        "The query contains more than the maximum {MAX_DISJUNCTIONS} disjunctions."
    ))
}

fn dnf(filter: &pb::Filter, scope: &Scope, out: &Query) -> Result<Vec<Branch>, Status> {
    use pb::property_filter::Operator;
    let leaf = |filters: Vec<Filter>| Branch {
        filters,
        ..Default::default()
    };
    match &filter.filter_type {
        None => Err(invalid("a filter must have exactly one of its fields set")),
        Some(pb::filter::FilterType::CompositeFilter(c)) => {
            if c.filters.is_empty() {
                return Err(invalid(
                    "A composite filter must have at least one sub-filter.",
                ));
            }
            match c.op() {
                pb::composite_filter::Operator::And => {
                    let mut acc = vec![Branch::default()];
                    for f in &c.filters {
                        let sub = dnf(f, scope, out)?;
                        if acc.len() * sub.len() > MAX_DISJUNCTIONS {
                            return Err(too_many_disjunctions());
                        }
                        acc = acc
                            .iter()
                            .flat_map(|a| sub.iter().map(move |b| a.and(b)))
                            .collect::<Result<_, _>>()?;
                    }
                    Ok(acc)
                }
                pb::composite_filter::Operator::Or => {
                    let mut acc = Vec::new();
                    for f in &c.filters {
                        acc.extend(dnf(f, scope, out)?);
                        if acc.len() > MAX_DISJUNCTIONS {
                            return Err(too_many_disjunctions());
                        }
                    }
                    Ok(acc)
                }
                _ => Err(invalid("a composite filter must specify an operator")),
            }
        }
        Some(pb::filter::FilterType::PropertyFilter(p)) => {
            let property = property_name(p.property.as_ref())?;
            let value_type = p.value.as_ref().and_then(|v| v.value_type.as_ref());
            if p.op == HAS_PARENT || p.op() == Operator::HasAncestor {
                if property != KEY_PROPERTY {
                    return Err(invalid("property must be __key__"));
                }
                let shallow = p.op == HAS_PARENT;
                let key = match value_type {
                    Some(ValueType::KeyValue(k)) => {
                        Some(convert::key_from_proto(k, scope, KeyUse::Value)?)
                    }
                    Some(ValueType::NullValue(_)) if shallow => None,
                    _ if shallow => return Err(invalid("HAS_PARENT requires a key value or null")),
                    _ => return Err(invalid("HAS_ANCESTOR requires a key value")),
                };
                if let Some(k) = &key
                    && !shallow
                    && k.partition.namespace_id != out.namespace_id
                {
                    return Err(invalid(format!(
                        "The query namespace is {} but ancestor namespace is {}",
                        out.namespace_id, k.partition.namespace_id
                    )));
                }
                return Ok(vec![Branch {
                    filters: Vec::new(),
                    ancestor: Some(key),
                    shallow,
                }]);
            }
            let simple = |op| -> Result<Vec<Branch>, Status> {
                let value = filter_value(
                    &property,
                    p.value.as_ref().unwrap_or(&pb::Value::default()),
                    scope,
                )?;
                Ok(vec![leaf(vec![Filter {
                    property: property.clone(),
                    op,
                    value,
                }])])
            };
            let array = |name: &str| match value_type {
                Some(ValueType::ArrayValue(a)) if !a.values.is_empty() => Ok(&a.values),
                _ => Err(invalid(format!(
                    "'{name}' property filter for '{property}' requires a non-empty ArrayValue"
                ))),
            };
            match p.op() {
                Operator::Unspecified => Err(invalid("a property filter must specify an operator")),
                Operator::LessThan => simple(FilterOp::LessThan),
                Operator::LessThanOrEqual => simple(FilterOp::LessThanOrEqual),
                Operator::GreaterThan => simple(FilterOp::GreaterThan),
                Operator::GreaterThanOrEqual => simple(FilterOp::GreaterThanOrEqual),
                Operator::Equal => simple(FilterOp::Equal),
                Operator::In => {
                    let values = array("IN")?;
                    if values.len() > MAX_DISJUNCTIONS {
                        return Err(too_many_disjunctions());
                    }
                    values
                        .iter()
                        .map(|v| {
                            Ok(leaf(vec![Filter {
                                property: property.clone(),
                                op: FilterOp::Equal,
                                value: filter_value(&property, v, scope)?,
                            }]))
                        })
                        .collect()
                }
                Operator::NotEqual => {
                    let value = filter_value(
                        &property,
                        p.value.as_ref().unwrap_or(&pb::Value::default()),
                        scope,
                    )?;
                    Ok(excluding(&property, vec![value]))
                }
                Operator::NotIn => {
                    let values = array("NOT_IN")?;
                    if values.len() > MAX_NOT_IN_VALUES {
                        return Err(invalid(format!(
                            "'NOT_IN' supports up to {MAX_NOT_IN_VALUES} comparison values."
                        )));
                    }
                    let values = values
                        .iter()
                        .map(|v| filter_value(&property, v, scope))
                        .collect::<Result<_, _>>()?;
                    Ok(excluding(&property, values))
                }
                Operator::HasAncestor => unreachable!("handled above"),
            }
        }
    }
}

/// Branches matching a non-null value of `property` other than `values`: the open ranges
/// between the sorted excluded values.
fn excluding(property: &str, mut values: Vec<IndexValue>) -> Vec<Branch> {
    values.retain(|v| *v != IndexValue::Null);
    values.sort();
    values.dedup();
    let bound = |op, value: &IndexValue| Filter {
        property: property.to_string(),
        op,
        value: value.clone(),
    };
    let mut lower = IndexValue::Null;
    let mut branches = Vec::with_capacity(values.len() + 1);
    for v in &values {
        branches.push(Branch {
            filters: vec![
                bound(FilterOp::GreaterThan, &lower),
                bound(FilterOp::LessThan, v),
            ],
            ..Default::default()
        });
        lower = v.clone();
    }
    branches.push(Branch {
        filters: vec![bound(FilterOp::GreaterThan, &lower)],
        ..Default::default()
    });
    branches
}

fn filter_value(property: &str, value: &pb::Value, scope: &Scope) -> Result<IndexValue, Status> {
    let Some(value_type) = &value.value_type else {
        return Err(invalid(format!(
            "The value \"{property}\" does not contain a value."
        )));
    };
    match value_type {
        ValueType::ArrayValue(_) => return Err(invalid("A list value is not allowed")),
        ValueType::EntityValue(_) => return Err(invalid("An entity value is not allowed")),
        _ => {}
    }
    if property == KEY_PROPERTY && !matches!(value_type, ValueType::KeyValue(_)) {
        return Err(invalid("__key__ filter value must be a Key"));
    }
    let model = convert::single_value_from_proto(property, value, scope)?;
    Ok(IndexValue::from_value(&model).expect("scalar value"))
}

/// Converts an index value back into a v1 value, as projections return it.
pub fn index_value_to_proto(v: &IndexValue) -> pb::Value {
    let value_type = match v {
        IndexValue::Null => ValueType::NullValue(0),
        IndexValue::Int(i) => ValueType::IntegerValue(*i),
        IndexValue::Bool(b) => ValueType::BooleanValue(*b),
        IndexValue::Bytes(b) => match String::from_utf8(b.clone()) {
            Ok(s) => ValueType::StringValue(s),
            Err(_) => ValueType::BlobValue(b.clone().into()),
        },
        IndexValue::Double(d) => ValueType::DoubleValue(*d),
        IndexValue::Point {
            latitude,
            longitude,
        } => ValueType::GeoPointValue(oxide_proto::google::r#type::LatLng {
            latitude: *latitude,
            longitude: *longitude,
        }),
        IndexValue::Key(k) => ValueType::KeyValue(convert::key_to_proto(k)),
    };
    pb::Value {
        value_type: Some(value_type),
        meaning: INDEX_VALUE_MEANING,
        exclude_from_indexes: false,
    }
}

pub fn batch_to_proto(q: &Query, batch: QueryBatch) -> pb::QueryResultBatch {
    use pb::entity_result::ResultType;
    let result_type = if q.keys_only {
        ResultType::KeyOnly
    } else if !q.projection.is_empty() {
        ResultType::Projection
    } else {
        ResultType::Full
    };
    let key_only_entity = |key: &Key| pb::Entity {
        key: Some(convert::key_to_proto(key)),
        properties: Default::default(),
    };
    pb::QueryResultBatch {
        skipped_results: batch.skipped_results as i32,
        skipped_cursor: batch
            .skipped_cursor
            .as_ref()
            .map(cursor::encode)
            .unwrap_or_default()
            .into(),
        entity_result_type: result_type as i32,
        entity_results: batch
            .results
            .iter()
            .map(|r| {
                let (entity, version) = match (&r.entity, result_type) {
                    (Some((entity, version)), ResultType::Full) => {
                        (convert::entity_to_proto(entity), *version)
                    }
                    _ => {
                        let mut e = key_only_entity(&r.key);
                        for (name, v) in &r.projection {
                            e.properties.insert(name.clone(), index_value_to_proto(v));
                        }
                        (e, 0)
                    }
                };
                pb::EntityResult {
                    entity: Some(entity),
                    version,
                    cursor: cursor::encode(&r.cursor).into(),
                    ..Default::default()
                }
            })
            .collect(),
        end_cursor: cursor::encode(&batch.end_cursor).into(),
        more_results: if batch.more_results {
            pb::query_result_batch::MoreResultsType::NotFinished
        } else {
            pb::query_result_batch::MoreResultsType::MoreResultsAfterLimit
        } as i32,
        snapshot_version: batch.snapshot_version.unwrap_or_default(),
        read_time: None,
    }
}
