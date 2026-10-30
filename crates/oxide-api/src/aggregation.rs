//! `RunAggregationQuery` (superset only: the Java emulator answers UNIMPLEMENTED, see
//! docs/superset-backlog.md): validation of the aggregations and COUNT / SUM / AVG over the rows
//! of the nested query, with production Datastore's semantics:
//!
//! - COUNT counts the rows (after cursors, offset and limit), stopping at `up_to`.
//! - SUM adds the integer and double values of a property, ignoring anything else. The result is
//!   an integer while every value is one and the sum fits in 64 bits, a double otherwise, and 0
//!   when there is nothing to add.
//! - AVG is the mean of those values as a double, or null when there are none.
//!
//! Values come from the index, as in production: unindexed values and arrays are skipped.

use std::collections::HashSet;

use oxide_engine::query::QueryResult;
use oxide_model::{Entity, Value, ValueKind};
use oxide_proto::google::datastore::v1 as pb;
use pb::aggregation_query::aggregation::Operator;
use pb::value::ValueType;
use tonic::Status;

use crate::convert::invalid;

/// Production's limit on aggregations per query.
const MAX_AGGREGATIONS: usize = 5;

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Count { up_to: Option<i64> },
    Sum(String),
    Avg(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Aggregation {
    pub alias: String,
    pub op: Op,
}

impl Aggregation {
    fn property(&self) -> Option<&str> {
        match &self.op {
            Op::Count { .. } => None,
            Op::Sum(p) | Op::Avg(p) => Some(p),
        }
    }
}

/// Validates an aggregation query. Returns its nested query and the aggregations, with aliases
/// filled in (`property_1`, `property_2`, ... for unnamed ones, like production).
pub fn from_proto(q: &pb::AggregationQuery) -> Result<(&pb::Query, Vec<Aggregation>), Status> {
    let Some(pb::aggregation_query::QueryType::NestedQuery(nested)) = &q.query_type else {
        return Err(invalid("The aggregation query is missing a nested query."));
    };
    if q.aggregations.is_empty() {
        return Err(invalid("At least one aggregation is required."));
    }
    if q.aggregations.len() > MAX_AGGREGATIONS {
        return Err(invalid(format!(
            "Too many aggregations: {}, at most {MAX_AGGREGATIONS} are allowed.",
            q.aggregations.len()
        )));
    }
    if nested
        .projection
        .iter()
        .any(|p| p.property.as_ref().is_none_or(|p| p.name != "__key__"))
    {
        return Err(invalid(
            "Aggregation queries support no projection other than __key__.",
        ));
    }
    if !nested.distinct_on.is_empty() {
        return Err(invalid("Aggregation queries do not support DISTINCT ON."));
    }

    let mut used = HashSet::new();
    for a in &q.aggregations {
        if a.alias.is_empty() {
            continue;
        }
        if a.alias.starts_with("__") && a.alias.ends_with("__") {
            return Err(invalid(format!("The alias \"{}\" is reserved.", a.alias)));
        }
        if !used.insert(a.alias.as_str()) {
            return Err(invalid(format!("Duplicate alias \"{}\".", a.alias)));
        }
    }
    let mut next_default = 1;
    let mut default_alias = || loop {
        let alias = format!("property_{next_default}");
        next_default += 1;
        if !used.contains(alias.as_str()) {
            return alias;
        }
    };

    let mut out = Vec::with_capacity(q.aggregations.len());
    for a in &q.aggregations {
        let property = |r: &Option<pb::PropertyReference>| {
            r.as_ref()
                .map(|r| r.name.clone())
                .filter(|n| !n.is_empty())
                .ok_or_else(|| invalid("The property.name is the empty string."))
        };
        let op = match &a.operator {
            None => return Err(invalid("An aggregation must specify an operator.")),
            Some(Operator::Count(c)) => {
                if c.up_to.is_some_and(|n| n <= 0) {
                    return Err(invalid(format!(
                        "COUNT up_to must be greater than zero (received {}).",
                        c.up_to.unwrap_or_default()
                    )));
                }
                Op::Count { up_to: c.up_to }
            }
            Some(Operator::Sum(s)) => Op::Sum(property(&s.property)?),
            Some(Operator::Avg(s)) => Op::Avg(property(&s.property)?),
        };
        let alias = if a.alias.is_empty() {
            default_alias()
        } else {
            a.alias.clone()
        };
        out.push(Aggregation { alias, op });
    }
    Ok((nested, out))
}

/// Whether the rows need their entities (SUM or AVG), or keys are enough.
pub fn needs_entities(aggregations: &[Aggregation]) -> bool {
    aggregations.iter().any(|a| a.property().is_some())
}

/// Computes every aggregation over `rows`.
pub fn compute<'a>(
    aggregations: &[Aggregation],
    rows: impl Iterator<Item = &'a QueryResult> + Clone,
) -> pb::AggregationResult {
    let value = |v: ValueType| pb::Value {
        value_type: Some(v),
        ..Default::default()
    };
    let aggregate_properties = aggregations
        .iter()
        .map(|a| {
            let result = match &a.op {
                Op::Count { up_to } => {
                    let count = rows.clone().count() as i64;
                    ValueType::IntegerValue(up_to.map_or(count, |n| count.min(n)))
                }
                Op::Sum(p) => {
                    let s = Sum::over(p, rows.clone());
                    match s.int {
                        Some(i) if !s.saw_double => ValueType::IntegerValue(i),
                        _ => ValueType::DoubleValue(s.double),
                    }
                }
                Op::Avg(p) => {
                    let s = Sum::over(p, rows.clone());
                    if s.count == 0 {
                        ValueType::NullValue(0)
                    } else {
                        ValueType::DoubleValue(s.double / s.count as f64)
                    }
                }
            };
            (a.alias.clone(), value(result))
        })
        .collect();
    pb::AggregationResult {
        aggregate_properties,
    }
}

struct Sum {
    /// The exact integer sum, until a value overflows it.
    int: Option<i64>,
    double: f64,
    saw_double: bool,
    count: usize,
}

impl Sum {
    fn over<'a>(property: &str, rows: impl Iterator<Item = &'a QueryResult>) -> Self {
        let mut s = Sum {
            int: Some(0),
            double: 0.0,
            saw_double: false,
            count: 0,
        };
        for (entity, _) in rows.filter_map(|r| r.entity.as_ref()) {
            match lookup(entity, property).map(|v| &v.kind) {
                Some(ValueKind::Integer(i)) => {
                    s.int = s.int.and_then(|sum| sum.checked_add(*i));
                    s.double += *i as f64;
                }
                Some(ValueKind::Double(d)) => {
                    s.saw_double = true;
                    s.double += d;
                }
                _ => continue,
            }
            s.count += 1;
        }
        s
    }
}

/// The indexed value of `property`, which names a top-level property or, with dots, a property
/// of an embedded entity.
fn lookup<'e>(entity: &'e Entity, property: &str) -> Option<&'e Value> {
    let found = entity.properties.get(property).or_else(|| {
        let (head, rest) = property.split_once('.')?;
        match &entity.properties.get(head)?.kind {
            ValueKind::Entity(e) => lookup(e, rest),
            _ => None,
        }
    })?;
    (!found.exclude_from_indexes).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxide_engine::cursor::Position;
    use oxide_model::Key;

    fn agg(alias: &str, op: Operator) -> pb::aggregation_query::Aggregation {
        pb::aggregation_query::Aggregation {
            alias: alias.into(),
            operator: Some(op),
        }
    }

    fn count(up_to: Option<i64>) -> Operator {
        Operator::Count(pb::aggregation_query::aggregation::Count { up_to })
    }

    fn sum(p: &str) -> Operator {
        Operator::Sum(pb::aggregation_query::aggregation::Sum {
            property: Some(pb::PropertyReference { name: p.into() }),
        })
    }

    fn avg(p: &str) -> Operator {
        Operator::Avg(pb::aggregation_query::aggregation::Avg {
            property: Some(pb::PropertyReference { name: p.into() }),
        })
    }

    fn query(aggregations: Vec<pb::aggregation_query::Aggregation>) -> pb::AggregationQuery {
        pb::AggregationQuery {
            aggregations,
            query_type: Some(pb::aggregation_query::QueryType::NestedQuery(
                pb::Query::default(),
            )),
        }
    }

    fn row(values: &[(&str, ValueKind)]) -> QueryResult {
        let key = Key {
            partition: Default::default(),
            path: vec![],
        };
        let mut e = Entity::new(key.clone());
        for (name, v) in values {
            e.properties.insert((*name).into(), Value::new(v.clone()));
        }
        QueryResult {
            key,
            entity: Some((e, 1)),
            projection: vec![],
            cursor: Position::default(),
        }
    }

    fn results(
        aggs: Vec<pb::aggregation_query::Aggregation>,
        rows: &[QueryResult],
    ) -> Vec<(String, Option<ValueType>)> {
        let (_, aggs) = from_proto(&query(aggs)).unwrap();
        let mut out: Vec<_> = compute(&aggs, rows.iter())
            .aggregate_properties
            .into_iter()
            .map(|(k, v)| (k, v.value_type))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    #[test]
    fn default_aliases_skip_explicit_ones() {
        let (_, aggs) = from_proto(&query(vec![
            agg("", count(None)),
            agg("property_2", sum("n")),
            agg("", avg("n")),
        ]))
        .unwrap();
        let aliases: Vec<_> = aggs.iter().map(|a| a.alias.as_str()).collect();
        assert_eq!(aliases, ["property_1", "property_2", "property_3"]);
    }

    #[test]
    fn rejects_bad_aggregations() {
        let err = |aggs| from_proto(&query(aggs)).unwrap_err().message().to_string();
        assert_eq!(err(vec![]), "At least one aggregation is required.");
        assert_eq!(
            err(vec![agg("a", count(None)), agg("a", count(None))]),
            "Duplicate alias \"a\"."
        );
        assert_eq!(
            err(vec![agg("", count(Some(0)))]),
            "COUNT up_to must be greater than zero (received 0)."
        );
        assert_eq!(
            err(vec![agg("", sum(""))]),
            "The property.name is the empty string."
        );
        assert_eq!(
            err((0..6).map(|_| agg("", count(None))).collect()),
            "Too many aggregations: 6, at most 5 are allowed."
        );
    }

    #[test]
    fn count_sum_avg() {
        use ValueKind::*;
        let rows = [
            row(&[("n", Integer(1))]),
            row(&[("n", Integer(2)), ("d", Double(0.5))]),
            row(&[("n", String("x".into())), ("d", Integer(1))]),
            row(&[]),
        ];
        assert_eq!(
            results(
                vec![
                    agg("c", count(None)),
                    agg("c2", count(Some(2))),
                    agg("si", sum("n")),
                    agg("sd", sum("d")),
                    agg("ai", avg("n")),
                ],
                &rows
            ),
            [
                ("ai".into(), Some(ValueType::DoubleValue(1.5))),
                ("c".into(), Some(ValueType::IntegerValue(4))),
                ("c2".into(), Some(ValueType::IntegerValue(2))),
                ("sd".into(), Some(ValueType::DoubleValue(1.5))),
                ("si".into(), Some(ValueType::IntegerValue(3))),
            ]
        );
        assert_eq!(
            results(vec![agg("s", sum("x")), agg("a", avg("x"))], &rows),
            [
                ("a".into(), Some(ValueType::NullValue(0))),
                ("s".into(), Some(ValueType::IntegerValue(0))),
            ]
        );
    }

    #[test]
    fn sum_overflows_to_double() {
        let rows = [
            row(&[("n", ValueKind::Integer(i64::MAX))]),
            row(&[("n", ValueKind::Integer(1))]),
        ];
        assert_eq!(
            results(vec![agg("s", sum("n"))], &rows),
            [(
                "s".into(),
                Some(ValueType::DoubleValue(i64::MAX as f64 + 1.0))
            )]
        );
    }

    #[test]
    fn skips_unindexed_and_reads_embedded() {
        let mut r = row(&[]);
        let (e, _) = r.entity.as_mut().unwrap();
        e.properties.insert(
            "hidden".into(),
            Value {
                exclude_from_indexes: true,
                ..Value::new(ValueKind::Integer(5))
            },
        );
        let mut inner = Entity::default();
        inner
            .properties
            .insert("n".into(), Value::new(ValueKind::Integer(7)));
        e.properties
            .insert("e".into(), Value::new(ValueKind::Entity(Box::new(inner))));
        assert_eq!(
            results(vec![agg("h", sum("hidden")), agg("e", sum("e.n"))], &[r]),
            [
                ("e".into(), Some(ValueType::IntegerValue(7))),
                ("h".into(), Some(ValueType::IntegerValue(0))),
            ]
        );
    }
}
