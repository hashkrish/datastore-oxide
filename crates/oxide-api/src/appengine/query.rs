//! App Engine v3 `RunQuery`/`Next`: queries run once, and their result snapshot is handed out in
//! batches.

use bytes::Bytes;
use oxide_engine::composite::{CompositeIndex, IndexQuery};
use oxide_engine::cursor::{self, Position};
use oxide_engine::index::IndexValue;
use oxide_engine::query::{Direction, Filter, FilterOp, KEY_PROPERTY, Order, Query, QueryResult};
use oxide_proto::apphosting_datastore_v3 as v3;
use oxide_proto::storage_onestore_v3 as os;
use prost::Message;

use super::entity::{self, INDEX_VALUE, text};
use super::{Fail, Service};

/// Default and maximum batch sizes.
const DEFAULT_COUNT: i32 = 20;
const MAX_COUNT: i32 = 300;

/// A query's result snapshot and how far it has been read.
pub struct LiveQuery {
    project: String,
    rows: Vec<QueryResult>,
    pos: usize,
    /// Results still to skip; negative offsets never skip nor return anything.
    remaining_offset: i32,
    count: Option<i32>,
    keys_only: bool,
    projection: bool,
    index: Option<CompositeIndex>,
}

pub struct Batch {
    pub result: v3::QueryResult,
    pub more: bool,
}

fn bad_request(message: impl Into<String>) -> Fail {
    Fail::App(1, message.into())
}

/// A compiled cursor as the engine's cursor position.
fn position(c: &v3::CompiledCursor) -> Option<Position> {
    cursor::decode(&c.encode_to_vec()).ok()
}

/// Converts a v3 query (app ids already mapped to project ids) into an engine query that returns
/// every row up to `offset + limit`. Also returns whether the query can match nothing at all (an
/// explicitly empty namespace, an incomplete ancestor).
pub fn convert(svc: &Service, q: &v3::Query, project: &str) -> Result<(Query, bool), Fail> {
    // With the superset, the caller filters the rows by the search words.
    if q.search_query.is_some() && !svc.superset {
        return Err(bad_request("full-text search unsupported"));
    }
    let namespace = q.name_space.as_deref().map(text).unwrap_or_default();
    let mut nothing = q.name_space.as_ref().is_some_and(Bytes::is_empty);
    let mut out = Query {
        project_id: project.to_string(),
        namespace_id: namespace.clone(),
        kind: q.kind.as_deref().map(text),
        keys_only: q.keys_only == Some(true),
        offset: q.offset.map_or(0, |o| usize::try_from(o).unwrap_or(0)),
        limit: q.limit.map(|l| usize::try_from(l).unwrap_or(0)),
        unbatched: true,
        multiple_inequalities: svc.superset,
        ..Default::default()
    };
    // Production's disjunctive filters (superset): IN with several values, NOT_EQUAL, NOT_IN.
    let mut disjunction: Vec<Vec<Filter>> = vec![Vec::new()];
    if let Some(ancestor) = &q.ancestor {
        let app = text(&ancestor.app);
        if app != project {
            return Err(bad_request(format!(
                "The query app is {project} but ancestor app is {app}"
            )));
        }
        let ancestor_ns = ancestor.name_space.as_deref().map(text).unwrap_or_default();
        if ancestor_ns != namespace {
            return Err(bad_request(format!(
                "The query namespace is {namespace} but ancestor namespace is {ancestor_ns}"
            )));
        }
        nothing |= ancestor.path.element.iter().any(entity::incomplete);
        out.ancestor = Some(entity::key_of(ancestor, project));
    }
    for f in &q.filter {
        if svc.superset && (matches!(f.op, 9 | 10) || (f.op == 6 && f.property.len() > 1)) {
            let branches = disjunctive_filter(f, project)?;
            if disjunction.len() * branches.len() > MAX_DISJUNCTIONS {
                return Err(bad_request(format!(
                    "The query contains more than the maximum {MAX_DISJUNCTIONS} disjunctions."
                )));
            }
            disjunction = disjunction
                .iter()
                .flat_map(|d| {
                    branches.iter().map(move |b| {
                        let mut d = d.clone();
                        d.extend(b.iter().cloned());
                        d
                    })
                })
                .collect();
            continue;
        }
        if matches!(f.op, 9 | 10) {
            return Err(Fail::Unknown);
        }
        if f.property.len() != 1 {
            return Err(bad_request(format!(
                "Filter has {} properties, expected 1",
                f.property.len()
            )));
        }
        let op = match f.op {
            1 => FilterOp::LessThan,
            2 => FilterOp::LessThanOrEqual,
            3 => FilterOp::GreaterThan,
            4 => FilterOp::GreaterThanOrEqual,
            5 | 6 => FilterOp::Equal,
            // EXISTS: no constraint.
            7 => continue,
            _ => return Err(Fail::Unknown),
        };
        let (property, value) = filter_value(&f.property[0], project)?;
        out.filters.push(Filter {
            property,
            op,
            value,
        });
    }
    if disjunction.len() > 1 {
        out.disjunction = disjunction;
    } else {
        out.filters.extend(disjunction.remove(0));
    }
    for o in &q.order {
        out.orders.push(Order {
            property: text(&o.property),
            direction: if o.direction == Some(2) {
                Direction::Descending
            } else {
                Direction::Ascending
            },
        });
    }
    for p in &q.property_name {
        let p = text(p);
        if p == KEY_PROPERTY {
            return Err(bad_request("__key__ filter value must be a Key"));
        }
        out.projection.push(p);
    }
    out.group_by = q.group_by_property_name.iter().map(|p| text(p)).collect();
    out.start_cursor = q.compiled_cursor.as_ref().and_then(position);
    out.end_cursor = q.end_compiled_cursor.as_ref().and_then(position);
    Ok((out, nothing))
}

/// Production's limit on disjunctions.
const MAX_DISJUNCTIONS: usize = 30;

/// A filter property's name and index value.
fn filter_value(p: &os::Property, project: &str) -> Result<(String, IndexValue), Fail> {
    let property = text(&p.name);
    let view = entity::view_of(&os::EntityProto {
        key: os::Reference {
            app: project.to_string().into(),
            ..Default::default()
        },
        property: vec![os::Property {
            multiple: false,
            ..p.clone()
        }],
        ..Default::default()
    });
    let value = view
        .properties
        .values()
        .next()
        .and_then(IndexValue::from_value);
    if property == KEY_PROPERTY && !matches!(value, Some(IndexValue::Key(_))) {
        return Err(bad_request("__key__ filter value must be a Key"));
    }
    Ok((property, value.unwrap_or(IndexValue::Null)))
}

/// `IN` with several values (one branch per value), `NOT_EQUAL` (9) and `NOT_IN` (10: the ranges
/// around the excluded values, never matching null), as the v1 API's superset does.
fn disjunctive_filter(f: &v3::query::Filter, project: &str) -> Result<Vec<Vec<Filter>>, Fail> {
    let values = f
        .property
        .iter()
        .map(|p| filter_value(p, project))
        .collect::<Result<Vec<_>, _>>()?;
    let Some((property, _)) = values.first().cloned() else {
        return Err(bad_request("Filter has 0 properties, expected 1"));
    };
    if values.iter().any(|(p, _)| *p != property) {
        return Err(bad_request(
            "A filter's values must all be for one property.",
        ));
    }
    let filter = |op, value: IndexValue| Filter {
        property: property.clone(),
        op,
        value,
    };
    if f.op == 6 {
        return Ok(values
            .into_iter()
            .map(|(_, v)| vec![filter(FilterOp::Equal, v)])
            .collect());
    }
    let mut excluded: Vec<IndexValue> = values
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| *v != IndexValue::Null)
        .collect();
    excluded.sort();
    excluded.dedup();
    let mut lower = IndexValue::Null;
    let mut branches = Vec::with_capacity(excluded.len() + 1);
    for v in excluded {
        branches.push(vec![
            filter(FilterOp::GreaterThan, lower.clone()),
            filter(FilterOp::LessThan, v.clone()),
        ]);
        lower = v;
    }
    branches.push(vec![filter(FilterOp::GreaterThan, lower)]);
    Ok(branches)
}

/// The engine index of a v3 index definition (`CreateIndex` and friends, superset).
pub fn index_from_definition(d: &os::Index) -> Option<CompositeIndex> {
    use os::index::property::Direction as D;
    use oxide_engine::composite::IndexProperty;
    let kind = text(&d.entity_type);
    if kind.is_empty() {
        return None;
    }
    let properties = d
        .property
        .iter()
        .map(|p| {
            let name = text(&p.name);
            (!name.is_empty()).then(|| IndexProperty {
                name,
                direction: Some(if p.direction == Some(D::Descending as i32) {
                    Direction::Descending
                } else {
                    Direction::Ascending
                }),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(CompositeIndex {
        kind,
        ancestor: d.ancestor,
        properties,
    })
}

/// The lowercase words of a text (full-text search, superset).
pub fn words(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The words of every string value of an entity, including arrays and embedded entities.
pub fn entity_words(e: &oxide_model::Entity) -> std::collections::HashSet<String> {
    fn collect(v: &oxide_model::Value, out: &mut std::collections::HashSet<String>) {
        match &v.kind {
            oxide_model::ValueKind::String(s) => out.extend(words(s)),
            oxide_model::ValueKind::Array(a) => a.iter().for_each(|v| collect(v, out)),
            oxide_model::ValueKind::Entity(e) => {
                e.properties.values().for_each(|v| collect(v, out))
            }
            _ => {}
        }
    }
    let mut out = std::collections::HashSet::new();
    e.properties.values().for_each(|v| collect(v, &mut out));
    out
}

impl LiveQuery {
    pub fn new(project: &str, q: &Query, rows: Vec<QueryResult>, v3: &v3::Query) -> Self {
        Self {
            project: project.to_string(),
            rows,
            pos: 0,
            remaining_offset: v3.offset.unwrap_or(0),
            count: v3.count,
            keys_only: q.keys_only,
            projection: !q.projection.is_empty(),
            index: IndexQuery::new(q).composite_index(),
        }
    }

    /// The next batch: skips what is left of the offset (at most 300), then returns up to
    /// `count` (else the query's count, else 20; at most 300) results.
    pub fn next(
        &mut self,
        svc: &Service,
        offset: Option<i32>,
        count: Option<i32>,
        compile: bool,
    ) -> Result<Batch, Fail> {
        if offset.is_some_and(|o| o != self.remaining_offset) {
            return Err(bad_request("offset mismatch"));
        }
        let count = count.or(self.count).unwrap_or(DEFAULT_COUNT);
        if count < 0 {
            return Err(Fail::Unknown);
        }
        let count = count.min(MAX_COUNT) as usize;
        let left = self.rows.len() - self.pos;
        let offset_set = self.remaining_offset > 0;
        let skip = if self.remaining_offset > 0 {
            (self.remaining_offset as usize)
                .min(MAX_COUNT as usize)
                .min(left)
        } else {
            0
        };
        self.remaining_offset -= skip as i32;
        let skipped = &self.rows[self.pos..self.pos + skip];
        let skipped_cursor = skipped.last().map(|r| r.cursor.clone());
        self.pos += skip;
        let take = if self.remaining_offset == 0 {
            count.min(self.rows.len() - self.pos)
        } else {
            0
        };
        let rows = &self.rows[self.pos..self.pos + take];
        self.pos += take;
        let more = self.pos < self.rows.len();

        let app = svc.resolver.to_app_id(&self.project);
        let mut result = v3::QueryResult {
            more_results: more,
            keys_only: Some(self.keys_only),
            index_only: Some(self.projection),
            skipped_results: offset_set.then_some(skip as i32),
            ..Default::default()
        };
        for row in rows {
            let key = entity::reference(&row.key, &app);
            let proto = if self.keys_only {
                os::EntityProto {
                    key,
                    ..Default::default()
                }
            } else if self.projection {
                os::EntityProto {
                    key,
                    property: row
                        .projection
                        .iter()
                        .map(|(name, v)| os::Property {
                            meaning: Some(INDEX_VALUE),
                            name: name.clone().into(),
                            multiple: false,
                            value: property_value(v, |k| entity::reference(k, &self.project)),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }
            } else {
                let (e, version) = row.entity.as_ref().expect("full results have entities");
                // Pseudo-kind results (version 0) have no version.
                if *version != 0 {
                    result.version.push(*version);
                }
                svc.read_proto(e, &app)?
            };
            result.result.push(proto);
        }
        if let Some(index) = &self.index {
            result.index.push(os::CompositeIndex {
                app_id: app.clone().into(),
                id: 0,
                definition: index_definition(index),
                state: os::composite_index::State::ReadWrite as i32,
                ..Default::default()
            });
        }
        if compile {
            let compiled = |p: &Position| {
                v3::CompiledCursor::decode(cursor::encode(p).as_slice()).unwrap_or_default()
            };
            let last = rows.last().map(|r| &r.cursor).or(skipped_cursor.as_ref());
            result.compiled_cursor = Some(compiled(last.unwrap_or(&Position::default())));
            result.result_compiled_cursor = rows.iter().map(|r| compiled(&r.cursor)).collect();
            result.skipped_results_compiled_cursor = skipped_cursor.as_ref().map(compiled);
        }
        Ok(Batch { result, more })
    }
}

/// The compiled query Java reports: the query itself (with project ids) as the primary scan.
pub fn compiled_query(q: &v3::Query) -> v3::CompiledQuery {
    v3::CompiledQuery {
        primaryscan: v3::compiled_query::PrimaryScan {
            index_name: Some(q.encode_to_vec().into()),
            ..Default::default()
        },
        keys_only: false,
        ..Default::default()
    }
}

pub fn index_definition(index: &CompositeIndex) -> os::Index {
    use os::index::property::Direction as D;
    os::Index {
        entity_type: index.kind.clone().into(),
        ancestor: index.ancestor,
        property: index
            .properties
            .iter()
            .map(|p| os::index::Property {
                name: p.name.clone().into(),
                direction: p.direction.map(|d| match d {
                    Direction::Ascending => D::Ascending as i32,
                    Direction::Descending => D::Descending as i32,
                }),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// A projected index value as a v3 property value.
pub fn property_value(
    v: &IndexValue,
    reference: impl Fn(&oxide_model::Key) -> os::Reference,
) -> os::PropertyValue {
    let mut out = os::PropertyValue::default();
    match v {
        IndexValue::Null => {}
        IndexValue::Int(i) => out.int64_value = Some(*i),
        IndexValue::Bool(b) => out.boolean_value = Some(*b),
        IndexValue::Bytes(b) => out.string_value = Some(b.clone().into()),
        IndexValue::Double(d) => out.double_value = Some(*d),
        IndexValue::Point {
            latitude,
            longitude,
        } => {
            out.pointvalue = Some(os::property_value::PointValue {
                x: *latitude,
                y: *longitude,
            })
        }
        IndexValue::Key(k) => {
            let r = reference(k);
            out.referencevalue = Some(os::property_value::ReferenceValue {
                app: r.app,
                pathelement: r
                    .path
                    .element
                    .into_iter()
                    .map(|e| os::property_value::reference_value::PathElement {
                        r#type: e.r#type,
                        id: e.id,
                        name: e.name,
                    })
                    .collect(),
                name_space: r.name_space,
                database_id: r.database_id,
            });
        }
    }
    out
}
