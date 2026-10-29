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
        .namespace_id)
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

/// Converts a structured v1 query.
pub fn query_from_proto(q: &pb::Query, scope: &Scope, namespace: String) -> Result<Query, Status> {
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
    if let Some(filter) = &q.filter {
        check_filter_counts(filter)?;
        add_filter(filter, scope, &mut out)?;
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
